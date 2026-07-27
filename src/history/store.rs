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
