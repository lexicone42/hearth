//! Off-disk backups of the weight-history archive.
//!
//! `data/whisker/visits.jsonl` is the ONLY copy of this household's cat health
//! history beyond ~30 days — Whisker's cloud has already forgotten the rest, so
//! a lost archive is lost permanently. This module keeps dated full copies of it
//! somewhere else.
//!
//! Deliberately simple, because a backup you can't restore isn't one:
//!   - **Full plain copies**, not deltas or archives — restoring is `cp`, and
//!     any copy is independently readable. The file is ~200 KB and grows by a
//!     few KB a day, so the space this "wastes" is irrelevant next to the
//!     ability to open a backup in any text editor.
//!   - **One per UTC day** (`visits-YYYY-MM-DD.jsonl`), written atomically
//!     (temp file → `fsync` → rename) so a crash mid-backup can never leave a
//!     truncated file where a good one used to be.
//!   - **Owner-only (0600)**, like the archive — it's personal pet/health data.
//!   - **Verified on write**: every line is parsed back as a [`VisitRecord`] and
//!     the count is logged, so a silently-corrupt backup is visible.
//!
//! It does NOT protect against losing the whole machine. Point `backup_dir` at a
//! different physical disk (or a mounted NAS) — [`daily`] warns if it lands on
//! the same device as the archive, which is the failure mode that quietly
//! provides false confidence.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::whisker::history::{VisitRecord, VisitStore, restrict_to_owner};

/// What a successful backup did.
#[derive(Debug, Clone, PartialEq)]
pub struct BackupOutcome {
    /// The file written.
    pub path: PathBuf,
    /// Valid visit records it contains (parsed back after writing).
    pub records: usize,
    /// Old backups removed by retention.
    pub pruned: usize,
}

/// Back the archive up into `backup_dir`, at most once per UTC day. Returns
/// `Ok(None)` when there's nothing to do — no archive yet, or today's backup
/// already exists — so this is safe to call on every poll tick.
///
/// `keep` is how many dated backups to retain (oldest pruned first); 0 means
/// keep everything.
pub fn daily(archive_dir: &Path, backup_dir: &Path, keep: usize) -> Result<Option<BackupOutcome>> {
    let src = archive_dir.join(VisitStore::FILE_NAME);
    if !src.exists() {
        return Ok(None); // nothing archived yet
    }
    std::fs::create_dir_all(backup_dir)
        .with_context(|| format!("creating backup dir {}", backup_dir.display()))?;
    warn_if_same_device(&src, backup_dir);

    let name = format!("visits-{}.jsonl", crate::clock::today_utc());
    let dst = backup_dir.join(&name);
    if dst.exists() {
        return Ok(None); // already backed up today
    }

    let body = std::fs::read(&src)
        .with_context(|| format!("reading archive {} for backup", src.display()))?;

    // Write to a temp file, flush it to disk, then rename into place. Rename is
    // atomic within a filesystem, so a reader never sees a half-written backup.
    let tmp = backup_dir.join(format!(".{name}.tmp"));
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(&body)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("flushing {}", tmp.display()))?;
    }
    restrict_to_owner(&tmp)?;
    std::fs::rename(&tmp, &dst)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), dst.display()))?;

    // Verify what actually landed on disk rather than trusting the write.
    let records = count_records(&dst)?;
    if records == 0 && !body.is_empty() {
        warn!(
            path = %dst.display(),
            "Whisker backup parsed 0 records from a non-empty archive — check the archive for corruption"
        );
    }

    let pruned = prune(backup_dir, keep)?;
    Ok(Some(BackupOutcome {
        path: dst,
        records,
        pruned,
    }))
}

/// Parse a backup back into records, counting the valid ones. A malformed line
/// is skipped (the archive tolerates them too), so this is a health signal
/// rather than a hard check.
fn count_records(path: &Path) -> Result<usize> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("re-reading backup {} to verify it", path.display()))?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| serde_json::from_str::<VisitRecord>(l).is_ok())
        .count())
}

/// Keep the newest `keep` dated backups, removing the rest. Names are
/// `visits-YYYY-MM-DD.jsonl`, so lexicographic order IS chronological order.
/// `keep == 0` disables pruning.
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
                .is_some_and(|n| n.starts_with("visits-") && n.ends_with(".jsonl"))
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
            // A backup we couldn't delete is not a reason to fail the backup.
            Err(e) => warn!(path = %p.display(), error = %e, "could not prune old Whisker backup"),
        }
    }
    Ok(pruned)
}

/// Warn when the backup lands on the same filesystem as the archive: it then
/// protects against fat-fingers but NOT against the disk failing, which is the
/// whole point. Best-effort — a metadata error just skips the check.
fn warn_if_same_device(src: &Path, backup_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (Ok(a), Ok(b)) = (std::fs::metadata(src), std::fs::metadata(backup_dir)) else {
            return;
        };
        if a.dev() == b.dev() {
            warn!(
                backup_dir = %backup_dir.display(),
                "Whisker backups are on the SAME filesystem as the archive — they won't survive a disk failure; point [whisker].backup_dir at another disk"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (src, backup_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn unique_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("hearth-backup-{}-{tag}-{n}", std::process::id()))
    }

    /// Synthetic archive lines only — the repo is public.
    fn seed_archive(dir: &Path, n: usize) {
        std::fs::create_dir_all(dir).unwrap();
        let mut body = String::new();
        for i in 0..n {
            body.push_str(&format!(
                r#"{{"event_id":"EV-TEST-{i}","ts":"2026-01-01T08:00:00Z","serial":"LR5-TEST-000000","box_name":"test room","pet_id":"PET-TEST-1","cat":"Fixture One","weight_lb":9.4,"waste_type":"Urine","waste_weight":48.0,"duration_s":61}}"#
            ));
            body.push('\n');
        }
        std::fs::write(dir.join(VisitStore::FILE_NAME), body).unwrap();
    }

    #[test]
    fn backs_up_once_a_day_and_verifies_contents() {
        let arch = unique_dir("arch");
        let back = unique_dir("back");
        let _ = std::fs::remove_dir_all(&arch);
        let _ = std::fs::remove_dir_all(&back);
        seed_archive(&arch, 3);

        let out = daily(&arch, &back, 14).unwrap().expect("first backup");
        assert_eq!(out.records, 3, "verified by re-parsing what landed on disk");
        assert_eq!(out.pruned, 0);
        assert!(out.path.exists());
        assert_eq!(
            std::fs::read(&out.path).unwrap(),
            std::fs::read(arch.join(VisitStore::FILE_NAME)).unwrap(),
            "backup must be a byte-exact copy"
        );

        // Owner-only, like the archive it copies.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&out.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Second call the same day is a no-op — safe to call every tick.
        assert_eq!(daily(&arch, &back, 14).unwrap(), None);

        std::fs::remove_dir_all(&arch).ok();
        std::fs::remove_dir_all(&back).ok();
    }

    #[test]
    fn missing_archive_is_not_an_error() {
        let arch = unique_dir("empty");
        let back = unique_dir("empty-back");
        let _ = std::fs::remove_dir_all(&arch);
        std::fs::create_dir_all(&arch).unwrap();
        assert_eq!(daily(&arch, &back, 14).unwrap(), None);
        std::fs::remove_dir_all(&arch).ok();
        std::fs::remove_dir_all(&back).ok();
    }

    #[test]
    fn prune_keeps_the_newest_and_leaves_strays_alone() {
        let back = unique_dir("prune");
        let _ = std::fs::remove_dir_all(&back);
        std::fs::create_dir_all(&back).unwrap();
        for d in ["2026-01-01", "2026-01-02", "2026-01-03", "2026-01-04"] {
            std::fs::write(back.join(format!("visits-{d}.jsonl")), "").unwrap();
        }
        // An unrelated file must survive pruning.
        std::fs::write(back.join("notes.txt"), "keep me").unwrap();

        assert_eq!(prune(&back, 2).unwrap(), 2);
        assert!(!back.join("visits-2026-01-01.jsonl").exists());
        assert!(!back.join("visits-2026-01-02.jsonl").exists());
        assert!(back.join("visits-2026-01-03.jsonl").exists());
        assert!(back.join("visits-2026-01-04.jsonl").exists());
        assert!(back.join("notes.txt").exists());

        // keep = 0 disables pruning entirely.
        assert_eq!(prune(&back, 0).unwrap(), 0);
        assert!(back.join("visits-2026-01-03.jsonl").exists());

        std::fs::remove_dir_all(&back).ok();
    }
}
