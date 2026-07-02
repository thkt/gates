use std::env;
use std::fs;
use std::ops::Deref;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Fixture for the `project.rs`/`snapshot.rs` EXCLUDED_DIRS consolidation
/// guard tests. A literal, deliberately not a reference to either production
/// `EXCLUDED_DIRS` array, so deleting an entry from either array diverges
/// this expectation from actual behavior and fails the guard test.
pub const GUARDED_EXCLUDED_DIR_NAMES: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    "coverage",
    ".next",
    ".claude",
];

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "claude-gates-test-{}-{}-{}",
            prefix,
            process::id(),
            id
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

/// Symlink a committed, exec-only fixture from `tests/fixtures/<fixture>` to
/// `<project>/node_modules/.bin/<bin_name>`. The exec target is never written
/// during the test, so no parallel fork can hold a writable fd to the file the
/// gate execs — structurally removing the write-then-exec ETXTBSY race that
/// `fs::write` + `set_permissions` + exec hit under llvm-cov instrumentation (#59).
pub fn link_fake_bin(project: &Path, bin_name: &str, fixture: &str) {
    let bin_dir = project.join("node_modules/.bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    // symlink does not validate its target; assert first so a missing or
    // renamed fixture fails here with the path, not later as an opaque exec error.
    assert!(
        fixture_path.exists(),
        "missing fixture: {}",
        fixture_path.display()
    );
    symlink(&fixture_path, bin_dir.join(bin_name)).unwrap();
}
