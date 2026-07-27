//! Dated off-disk backups of the history database.
//!
//! Mirrors [`crate::whisker::backup`] in behaviour — one dated copy per UTC day,
//! atomic, owner-only, oldest pruned past `keep`, and a warning when the backup
//! lands on the database's own device. It differs in *how* the copy is made:
//! the visit archive is a plain file that can be read and rewritten, while a
//! redb database is copy-on-write and exclusively locked, so its backup must be
//! a consistent logical snapshot taken from inside the running process (see
//! [`HistoryStore::snapshot`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::clock;
use crate::history::HistoryStore;

/// What a successful backup did.
#[derive(Debug, Clone, PartialEq)]
pub struct BackupOutcome {
    /// The snapshot written.
    pub path: PathBuf,
    /// Points copied into it.
    pub points: usize,
    /// Old snapshots removed by retention.
    pub pruned: usize,
}

/// Snapshot the history into `backup_dir`, at most once per UTC day. Returns
/// `Ok(None)` when today's snapshot already exists, so this is safe to call on
/// every maintenance tick. `keep` is how many to retain; `0` keeps everything.
pub fn daily(
    store: &HistoryStore,
    db_path: &Path,
    backup_dir: &Path,
    keep: usize,
) -> Result<Option<BackupOutcome>> {
    std::fs::create_dir_all(backup_dir)
        .with_context(|| format!("creating backup dir {}", backup_dir.display()))?;
    warn_if_same_device(db_path, backup_dir);

    let name = format!("history-{}.redb", clock::today_utc());
    let dest = backup_dir.join(&name);
    if dest.exists() {
        return Ok(None); // already snapshotted today
    }

    let points = store.snapshot(&dest)?;
    let pruned = prune(backup_dir, keep)?;
    Ok(Some(BackupOutcome {
        path: dest,
        points,
        pruned,
    }))
}

/// Keep the newest `keep` dated snapshots. `history-YYYY-MM-DD.redb` sorts
/// lexicographically in chronological order. `keep == 0` disables pruning.
fn prune(backup_dir: &Path, keep: usize) -> Result<usize> {
    if keep == 0 {
        return Ok(0);
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(backup_dir)
        .with_context(|| format!("listing backup dir {}", backup_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("history-") && n.ends_with(".redb"))
        })
        .collect();
    if found.len() <= keep {
        return Ok(0);
    }
    found.sort();
    let doomed = found.len() - keep;
    let mut pruned = 0;
    for p in found.into_iter().take(doomed) {
        match std::fs::remove_file(&p) {
            Ok(()) => pruned += 1,
            Err(e) => {
                warn!(path = %p.display(), error = %e, "could not prune old history snapshot")
            }
        }
    }
    Ok(pruned)
}

/// Warn when snapshots land on the database's own filesystem — that survives a
/// mistake but not the disk, which is the case that looks like protection while
/// providing none.
fn warn_if_same_device(db_path: &Path, backup_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (Ok(a), Ok(b)) = (std::fs::metadata(db_path), std::fs::metadata(backup_dir)) else {
            return;
        };
        if a.dev() == b.dev() {
            warn!(
                backup_dir = %backup_dir.display(),
                "history snapshots are on the SAME filesystem as the database — they won't survive a disk failure; point [history].backup_dir at another disk"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (db_path, backup_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeviceClass, EntityId, Observation, Unit, Value};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("hearth-hbk-{}-{tag}-{n}", std::process::id()))
    }

    fn obs(entity: &str, v: f64) -> Observation {
        Observation::new(
            EntityId::new(entity.split('.')),
            DeviceClass::Temperature,
            Value::quantity(v, Unit::Fahrenheit),
            None,
        )
    }

    #[test]
    fn snapshots_once_a_day_and_the_copy_is_readable() {
        let root = temp_dir("daily");
        let _ = std::fs::remove_dir_all(&root);
        let db_path = root.join("history.redb");
        let backup_dir = root.join("backups");
        let e = "ambient_weather.outdoor.temperature";

        let store = HistoryStore::open(&db_path, Duration::from_secs(900)).unwrap();
        for (i, v) in [70.0, 71.0, 72.0].iter().enumerate() {
            store
                .record(&[obs(e, *v)], 1_000 + i as i64 * 60_000)
                .unwrap();
        }

        let out = daily(&store, &db_path, &backup_dir, 14)
            .unwrap()
            .expect("first snapshot");
        assert_eq!(out.points, 3);
        assert_eq!(out.pruned, 0);
        assert!(out.path.exists());

        // Second call the same day is a no-op — safe on every tick.
        assert_eq!(daily(&store, &db_path, &backup_dir, 14).unwrap(), None);

        // The snapshot must be a real, openable database with the same points —
        // a backup you can't read is not a backup.
        drop(store);
        let restored = HistoryStore::open(&out.path, Duration::from_secs(900)).unwrap();
        let points = restored.range(e, 0, i64::MAX).unwrap();
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].1, Value::quantity(70.0, Unit::Fahrenheit));
        assert_eq!(points[2].1, Value::quantity(72.0, Unit::Fahrenheit));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn snapshot_of_an_empty_history_still_produces_a_valid_database() {
        let root = temp_dir("empty");
        let _ = std::fs::remove_dir_all(&root);
        let db_path = root.join("history.redb");
        let store = HistoryStore::open(&db_path, Duration::from_secs(900)).unwrap();

        let out = daily(&store, &db_path, &root.join("backups"), 14)
            .unwrap()
            .expect("snapshot");
        assert_eq!(out.points, 0);
        drop(store);
        assert!(HistoryStore::open(&out.path, Duration::from_secs(900)).is_ok());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn prune_keeps_the_newest_and_leaves_strays_alone() {
        let dir = temp_dir("prune");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for d in ["2026-01-01", "2026-01-02", "2026-01-03"] {
            std::fs::write(dir.join(format!("history-{d}.redb")), "").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "keep me").unwrap();

        assert_eq!(prune(&dir, 1).unwrap(), 2);
        assert!(dir.join("history-2026-01-03.redb").exists());
        assert!(dir.join("notes.txt").exists());
        assert_eq!(prune(&dir, 0).unwrap(), 0); // 0 disables pruning

        std::fs::remove_dir_all(&dir).ok();
    }
}
