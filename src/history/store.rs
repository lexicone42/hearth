//! The time-series store: every observation hearth has ever seen, on disk.
//!
//! Backed by [redb] — a pure-Rust, ACID, single-file embedded key-value store.
//! One table:
//!
//! ```text
//! observations : (entity: &str, ts_ms: i64) -> encoded Value
//! ```
//!
//! redb orders tuple keys by component, so every point for one entity is
//! **contiguous** in the B-tree and a time range is one sequential scan — the
//! exact access pattern a chart needs, with no secondary index.
//!
//! ## Why not record every poll
//!
//! Polling ~80 entities each minute is ~42M points a year, nearly all of them
//! repeats of the value before. [`HistoryStore::record`] instead writes a point
//! only when the value **changed**, or when the last stored point is older than
//! `heartbeat`. The heartbeat is what keeps that honest: without it, "no points
//! for six hours" would be ambiguous between *steady* and *hearth was down*.
//! With it, a gap longer than the heartbeat means genuinely no data.
//!
//! ## Deliberate simplicity
//!
//! Keys store the entity id as a string rather than an interned integer. That
//! costs disk (the id repeats on every row) and buys something worth more here:
//! the database is self-describing and salvageable — there is no id map that,
//! if lost, turns years of history into anonymous numbers.
//!
//! [redb]: https://github.com/cberner/redb

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use tracing::warn;

use crate::domain::{Observation, Value};
use crate::history::codec;

/// `(entity id, epoch ms) -> encoded value`.
const OBSERVATIONS: TableDefinition<(&str, i64), &[u8]> = TableDefinition::new("observations");

/// `(source, ts_ms, id) -> JSON blob` — discrete events, time-ordered per source.
///
/// The second half of the data model. An *observation* is one scalar sample of
/// one channel; an *event* is a thing that happened carrying several correlated
/// fields (a bird detection: species, confidence, score, clip; a litter-box
/// visit: cat, weight, waste, duration). Splitting an event into independent
/// observations throws away the correlation that makes it useful, so events get
/// their own table — in the same database, so one file, one backup, one snapshot
/// covers everything.
const EVENTS: TableDefinition<(&str, i64, &str), &[u8]> = TableDefinition::new("events");

/// `(source, id) -> ts_ms` — the dedup index.
///
/// The main table is keyed for *time* scans, which makes "have I already stored
/// event X?" a scan rather than a lookup. This index answers it in `O(log n)`,
/// so re-fetching an overlapping window (which every polling source does) is
/// cheap and idempotent no matter how large the archive grows.
const EVENT_IDS: TableDefinition<(&str, &str), i64> = TableDefinition::new("event_ids");

/// One stored event: when it happened, its source-assigned id, and its payload.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub ts_ms: i64,
    pub id: String,
    pub blob: Vec<u8>,
}

/// One entity present in the history, and the span it covers.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EntitySpan {
    pub entity: String,
    pub first_ms: i64,
    pub last_ms: i64,
}

/// A read of one entity's history, already reduced to chart size.
pub struct Series {
    /// Chronological points, at most the caller's `max`.
    pub points: Vec<(i64, Value)>,
    /// How many stored points the range actually held, before reduction.
    pub total: usize,
    /// Whether reduction happened — so a client can say so rather than imply
    /// it is looking at raw data.
    pub downsampled: bool,
}

/// Accumulator for one downsampling bucket. Numbers average; flags and text
/// keep their last value, having no meaningful mean.
struct Bucket {
    ts_sum: i128,
    n: i64,
    sum: f64,
    unit: Option<crate::domain::Unit>,
    last: Value,
}

impl Bucket {
    fn new(at: i64, v: Value) -> Self {
        let mut b = Bucket {
            ts_sum: 0,
            n: 0,
            sum: 0.0,
            unit: None,
            last: v.clone(),
        };
        b.push(at, v);
        b
    }
    fn push(&mut self, at: i64, v: Value) {
        self.ts_sum += at as i128;
        self.n += 1;
        match &v {
            Value::Quantity { value, unit } => {
                self.sum += value;
                self.unit = Some(*unit);
            }
            Value::Count(c) => self.sum += *c as f64,
            _ => {}
        }
        self.last = v;
    }
    fn finish(self) -> (i64, Value) {
        let ts = (self.ts_sum / self.n as i128) as i64;
        let mean = self.sum / self.n as f64;
        let value = match (&self.last, self.unit) {
            (Value::Quantity { .. }, Some(unit)) => Value::Quantity { value: mean, unit },
            (Value::Count(_), _) => Value::Count(mean.round() as i64),
            _ => self.last,
        };
        (ts, value)
    }
}

/// What the last write for an entity was, so we can skip unchanged values.
#[derive(Debug, Clone)]
struct Last {
    at_ms: i64,
    value: Value,
}

/// A handle on the on-disk history. Cheap to share behind an `Arc`; redb does
/// its own locking, and the change-detection map has its own mutex.
pub struct HistoryStore {
    db: Database,
    last: Mutex<HashMap<String, Last>>,
    heartbeat_ms: i64,
}

impl HistoryStore {
    /// Open (or create) the database at `path`, writing a point at most every
    /// `heartbeat` even when a value never changes.
    pub fn open(path: impl AsRef<Path>, heartbeat: std::time::Duration) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating history dir {}", parent.display()))?;
        }
        let db = Database::create(path)
            .with_context(|| format!("opening history database {}", path.display()))?;
        // Create the table on first open so reads before any write succeed.
        let txn = db.begin_write().context("opening history write txn")?;
        txn.open_table(OBSERVATIONS)
            .context("creating observations table")?;
        txn.commit().context("committing history schema")?;

        Ok(Self {
            db,
            last: Mutex::new(HashMap::new()),
            heartbeat_ms: heartbeat.as_millis() as i64,
        })
    }

    /// Record a batch, skipping values that haven't changed since the last
    /// stored point (unless the heartbeat has elapsed). Returns how many points
    /// were actually written.
    ///
    /// `now_ms` is the wall clock; an observation's own `observed_at` is
    /// preferred when the source supplied one, so a point is stamped when it was
    /// measured rather than when we happened to poll.
    pub fn record(&self, batch: &[Observation], now_ms: i64) -> Result<usize> {
        // Decide what to write while holding only the change-map lock, so the
        // (slower) database write isn't serialized behind it any longer than
        // needed.
        let due: Vec<(String, i64, Value)> = {
            let mut last = self.last.lock().expect("history change-map poisoned");
            batch
                .iter()
                .filter_map(|obs| {
                    let entity = obs.entity.as_str();
                    let at = obs.observed_at.unwrap_or(now_ms);
                    match last.get(entity) {
                        Some(prev)
                            if prev.value == obs.value && at - prev.at_ms < self.heartbeat_ms =>
                        {
                            None // unchanged, and the heartbeat hasn't elapsed
                        }
                        // Never move a value's timestamp backwards: an out-of-order
                        // or replayed observation must not rewrite history.
                        Some(prev) if at <= prev.at_ms => None,
                        _ => {
                            last.insert(
                                entity.to_string(),
                                Last {
                                    at_ms: at,
                                    value: obs.value.clone(),
                                },
                            );
                            Some((entity.to_string(), at, obs.value.clone()))
                        }
                    }
                })
                .collect()
        };
        if due.is_empty() {
            return Ok(0);
        }

        let txn = self.db.begin_write().context("opening history write txn")?;
        {
            let mut table = txn
                .open_table(OBSERVATIONS)
                .context("opening observations table")?;
            for (entity, at, value) in &due {
                table
                    .insert((entity.as_str(), *at), codec::encode(value).as_slice())
                    .with_context(|| format!("writing history point for {entity}"))?;
            }
        }
        txn.commit().context("committing history points")?;
        Ok(due.len())
    }

    /// Every stored point for `entity` in `[from_ms, to_ms]`, oldest first.
    /// Undecodable rows are skipped with a warning rather than failing the read.
    ///
    /// Exercised by the tests; the HTTP read path (`GET /api/series`) lands in
    /// the next change, which is why the binary doesn't call it yet.
    #[allow(dead_code)]
    pub fn range(&self, entity: &str, from_ms: i64, to_ms: i64) -> Result<Vec<(i64, Value)>> {
        let txn = self.db.begin_read().context("opening history read txn")?;
        let table = txn
            .open_table(OBSERVATIONS)
            .context("opening observations table")?;
        let mut out = Vec::new();
        // Keys sort by (entity, ts), so one entity's points are contiguous.
        for row in table
            .range((entity, from_ms)..=(entity, to_ms))
            .context("scanning history range")?
        {
            let (k, v) = row.context("reading history row")?;
            let (row_entity, at) = k.value();
            if row_entity != entity {
                break; // walked past this entity
            }
            match codec::decode(v.value()) {
                Some(value) => out.push((at, value)),
                None => warn!(entity, at, "skipping undecodable history point"),
            }
        }
        Ok(out)
    }

    /// Append events for `source`, skipping any whose id is already stored.
    /// Returns how many were new — so a source can re-fetch an overlapping
    /// window every poll and this stays idempotent.
    pub fn record_events(&self, source: &str, events: &[StoredEvent]) -> Result<usize> {
        if events.is_empty() {
            return Ok(0);
        }
        let txn = self.db.begin_write().context("opening events write txn")?;
        let mut written = 0usize;
        {
            let mut ids = txn.open_table(EVENT_IDS).context("opening event index")?;
            let mut tbl = txn.open_table(EVENTS).context("opening events table")?;
            for e in events {
                if ids
                    .get((source, e.id.as_str()))
                    .context("checking event index")?
                    .is_some()
                {
                    continue; // already have it
                }
                tbl.insert((source, e.ts_ms, e.id.as_str()), e.blob.as_slice())
                    .context("writing event")?;
                ids.insert((source, e.id.as_str()), e.ts_ms)
                    .context("indexing event")?;
                written += 1;
            }
        }
        txn.commit().context("committing events")?;
        Ok(written)
    }

    /// Events for `source` in `[from_ms, to_ms]`, newest first, capped at `limit`.
    /// Newest-first because every consumer of an event log wants the recent end.
    pub fn events(
        &self,
        source: &str,
        from_ms: i64,
        to_ms: i64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let txn = self.db.begin_read().context("opening events read txn")?;
        let table = match txn.open_table(EVENTS) {
            Ok(t) => t,
            // Nothing has ever been written; an empty log is not an error.
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        let mut it = table
            .range((source, from_ms, "")..=(source, to_ms, "\u{10FFFF}"))
            .context("scanning events")?;
        while out.len() < limit {
            let Some(row) = it.next_back() else { break };
            let (k, v) = row.context("reading event row")?;
            let (row_source, ts, id) = k.value();
            if row_source != source {
                break;
            }
            out.push(StoredEvent {
                ts_ms: ts,
                id: id.to_string(),
                blob: v.value().to_vec(),
            });
        }
        Ok(out)
    }

    /// `(events stored, newest ts)` for one source — a source uses the timestamp
    /// as a watermark so it only asks upstream for what it hasn't seen.
    pub fn event_stats(&self, source: &str) -> Result<(usize, Option<i64>)> {
        let txn = self.db.begin_read().context("opening events read txn")?;
        let Ok(table) = txn.open_table(EVENTS) else {
            return Ok((0, None));
        };
        let mut n = 0usize;
        let mut newest = None;
        for row in table
            .range((source, i64::MIN, "")..=(source, i64::MAX, "\u{10FFFF}"))
            .context("scanning events")?
        {
            let (k, _) = row.context("reading event row")?;
            let (row_source, ts, _) = k.value();
            if row_source != source {
                break;
            }
            n += 1;
            newest = Some(ts);
        }
        Ok((n, newest))
    }

    /// Every entity that has history, with the span it covers.
    ///
    /// Deliberately **not** a full scan. Keys sort by `(entity, ts)`, so after
    /// reading one entity's first key we seek straight past its whole range
    /// rather than walking its points — `O(entities · log n)` instead of
    /// `O(points)`. At two years' retention that's the difference between a
    /// millisecond and reading millions of rows on every page load.
    pub fn entities(&self) -> Result<Vec<EntitySpan>> {
        let txn = self.db.begin_read().context("opening history read txn")?;
        let table = txn
            .open_table(OBSERVATIONS)
            .context("opening observations table")?;

        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            // First key at or after the cursor: the next entity's earliest point.
            let (entity, first_ms) = {
                let mut it = table
                    .range((cursor.as_str(), i64::MIN)..)
                    .context("seeking next entity")?;
                match it.next() {
                    Some(row) => {
                        let (k, _) = row.context("reading history row")?;
                        let (e, t) = k.value();
                        (e.to_string(), t)
                    }
                    None => break,
                }
            };
            // That entity's latest point, from the other end of its own range.
            let last_ms = {
                let mut it = table
                    .range((entity.as_str(), i64::MIN)..=(entity.as_str(), i64::MAX))
                    .context("seeking entity end")?;
                match it.next_back() {
                    Some(row) => row.context("reading history row")?.0.value().1,
                    None => first_ms,
                }
            };
            out.push(EntitySpan {
                entity: entity.clone(),
                first_ms,
                last_ms,
            });
            // `\0` sorts above every printable char, so this lands on the next
            // entity without walking this one's points.
            cursor = entity + "\0";
        }
        Ok(out)
    }

    /// Points for `entity` in `[from_ms, to_ms]`, reduced to at most `max`.
    ///
    /// Downsamples while streaming — a chart wants a few hundred points, and a
    /// two-year range holds far more than a fridge should ever have to parse.
    /// Time is split into `max` buckets and each is averaged (numbers) or takes
    /// its last value (flags and text, which have no meaningful mean), stamped
    /// with the mean time of the points in it. A bucket holding a single point
    /// therefore reproduces that point exactly, so a short range comes back
    /// untouched. Returns `(points, total_in_range, downsampled)`.
    pub fn series(&self, entity: &str, from_ms: i64, to_ms: i64, max: usize) -> Result<Series> {
        let max = max.clamp(1, 5_000);
        let txn = self.db.begin_read().context("opening history read txn")?;
        let table = txn
            .open_table(OBSERVATIONS)
            .context("opening observations table")?;

        let span = (to_ms - from_ms).max(1);
        let mut buckets: Vec<Option<Bucket>> = (0..max).map(|_| None).collect();
        let mut total = 0usize;

        for row in table
            .range((entity, from_ms)..=(entity, to_ms))
            .context("scanning history range")?
        {
            let (k, v) = row.context("reading history row")?;
            let (row_entity, at) = k.value();
            if row_entity != entity {
                break;
            }
            let Some(value) = codec::decode(v.value()) else {
                warn!(entity, at, "skipping undecodable history point");
                continue;
            };
            total += 1;
            let idx = (((at - from_ms) as i128 * max as i128) / span as i128)
                .clamp(0, max as i128 - 1) as usize;
            match &mut buckets[idx] {
                Some(b) => b.push(at, value),
                slot @ None => *slot = Some(Bucket::new(at, value)),
            }
        }

        let points: Vec<(i64, Value)> = buckets.into_iter().flatten().map(Bucket::finish).collect();
        let downsampled = total > points.len();
        Ok(Series {
            points,
            total,
            downsampled,
        })
    }

    /// Drop every point older than `cutoff_ms`. Returns how many were removed.
    pub fn prune(&self, cutoff_ms: i64) -> Result<usize> {
        // Collect first, then delete: mutating while iterating a redb table
        // isn't allowed, and retention runs rarely enough that the extra pass
        // is irrelevant.
        let doomed: Vec<(String, i64)> = {
            let txn = self.db.begin_read().context("opening history read txn")?;
            let table = txn
                .open_table(OBSERVATIONS)
                .context("opening observations table")?;
            table
                .iter()
                .context("scanning history for retention")?
                .filter_map(|row| row.ok())
                .filter_map(|(k, _)| {
                    let (entity, at) = k.value();
                    (at < cutoff_ms).then(|| (entity.to_string(), at))
                })
                .collect()
        };
        if doomed.is_empty() {
            return Ok(0);
        }
        let txn = self.db.begin_write().context("opening history write txn")?;
        {
            let mut table = txn
                .open_table(OBSERVATIONS)
                .context("opening observations table")?;
            for (entity, at) in &doomed {
                table
                    .remove((entity.as_str(), *at))
                    .context("removing expired history point")?;
            }
        }
        txn.commit().context("committing history retention")?;
        Ok(doomed.len())
    }

    /// Write a consistent point-in-time copy of the history to `dest`.
    ///
    /// A redb file **cannot** be backed up with `cp`: it is copy-on-write and
    /// holds an exclusive lock, so a byte copy taken while hearth is running can
    /// catch a torn state, and a second process can't even open it to try. The
    /// backup therefore has to come from inside, and this is it.
    ///
    /// The copy is *logical*: a read transaction gives an MVCC snapshot that
    /// writers don't block and can't disturb, and every row in it is written
    /// into a brand-new database. The result is a valid, compact redb file that
    /// can simply be moved into place to restore. Returns the points copied.
    ///
    /// Cost is O(size) — fine while the database is small, and worth revisiting
    /// (incremental export) if it ever isn't.
    pub fn snapshot(&self, dest: &Path) -> Result<usize> {
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating snapshot dir {}", parent.display()))?;
        }
        // Build into a temp file, then rename: a crash mid-snapshot must never
        // leave a half-written file where a good backup used to be.
        let tmp = dest.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);

        let read = self.db.begin_read().context("opening history read txn")?;
        let source = read
            .open_table(OBSERVATIONS)
            .context("opening observations table")?;

        let mut copied = 0usize;
        {
            let out = Database::create(&tmp)
                .with_context(|| format!("creating snapshot {}", tmp.display()))?;
            let txn = out.begin_write().context("opening snapshot write txn")?;
            {
                let mut table = txn
                    .open_table(OBSERVATIONS)
                    .context("creating snapshot table")?;
                for row in source.iter().context("scanning history for snapshot")? {
                    let (k, v) = row.context("reading history row")?;
                    let (entity, at) = k.value();
                    table
                        .insert((entity, at), v.value())
                        .context("writing snapshot row")?;
                    copied += 1;
                }
            }
            txn.commit().context("committing snapshot")?;
        } // drop the snapshot db, releasing its lock before the rename

        crate::whisker::history::restrict_to_owner(&tmp)?;
        std::fs::rename(&tmp, dest)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), dest.display()))?;
        Ok(copied)
    }

    /// `(distinct entities, total points)` — for startup and retention logging.
    pub fn stats(&self) -> Result<(usize, usize)> {
        let txn = self.db.begin_read().context("opening history read txn")?;
        let table = txn
            .open_table(OBSERVATIONS)
            .context("opening observations table")?;
        let mut entities = std::collections::HashSet::new();
        let mut points = 0usize;
        for row in table.iter().context("scanning history")? {
            let (k, _) = row.context("reading history row")?;
            entities.insert(k.value().0.to_string());
            points += 1;
        }
        Ok((entities.len(), points))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeviceClass, EntityId, Unit};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "hearth-hist-{}-{tag}-{n}/history.redb",
            std::process::id()
        ))
    }

    fn obs(entity: &str, v: Value) -> Observation {
        Observation::new(
            EntityId::new(entity.split('.')),
            DeviceClass::Temperature,
            v,
            None,
        )
    }

    fn temp(v: f64) -> Value {
        Value::quantity(v, Unit::Fahrenheit)
    }

    #[test]
    fn records_only_changes_then_honors_the_heartbeat() {
        let path = temp_db("changes");
        let store = HistoryStore::open(&path, Duration::from_secs(900)).unwrap();
        let e = "ambient_weather.outdoor.temperature";

        // First sighting always lands — it establishes the baseline.
        assert_eq!(store.record(&[obs(e, temp(70.0))], 1_000).unwrap(), 1);
        // Same value a minute later: skipped.
        assert_eq!(store.record(&[obs(e, temp(70.0))], 61_000).unwrap(), 0);
        // Changed value: recorded.
        assert_eq!(store.record(&[obs(e, temp(71.0))], 121_000).unwrap(), 1);
        // Unchanged, but past the 900s heartbeat: recorded, so a long flat
        // stretch is distinguishable from hearth being down.
        assert_eq!(store.record(&[obs(e, temp(71.0))], 1_100_000).unwrap(), 1);

        let points = store.range(e, 0, i64::MAX).unwrap();
        assert_eq!(points.len(), 3);
        assert_eq!(points[0], (1_000, temp(70.0)));
        assert_eq!(points[1], (121_000, temp(71.0)));
        assert_eq!(points[2], (1_100_000, temp(71.0)));
        // Oldest first.
        assert!(points.windows(2).all(|w| w[0].0 < w[1].0));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn range_is_bounded_and_per_entity() {
        let path = temp_db("range");
        let store = HistoryStore::open(&path, Duration::from_secs(900)).unwrap();
        let a = "ambient_weather.outdoor.temperature";
        let b = "ambient_weather.indoor.temperature";
        for (i, v) in [70.0, 71.0, 72.0, 73.0].iter().enumerate() {
            let t = 1_000 + i as i64 * 60_000;
            store
                .record(&[obs(a, temp(*v)), obs(b, temp(v + 10.0))], t)
                .unwrap();
        }

        // Entity isolation: `a`'s scan must not bleed into `b`'s keys.
        let all_a = store.range(a, 0, i64::MAX).unwrap();
        assert_eq!(all_a.len(), 4);
        assert!(all_a.iter().all(|(_, v)| *v != temp(80.0)));
        assert_eq!(store.range(b, 0, i64::MAX).unwrap().len(), 4);

        // Bounds are inclusive on both ends.
        let mid = store.range(a, 61_000, 121_000).unwrap();
        assert_eq!(mid.len(), 2);
        assert_eq!(mid[0].0, 61_000);
        assert_eq!(mid[1].0, 121_000);

        assert_eq!(store.range("nope.not.here", 0, i64::MAX).unwrap().len(), 0);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn prune_drops_only_old_points() {
        let path = temp_db("prune");
        let store = HistoryStore::open(&path, Duration::from_secs(1)).unwrap();
        let e = "ambient_weather.outdoor.temperature";
        for i in 0..5 {
            store
                .record(&[obs(e, temp(70.0 + i as f64))], 1_000 + i * 60_000)
                .unwrap();
        }
        assert_eq!(store.stats().unwrap(), (1, 5));

        assert_eq!(store.prune(121_000).unwrap(), 2); // t=1_000 and t=61_000
        let left = store.range(e, 0, i64::MAX).unwrap();
        assert_eq!(left.len(), 3);
        assert!(left.iter().all(|(t, _)| *t >= 121_000));
        assert_eq!(store.prune(0).unwrap(), 0); // nothing older than the epoch

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn survives_reopen_and_ignores_out_of_order_replays() {
        let path = temp_db("reopen");
        let e = "ambient_weather.outdoor.temperature";
        {
            let store = HistoryStore::open(&path, Duration::from_secs(900)).unwrap();
            store.record(&[obs(e, temp(70.0))], 60_000).unwrap();
            store.record(&[obs(e, temp(71.0))], 120_000).unwrap();
        }
        // Reopening must see the committed points (ACID, not a cache).
        let store = HistoryStore::open(&path, Duration::from_secs(900)).unwrap();
        assert_eq!(store.range(e, 0, i64::MAX).unwrap().len(), 2);

        // A replayed/older observation must not rewrite history. (The in-memory
        // change map is empty after reopen, so this also covers the cold path.)
        store.record(&[obs(e, temp(99.0))], 120_000).unwrap();
        store.record(&[obs(e, temp(98.0))], 30_000).unwrap();
        let points = store.range(e, 0, i64::MAX).unwrap();
        assert!(
            points.iter().all(|(_, v)| *v != temp(98.0)),
            "an older timestamp must never be written after a newer one"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn entities_lists_every_entity_with_its_span() {
        let path = temp_db("entities");
        let store = HistoryStore::open(&path, Duration::from_secs(1)).unwrap();
        // Names chosen so one is a strict PREFIX of another: the seek-skip must
        // not step over `...temp` when jumping past `...tempera`.
        let a = "ambient_weather.outdoor.temp";
        let b = "ambient_weather.outdoor.temperature";
        let c = "dyson.living.pm25";
        store.record(&[obs(a, temp(1.0))], 1_000).unwrap();
        store.record(&[obs(a, temp(2.0))], 5_000).unwrap();
        store.record(&[obs(b, temp(3.0))], 2_000).unwrap();
        store.record(&[obs(c, temp(4.0))], 3_000).unwrap();

        let es = store.entities().unwrap();
        let names: Vec<&str> = es.iter().map(|e| e.entity.as_str()).collect();
        assert_eq!(
            names,
            vec![a, b, c],
            "sorted, one row per entity, none skipped"
        );
        assert_eq!((es[0].first_ms, es[0].last_ms), (1_000, 5_000));
        assert_eq!((es[1].first_ms, es[1].last_ms), (2_000, 2_000));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn series_returns_exact_points_until_it_must_downsample() {
        let path = temp_db("series");
        let store = HistoryStore::open(&path, Duration::from_secs(1)).unwrap();
        let e = "ambient_weather.outdoor.temperature";
        for i in 0..100i64 {
            store
                .record(&[obs(e, temp(i as f64))], 1_000 + i * 1_000)
                .unwrap();
        }

        // Room to spare: every point survives untouched.
        let s = store.series(e, 0, 200_000, 500).unwrap();
        let (pts, total, down) = (s.points, s.total, s.downsampled);
        assert_eq!(total, 100);
        assert_eq!(pts.len(), 100);
        assert!(!down);
        assert_eq!(pts[0].1, temp(0.0));
        assert_eq!(pts[99].1, temp(99.0));

        // Squeezed: fewer points, flagged, and still spanning the same range.
        let s = store.series(e, 0, 200_000, 10).unwrap();
        let (pts, total, down) = (s.points, s.total, s.downsampled);
        assert_eq!(total, 100);
        assert!(down && pts.len() <= 10 && !pts.is_empty());
        assert!(
            pts.windows(2).all(|w| w[0].0 < w[1].0),
            "still chronological"
        );
        // Buckets average, so the reduced series stays inside the original range.
        for (_, v) in &pts {
            if let Value::Quantity { value, .. } = v {
                assert!(
                    (0.0..=99.0).contains(value),
                    "bucket mean {value} out of range"
                );
            }
        }

        // A window with nothing in it is empty, not an error.
        let s = store.series(e, 900_000, 950_000, 100).unwrap();
        let (pts, total) = (s.points, s.total);
        assert!(pts.is_empty() && total == 0);

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn observed_at_wins_over_wall_clock() {
        let path = temp_db("observed");
        let store = HistoryStore::open(&path, Duration::from_secs(900)).unwrap();
        let e = "ambient_weather.outdoor.temperature";
        let mut o = obs(e, temp(70.0));
        o.observed_at = Some(500_000); // the source measured it here...
        store.record(&[o], 999_999).unwrap(); // ...even though we polled later
        assert_eq!(store.range(e, 0, i64::MAX).unwrap()[0].0, 500_000);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
