//! Persistent audit log: appends every pass/fail decision to a JSONL file and
//! reads it back for the `show` subcommand. Writes are fail-open — a failure to
//! record never breaks the hook (OUTCOME constraint: nothing blocks the agent).

use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE: &str = "audit.jsonl";

/// One audit record. `failed` is empty on a pass decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: String,
    pub project: String,
    pub decision: String,
    pub failed: Vec<String>,
}

/// Resolve the audit directory from `$XDG_DATA_HOME/gates`, falling back to
/// `$HOME/.local/share/gates`. Returns None when neither is set (write skipped).
pub fn default_dir() -> Option<PathBuf> {
    if let Some(xdg) = env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        return Some(Path::new(&xdg).join("gates"));
    }
    env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|home| Path::new(&home).join(".local/share/gates"))
}

/// Append one entry as a JSONL line. Creates the directory and file as needed.
/// Returns Err on any IO/serialization failure; the caller must not propagate it
/// into the hook control flow.
pub fn append(dir: &Path, entry: &AuditEntry) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut line = serde_json::to_string(entry)?;
    line.push('\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))?;
    // One write_all, not writeln!: writeln! emits the body and the newline as
    // separate write() syscalls, so under O_APPEND two concurrent hook processes
    // can interleave into a torn JSONL line. A single write of the line+newline
    // keeps each record atomic (within PIPE_BUF).
    file.write_all(line.as_bytes())
}

/// Read entries, optionally filter by decision, then keep the last `last` of the
/// matched set. A missing log file is normal (nothing recorded yet) → empty Vec.
pub fn query(dir: &Path, last: usize, decision: Option<&str>) -> Vec<AuditEntry> {
    let content = match fs::read_to_string(dir.join(LOG_FILE)) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut entries: Vec<AuditEntry> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok())
        .filter(|e| decision.is_none_or(|d| e.decision == d))
        .collect();
    if entries.len() > last {
        entries = entries.split_off(entries.len() - last);
    }
    entries
}

/// Render entries as an aligned table: `TIMESTAMP  DECISION  PROJECT  FAILED_GATES`.
pub fn render(entries: &[AuditEntry]) -> String {
    let header = ["TIMESTAMP", "DECISION", "PROJECT", "FAILED_GATES"];
    let mut rows: Vec<[String; 4]> = vec![header.map(str::to_owned)];
    for e in entries {
        let failed = if e.failed.is_empty() {
            "-".to_owned()
        } else {
            e.failed.join(",")
        };
        rows.push([e.ts.clone(), e.decision.clone(), e.project.clone(), failed]);
    }

    // The last column needs no padding, so width it across the first three only.
    // Count chars, not bytes: `{:<w$}` pads by char count, so a byte-length width
    // would misalign columns for non-ASCII project paths.
    let mut widths = [0usize; 3];
    for row in &rows {
        for (i, w) in widths.iter_mut().enumerate() {
            *w = (*w).max(row[i].chars().count());
        }
    }

    rows.iter()
        .map(|row| {
            format!(
                "{:<w0$}  {:<w1$}  {:<w2$}  {}",
                row[0],
                row[1],
                row[2],
                row[3],
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2],
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Current UTC time as an RFC3339 string (`YYYY-MM-DDThh:mm:ssZ`).
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_rfc3339(secs)
}

/// Format epoch seconds as an RFC3339 UTC string. Date split uses Howard
/// Hinnant's `civil_from_days` algorithm (chrono-free, no startup cost).
fn format_rfc3339(epoch_secs: u64) -> String {
    let days = i64::try_from(epoch_secs / 86_400).unwrap_or(0);
    let sod = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, min, sec) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Convert days since 1970-01-01 to (year, month [1-12], day [1-31]).
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>
// m ∈ [1,12], d ∈ [1,31] by construction, so neither u32 cast can truncate.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TempDir;

    fn entry(ts: &str, decision: &str, failed: &[&str]) -> AuditEntry {
        AuditEntry {
            ts: ts.to_owned(),
            project: "/p".to_owned(),
            decision: decision.to_owned(),
            failed: failed.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn formats_unix_epoch_zero() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn formats_known_epoch() {
        // 1700000000 == 2023-11-14T22:13:20Z
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn formats_leap_day() {
        // 1709164800 == 2024-02-29T00:00:00Z (leap day); +86400 == 2024-03-01
        assert_eq!(format_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_709_251_200), "2024-03-01T00:00:00Z");
    }

    #[test]
    fn append_then_query_round_trips() {
        let tmp = TempDir::new("audit-rt");
        let e = entry("2026-04-11T11:00:00Z", "fail", &["lint", "test"]);
        append(&tmp, &e).unwrap();
        let got = query(&tmp, 20, None);
        assert_eq!(got, vec![e]);
    }

    #[test]
    fn append_creates_missing_directory() {
        let tmp = TempDir::new("audit-mkdir");
        let nested = tmp.join("a/b/c");
        append(&nested, &entry("t", "pass", &[])).unwrap();
        assert_eq!(query(&nested, 20, None).len(), 1);
    }

    #[test]
    fn query_missing_file_returns_empty() {
        let tmp = TempDir::new("audit-missing");
        assert!(query(&tmp, 20, None).is_empty());
    }

    #[test]
    fn query_filters_by_decision() {
        let tmp = TempDir::new("audit-filter");
        append(&tmp, &entry("t1", "pass", &[])).unwrap();
        append(&tmp, &entry("t2", "fail", &["lint"])).unwrap();
        append(&tmp, &entry("t3", "pass", &[])).unwrap();

        let fails = query(&tmp, 20, Some("fail"));
        assert_eq!(fails.len(), 1);
        assert_eq!(fails[0].ts, "t2");

        assert_eq!(query(&tmp, 20, Some("pass")).len(), 2);
    }

    #[test]
    fn query_keeps_last_n_of_matched_set() {
        let tmp = TempDir::new("audit-lastn");
        for i in 0..5 {
            append(&tmp, &entry(&format!("t{i}"), "pass", &[])).unwrap();
        }
        let got = query(&tmp, 2, None);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ts, "t3");
        assert_eq!(got[1].ts, "t4");
    }

    #[test]
    fn concurrent_appends_do_not_tear_lines() {
        // Each record must reach the file as one atomic write so parallel hook
        // processes never interleave into a torn JSONL line. Drive it with many
        // threads appending to the same dir, then assert every entry survives.
        use std::sync::Arc;
        use std::thread;

        let tmp = Arc::new(TempDir::new("audit-concurrent"));
        let threads = 8;
        let per_thread = 25;
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let dir = Arc::clone(&tmp);
                thread::spawn(move || {
                    for i in 0..per_thread {
                        let e = entry(&format!("t{t}-{i}"), "fail", &["lint", "test"]);
                        append(&dir, &e).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let got = query(&tmp, usize::MAX, None);
        assert_eq!(got.len(), threads * per_thread);
    }

    #[test]
    fn query_skips_corrupt_lines() {
        let tmp = TempDir::new("audit-corrupt");
        append(&tmp, &entry("t1", "pass", &[])).unwrap();
        // Append a non-JSON line directly.
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(tmp.join(LOG_FILE))
            .unwrap();
        writeln!(f, "not json{{").unwrap();
        append(&tmp, &entry("t2", "fail", &["x"])).unwrap();

        let got = query(&tmp, 20, None);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ts, "t1");
        assert_eq!(got[1].ts, "t2");
    }

    #[test]
    fn renders_table_with_header_and_failed_dash() {
        let out = render(&[
            entry("2026-04-11T11:00:00Z", "fail", &["lint", "test"]),
            entry("2026-04-11T11:05:30Z", "pass", &[]),
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("TIMESTAMP"));
        assert!(lines[0].contains("FAILED_GATES"));
        assert!(lines[1].contains("fail"));
        assert!(lines[1].contains("lint,test"));
        assert!(lines[2].trim_end().ends_with('-'));
    }

    #[test]
    fn renders_align_columns_by_char_count_for_non_ascii() {
        let mut a = entry("2026-04-11T11:00:00Z", "pass", &[]);
        a.project = "/プロジェクト".to_owned(); // non-ASCII path
        let mut b = entry("2026-04-11T11:05:30Z", "pass", &[]);
        b.project = "/p".to_owned();
        let out = render(&[a, b]);
        // The FAILED_GATES column starts at the same char offset on every line.
        let offsets: Vec<usize> = out
            .lines()
            .map(|l| l.chars().count() - l.chars().rev().take_while(|c| *c != ' ').count())
            .collect();
        assert!(
            offsets.windows(2).all(|w| w[0] == w[1]),
            "columns misaligned: {offsets:?}"
        );
    }

    #[test]
    fn renders_empty_entries_as_header_only() {
        let out = render(&[]);
        assert_eq!(out.lines().count(), 1);
        assert!(out.starts_with("TIMESTAMP"));
    }
}
