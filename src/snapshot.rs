//! Filesystem-delta snapshot for the `PostToolUse` Bash trigger (issue #17).
//!
//! Computes a ctime+size digest over the target fileset (FR-001), and reads /
//! writes the per-project digest state file fail-open (FR-002..FR-004). Bash
//! edits that gates would otherwise miss are detected by comparing a freshly
//! computed digest against the stored one.
//!
//! Every read/write degrades fail-open: an uncertain read counts as "changed"
//! (run the gates) and a write failure is reported to stderr without ever
//! blocking. The `#[cfg(test)]` block below covers spec.md T-001..T-023.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

/// Directories whose contents never belong to the gated fileset. Mirrors
/// `depgraph::EXCLUDED_DIRS` plus `coverage`/`.next` (test/build artifacts that
/// gates never lints).
const EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    "coverage",
    ".next",
];

/// Source extensions gates consumes (tsc/eslint/embedded gates).
const SOURCE_EXTS: &[&str] = &["ts", "tsx", "cts", "mts", "js", "jsx", "cjs", "mjs"];

/// Disambiguates concurrent same-project tmp files within one process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Whether `path` is part of the gated fileset: a source file by extension, or a
/// `package.json` / `tsconfig*.json` config (at any nesting depth).
fn is_target_file(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && SOURCE_EXTS.contains(&ext)
    {
        return true;
    }
    match path.file_name().and_then(|n| n.to_str()) {
        Some("package.json") => true,
        Some(name) => name.starts_with("tsconfig") && name.ends_with(".json"),
        None => false,
    }
}

/// Collect (root-relative path, absolute path) of every target file under `dir`.
/// Recurses into subdirectories except the excluded set. A symlink is classified
/// by its own type (not the target), so a dangling symlink with a source
/// extension is still collected and its broken stat surfaces in `compute_digest`.
fn collect_targets(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    // An unreadable subtree contributes no entries, so its contents are not
    // represented in the digest. This is fail-open toward "run" whenever the
    // directory is readable in only one of the two snapshots (the file set
    // differs → digest differs → gates run). The sole blind spot — a directory
    // unreadable across both snapshots — is fundamentally undetectable: nothing
    // can reveal what changed inside a directory we cannot read either time.
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|ft| ft.is_dir());
        if is_dir {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !EXCLUDED_DIRS.contains(&name) {
                collect_targets(&path, root, out);
            }
        } else if is_target_file(&path)
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push((rel.to_string_lossy().into_owned(), path));
        }
    }
}

/// Compute the digest of the target fileset under `root` (FR-001).
///
/// Walks `root` for `.ts/.tsx/.cts/.mts/.js/.jsx/.cjs/.mjs` plus every
/// `package.json` / `tsconfig*.json` (recursive, excluding
/// `node_modules/.git/dist/build/target/coverage/.next`), feeding
/// (relative path, size, ctime secs, ctime nsecs) of each into a
/// `DefaultHasher`. ctime is used over content bytes because the kernel bumps it
/// on any inode write and userspace cannot backdate it (`utimes` touches only
/// mtime/atime), so a `tar x`/`cp -p` that restores mtime is still caught — at
/// stat cost, not O(bytes). A file whose stat fails contributes a distinct
/// sentinel marker so the failure propagates as a change rather than being
/// silently dropped.
pub fn compute_digest(root: &Path) -> String {
    let mut targets = Vec::new();
    collect_targets(root, root, &mut targets);
    targets.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = DefaultHasher::new();
    for (rel, path) in &targets {
        rel.hash(&mut hasher);
        match fs::metadata(path) {
            Ok(meta) => {
                // 0 marks a successful stat, keeping an empty file (len 0)
                // structurally distinct from a stat failure (marker 1).
                0u8.hash(&mut hasher);
                meta.len().hash(&mut hasher);
                meta.ctime().hash(&mut hasher);
                meta.ctime_nsec().hash(&mut hasher);
            }
            Err(_) => {
                1u8.hash(&mut hasher);
            }
        }
    }
    format!("{:016x}", hasher.finish())
}

/// Per-project state file path: `dir/<hash of canonical root>.digest`. Keyed by
/// the canonicalized root (falling back to the raw path, as audit does) so the
/// same project maps to one file regardless of the cwd passed to gates.
fn state_path(dir: &Path, root: &Path) -> PathBuf {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = DefaultHasher::new();
    canonical.to_string_lossy().hash(&mut hasher);
    dir.join(format!("{:016x}.digest", hasher.finish()))
}

/// Read the stored digest for `root` from `dir` (FR-003).
///
/// Returns `None` (treated as "changed" by the caller) for every degraded case:
/// state file absent, unreadable, empty, or not in digest (hex) form. Never
/// propagates an error to the caller.
pub fn read_stored(dir: &Path, root: &Path) -> Option<String> {
    let content = fs::read_to_string(state_path(dir, root)).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Write `digest` for `root` into `dir` via tmp + atomic rename (FR-004).
///
/// Creates `dir` as needed. The tmp file lives in `dir` (same filesystem, so the
/// rename is atomic) and carries the pid + a counter so concurrent same-project
/// writers never clobber one tmp. Returns `Err` on IO failure; the caller
/// reports it to stderr and never converts it into a block (fail-open).
pub fn write(dir: &Path, root: &Path, digest: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let final_path = state_path(dir, root);
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(".gates-snapshot-{}-{n}.tmp", process::id()));
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(digest.as_bytes())?;
    }
    fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    /// A valid digest literal (hex string of a u64) for round-trip tests that
    /// must not depend on `compute_digest` being implemented.
    const VALID_DIGEST: &str = "1a2b3c4d5e6f0011";

    fn find_state_file(dir: &Path) -> PathBuf {
        let mut found = None;
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) == Some("digest") {
                found = Some(path);
            }
        }
        found.expect("write should create a *.digest state file")
    }

    // T-001: a content edit that changes file size changes the digest.
    #[test]
    fn digest_changes_on_content_size_edit_t001() {
        let root = TempDir::new("snap-t001");
        fs::write(root.join("a.ts"), "const x = 1;").unwrap();
        let before = compute_digest(&root);
        fs::write(root.join("a.ts"), "const x = 1; const y = 2;").unwrap();
        let after = compute_digest(&root);
        assert_ne!(before, after);
    }

    // T-002: a SAME-SIZE content edit followed by restoring mtime to the past
    // still changes the digest, because ctime advances and cannot be backdated
    // from userspace. Same size on purpose: this is the exact case mtime+size
    // misses, so it is the test that justifies the ctime field. ctime_nsec gives
    // distinct values to two separate write syscalls, so it is not flaky.
    #[test]
    fn digest_changes_when_mtime_restored_to_past_t002() {
        let root = TempDir::new("snap-t002");
        let file = root.join("a.ts");
        fs::write(&file, "aaa").unwrap();
        let before = compute_digest(&root);
        // Same byte length (3) as before: only ctime can distinguish them.
        fs::write(&file, "bbb").unwrap();
        // Restore mtime to 2000-01-01 00:00; ctime still moves forward.
        let status = Command::new("touch")
            .arg("-mt")
            .arg("200001010000")
            .arg(&file)
            .status()
            .expect("touch should be available on macOS/Linux");
        assert!(status.success());
        let after = compute_digest(&root);
        assert_ne!(before, after);
    }

    // T-003: adding a target file changes the digest.
    #[test]
    fn digest_changes_on_target_file_add_t003() {
        let root = TempDir::new("snap-t003");
        fs::write(root.join("a.ts"), "x").unwrap();
        let before = compute_digest(&root);
        fs::write(root.join("b.ts"), "y").unwrap();
        let after = compute_digest(&root);
        assert_ne!(before, after);
    }

    // T-004: deleting a target file changes the digest.
    #[test]
    fn digest_changes_on_target_file_delete_t004() {
        let root = TempDir::new("snap-t004");
        fs::write(root.join("a.ts"), "x").unwrap();
        fs::write(root.join("b.ts"), "y").unwrap();
        let before = compute_digest(&root);
        fs::remove_file(root.join("b.ts")).unwrap();
        let after = compute_digest(&root);
        assert_ne!(before, after);
    }

    // T-005: renaming a target file changes the digest (relative path changes).
    #[test]
    fn digest_changes_on_target_file_rename_t005() {
        let root = TempDir::new("snap-t005");
        fs::write(root.join("a.ts"), "x").unwrap();
        let before = compute_digest(&root);
        fs::rename(root.join("a.ts"), root.join("b.ts")).unwrap();
        let after = compute_digest(&root);
        assert_ne!(before, after);
    }

    // T-006: changing a non-target file (.md) does not change the digest.
    #[test]
    fn digest_unchanged_on_non_target_file_edit_t006() {
        let root = TempDir::new("snap-t006");
        fs::write(root.join("a.ts"), "x").unwrap();
        fs::write(root.join("README.md"), "v1").unwrap();
        let before = compute_digest(&root);
        fs::write(root.join("README.md"), "v2 longer markdown body").unwrap();
        let after = compute_digest(&root);
        assert_eq!(before, after);
    }

    // T-007: changes under an excluded directory (dist/) do not change the digest.
    #[test]
    fn digest_unchanged_on_excluded_dir_edit_t007() {
        let root = TempDir::new("snap-t007");
        fs::write(root.join("a.ts"), "x").unwrap();
        let before = compute_digest(&root);
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("dist/bundle.ts"), "ignored").unwrap();
        let after = compute_digest(&root);
        assert_eq!(before, after);
    }

    // T-008: adding a .mts or .cts file changes the digest (extension scope).
    #[test]
    fn digest_changes_on_mts_and_cts_add_t008() {
        for ext in ["mts", "cts"] {
            let root = TempDir::new("snap-t008");
            fs::write(root.join("a.ts"), "x").unwrap();
            let before = compute_digest(&root);
            fs::write(root.join(format!("b.{ext}")), "y").unwrap();
            let after = compute_digest(&root);
            assert_ne!(before, after, "extension {ext} must be in scope");
        }
    }

    // T-009: a nested packages/foo/tsconfig.json change is in scope (recursive
    // config matching).
    #[test]
    fn digest_changes_on_nested_tsconfig_t009() {
        let root = TempDir::new("snap-t009");
        fs::write(root.join("a.ts"), "x").unwrap();
        let before = compute_digest(&root);
        fs::create_dir_all(root.join("packages/foo")).unwrap();
        fs::write(root.join("packages/foo/tsconfig.json"), "{}").unwrap();
        let after = compute_digest(&root);
        assert_ne!(before, after);
    }

    // T-010: a missing state file reads as None (= changed / baseline absent).
    #[test]
    fn read_stored_missing_file_is_none_t010() {
        let dir = TempDir::new("snap-t010-dir");
        let root = TempDir::new("snap-t010-root");
        assert_eq!(read_stored(&dir, &root), None);
    }

    // T-011: a corrupt state file (empty / non-hex) reads as None.
    #[test]
    fn read_stored_corrupt_file_is_none_t011() {
        let dir = TempDir::new("snap-t011-dir");
        let root = TempDir::new("snap-t011-root");
        // Create the correctly-named state file via write, then corrupt it.
        write(&dir, &root, VALID_DIGEST).unwrap();
        let state = find_state_file(&dir);

        fs::write(&state, "").unwrap();
        assert_eq!(read_stored(&dir, &root), None, "empty content is corrupt");

        fs::write(&state, "not-hex!!").unwrap();
        assert_eq!(read_stored(&dir, &root), None, "non-hex content is corrupt");
    }

    // T-012: write then read returns the same digest (round-trip).
    #[test]
    fn write_then_read_round_trips_t012() {
        let dir = TempDir::new("snap-t012-dir");
        let root = TempDir::new("snap-t012-root");
        write(&dir, &root, VALID_DIGEST).unwrap();
        assert_eq!(read_stored(&dir, &root), Some(VALID_DIGEST.to_owned()));
    }

    // T-013: write is atomic — no tmp file remains and the state file holds the
    // complete digest.
    #[test]
    fn write_is_atomic_no_tmp_leftover_t013() {
        let dir = TempDir::new("snap-t013-dir");
        let root = TempDir::new("snap-t013-root");
        write(&dir, &root, VALID_DIGEST).unwrap();

        for entry in fs::read_dir(&*dir).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains("tmp"),
                "no tmp file should remain after write, found {name}"
            );
        }
        assert_eq!(read_stored(&dir, &root), Some(VALID_DIGEST.to_owned()));
    }

    // T-018: double-detection prevention — after the W/E/M path writes the
    // digest, a follow-up read matches and recompute is unchanged, so the next
    // `gates post-bash` would skip. (Unit-level logic; full dispatch in main.rs.)
    #[test]
    fn write_then_read_matches_so_followup_skips_t018() {
        let dir = TempDir::new("snap-t018-dir");
        let root = TempDir::new("snap-t018-root");
        fs::write(root.join("a.ts"), "x").unwrap();
        let digest = compute_digest(&root);
        write(&dir, &root, &digest).unwrap();
        assert_eq!(read_stored(&dir, &root), Some(digest.clone()));
        // No filesystem change between writes → recompute equals stored.
        assert_eq!(compute_digest(&root), digest);
    }

    // T-020: fail-open — a missing snapshot dir reads as None (= changed), so
    // gates runs rather than blocking.
    #[test]
    fn read_stored_missing_dir_is_none_t020() {
        let root = TempDir::new("snap-t020-root");
        let missing = root.join("absent-snapshot-dir");
        assert_eq!(read_stored(&missing, &root), None);
    }

    // T-022: a stat-failed file contributes a sentinel rather than being dropped
    // (FR-001: "stat 失敗が変更として伝播する"). keep.ts is untouched between the
    // two calls, so the only delta is the dangling symlink broken.ts — no ctime
    // confound. If production mirrors depgraph's skip-on-canonicalize-failure the
    // broken file is dropped and the digests stay equal, failing this test.
    #[test]
    fn stat_failed_file_contributes_sentinel_t022() {
        let root = TempDir::new("snap-t022");
        fs::write(root.join("keep.ts"), "x").unwrap();
        let without = compute_digest(&root);
        symlink("nonexistent_target", root.join("broken.ts")).unwrap();
        let with_broken = compute_digest(&root);
        assert_ne!(without, with_broken);
    }
}
