//! Append-only weight-history archive for Litter-Robot PET_VISIT events.
//!
//! Whisker's cloud only retains ~30 days of activity history, so hearth persists
//! every PET_VISIT (a per-cat weight reading + the waste left) forever in a local
//! JSON-Lines file (`visits.jsonl`). The store is:
//!   - **append-only** — a new visit is one appended line; nothing is rewritten,
//!   - **idempotent** — de-duplicated by the event's `eventId`, so re-importing a
//!     saved snapshot or re-scanning the (overlapping) live feed never
//!     double-counts,
//!   - **owner-only** — it holds personal pet/health data, so the file is
//!     restricted to `0600` on unix (mirrors the token store in
//!     [`crate::smartthings::auth`]).
//!
//! Two writers feed it, both through [`VisitStore::append_new`]: the forward
//! archiving task (`run_whisker_history` in `main`) appends new visits each poll,
//! and the `whisker-history-import` subcommand banks a previously-saved 30-day
//! snapshot. The `eventId` set makes the two safe to overlap.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::whisker::model::Activity;

/// One archived Litter-Robot PET_VISIT: a cat's measured weight (lb) and the
/// waste it left, stamped with the event's id and time. Serialized as one JSON
/// object per line in `visits.jsonl`; `event_id` is the dedup key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisitRecord {
    /// The event's stable id (`eventId`) — the archive's dedup key.
    pub event_id: String,
    /// ISO-8601 event timestamp, verbatim from the feed.
    pub ts: String,
    /// The box's serial number.
    pub serial: String,
    /// The box's user-given name (falls back to the serial when unnamed).
    pub box_name: String,
    /// The visiting cat's Whisker pet id, when the feed attributed one.
    pub pet_id: Option<String>,
    /// The cat's name, resolved from the pet id at archive time (best-effort;
    /// `None` when it couldn't be resolved).
    pub cat: Option<String>,
    /// The measured weight, in pounds (`petWeight / 100`).
    pub weight_lb: f64,
    /// Waste type, e.g. "Urine" / "Feces", when reported.
    pub waste_type: Option<String>,
    /// Waste weight (grams, as the feed reports it), when reported.
    pub waste_weight: Option<f64>,
    /// Visit duration in seconds, when reported.
    pub duration_s: Option<i64>,
}

/// Project a PET_VISIT [`Activity`] into a [`VisitRecord`], stamping the cat's
/// name (the caller resolves it by pet id — best-effort, may be `None`). Returns
/// `None` for any non-PET_VISIT event, or a PET_VISIT missing either of the two
/// fields the archive is built on: a stable `eventId` (the dedup key) and a
/// `petWeight` (the reading). Pure and total — no I/O, no clock.
pub fn visit_from_activity(a: &Activity, cat_name: Option<&str>) -> Option<VisitRecord> {
    if a.r#type.as_deref() != Some("PET_VISIT") {
        return None;
    }
    // The dedup key and the reading are both required — without either there's
    // nothing worth archiving.
    let event_id = a.event_id.clone()?;
    let pet_weight = a.pet_weight?;

    let serial = a.serial.clone().unwrap_or_default();
    let box_name = a
        .robot_name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| serial.clone());

    Some(VisitRecord {
        event_id,
        ts: a.timestamp.clone().unwrap_or_default(),
        serial,
        box_name,
        pet_id: a.pet_ids.first().cloned(),
        cat: cat_name.map(str::to_string),
        // petWeight is pounds × 100.
        weight_lb: pet_weight / 100.0,
        waste_type: a.waste_type.clone(),
        waste_weight: a.waste_weight,
        duration_s: a.duration,
    })
}

/// Append-only, de-duplicated store of [`VisitRecord`]s backed by a JSON-Lines
/// file (`<dir>/visits.jsonl`). Holds the set of `event_id`s already on disk so
/// appends stay idempotent across restarts, overlapping live scans, and
/// re-imports.
pub struct VisitStore {
    path: PathBuf,
    seen: HashSet<String>,
}

impl VisitStore {
    /// The archive file name inside the configured directory.
    const FILE_NAME: &'static str = "visits.jsonl";

    /// Open (or initialize) the archive under `dir`, creating the directory tree
    /// if needed. An existing `visits.jsonl` is read once to seed the seen-set; a
    /// malformed line is logged (`warn!`) and skipped rather than failing — a
    /// partial write from a crash must never wedge the whole archive.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating whisker history dir {}", dir.display()))?;
        let path = dir.join(Self::FILE_NAME);

        let mut seen = HashSet::new();
        if path.exists() {
            let file = std::fs::File::open(&path)
                .with_context(|| format!("opening visit archive {}", path.display()))?;
            for (i, line) in BufReader::new(file).lines().enumerate() {
                let line =
                    line.with_context(|| format!("reading visit archive {}", path.display()))?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<VisitRecord>(&line) {
                    Ok(rec) => {
                        seen.insert(rec.event_id);
                    }
                    Err(e) => warn!(
                        line = i + 1,
                        error = %e,
                        "skipping malformed line in visit archive"
                    ),
                }
            }
        }
        Ok(Self { path, seen })
    }

    /// Append every record whose `event_id` hasn't been seen before, returning
    /// how many were newly written. Idempotent: re-appending already-archived
    /// events writes nothing and returns 0. After writing, the file is restricted
    /// to owner-only (`0600`) on unix — it holds personal pet/health data.
    pub fn append_new(&mut self, records: impl IntoIterator<Item = VisitRecord>) -> Result<usize> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening visit archive {} for append", self.path.display()))?;

        let mut written = 0usize;
        for rec in records {
            if self.seen.contains(&rec.event_id) {
                continue;
            }
            let line = serde_json::to_string(&rec).context("serializing visit record")?;
            file.write_all(line.as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .with_context(|| format!("appending to visit archive {}", self.path.display()))?;
            self.seen.insert(rec.event_id);
            written += 1;
        }

        // Personal pet/health data — keep it owner-only even if the file already
        // existed with a looser umask (mirrors the token store).
        restrict_to_owner(&self.path)?;
        Ok(written)
    }

    /// The number of distinct events archived (the size of the seen-set).
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether the archive holds no events yet. Part of the store's API (and the
    /// `len`/`is_empty` pair clippy expects); currently only exercised in tests.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Restrict a file to owner read/write only (`0600`) on unix; a no-op elsewhere.
/// The archive holds personal pet/health data, so it's tightened after every
/// append even if the file already existed with a looser umask — mirrors
/// `restrict_to_owner` in [`crate::smartthings::auth`].
fn restrict_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic fixtures only — the repo is public, so no real serials, pet ids,
    // or names appear here.

    fn activity(json: serde_json::Value) -> Activity {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn maps_a_pet_visit_and_resolves_the_cat() {
        let a = activity(serde_json::json!({
            "eventId": "EV-TEST-1",
            "serial": "LR5-TEST-000000",
            "robotName": "test room",
            "type": "PET_VISIT",
            "timestamp": "2026-01-01T00:00:00Z",
            "duration": 65,
            "wasteType": "Urine",
            "petIds": ["PET-TEST-1"],
            "petWeight": 943.0,
            "wasteWeight": 48.0,
            "isReassigned": false
        }));
        let rec = visit_from_activity(&a, Some("Fixture One")).expect("record");
        assert_eq!(rec.event_id, "EV-TEST-1");
        assert_eq!(rec.serial, "LR5-TEST-000000");
        assert_eq!(rec.box_name, "test room");
        assert_eq!(rec.pet_id.as_deref(), Some("PET-TEST-1"));
        assert_eq!(rec.cat.as_deref(), Some("Fixture One"));
        // petWeight 943 -> 9.43 lb.
        assert_eq!(rec.weight_lb, 9.43);
        assert_eq!(rec.waste_type.as_deref(), Some("Urine"));
        assert_eq!(rec.waste_weight, Some(48.0));
        assert_eq!(rec.duration_s, Some(65));
    }

    #[test]
    fn non_pet_visit_is_ignored() {
        let a = activity(serde_json::json!({
            "eventId": "EV-TEST-2",
            "type": "DRAWER_FULL",
            "petWeight": 943.0
        }));
        assert!(visit_from_activity(&a, None).is_none());
    }

    #[test]
    fn pet_visit_without_weight_is_ignored() {
        let a = activity(serde_json::json!({
            "eventId": "EV-TEST-3",
            "type": "PET_VISIT",
            "petIds": ["PET-TEST-1"]
        }));
        assert!(visit_from_activity(&a, Some("Fixture One")).is_none());
    }

    #[test]
    fn box_name_falls_back_to_serial_and_cat_may_be_absent() {
        let a = activity(serde_json::json!({
            "eventId": "EV-TEST-4",
            "serial": "LR5-TEST-000000",
            "type": "PET_VISIT",
            "petWeight": 700.0
        }));
        let rec = visit_from_activity(&a, None).expect("record");
        assert_eq!(rec.box_name, "LR5-TEST-000000");
        assert_eq!(rec.cat, None);
        assert_eq!(rec.pet_id, None);
        assert_eq!(rec.weight_lb, 7.0);
    }

    fn sample_records() -> Vec<VisitRecord> {
        (1..=3)
            .map(|i| VisitRecord {
                event_id: format!("EV-TEST-{i}"),
                ts: format!("2026-01-0{i}T00:00:00Z"),
                serial: "LR5-TEST-000000".to_string(),
                box_name: "test room".to_string(),
                pet_id: Some("PET-TEST-1".to_string()),
                cat: Some("Fixture One".to_string()),
                weight_lb: 9.4,
                waste_type: Some("Urine".to_string()),
                waste_weight: Some(48.0),
                duration_s: Some(60),
            })
            .collect()
    }

    /// A unique temp dir per test (tests run in one process, so `pid` alone
    /// would collide) — mirrors the token-store test's `temp_dir` approach.
    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("hearth-visits-{}-{tag}-{n}", std::process::id()))
    }

    #[test]
    fn store_round_trips_and_dedups() {
        let dir = unique_dir("dedup");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = VisitStore::open(&dir).unwrap();
        assert!(store.is_empty());

        // Append three: all new.
        let added = store.append_new(sample_records()).unwrap();
        assert_eq!(added, 3);
        assert_eq!(store.len(), 3);

        // Re-append the same three: idempotent, nothing written.
        let again = store.append_new(sample_records()).unwrap();
        assert_eq!(again, 0);
        assert_eq!(store.len(), 3);

        // The file has exactly three lines.
        let path = dir.join(VisitStore::FILE_NAME);
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 3);

        // A fresh open re-seeds all three from disk.
        let reopened = VisitStore::open(&dir).unwrap();
        assert_eq!(reopened.len(), 3);

        // Owner-only on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "visit archive must be owner-only");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_skips_malformed_lines() {
        let dir = unique_dir("malformed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(VisitStore::FILE_NAME);
        // One valid record, one garbage line, one blank line.
        let good = serde_json::to_string(&sample_records()[0]).unwrap();
        std::fs::write(&path, format!("{good}\nnot json at all\n\n")).unwrap();

        let store = VisitStore::open(&dir).unwrap();
        // Only the one valid line seeded the seen-set.
        assert_eq!(store.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
