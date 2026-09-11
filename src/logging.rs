//! Durable, dependency-free logging for a short-lived CLI.
//!
//! `rift-ergo` runs from Rift keybindings via `exec`, so stderr is discarded:
//! a failing binding is otherwise silent. Every invocation appends a line to
//! `~/Library/Logs/rift/rift-ergo.log`, alongside the reconciler's log, so a
//! breakage can be attributed to `rift-ergo` rather than to Rift itself.
//!
//! `RIFT_ERGO_LOG` overrides the path; `RIFT_ERGO_LOG=off` disables logging.
//! Logging never fails a workflow: write errors are intentionally dropped.

use std::env;
use std::fs::{OpenOptions, create_dir_all};
use std::io::Write;
use std::path::PathBuf;
use std::process;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

fn path() -> Option<&'static PathBuf> {
    PATH.get_or_init(|| match env::var_os("RIFT_ERGO_LOG") {
        Some(value) if value.eq_ignore_ascii_case("off") => None,
        Some(value) => Some(PathBuf::from(value)),
        None => env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Logs/rift/rift-ergo.log")),
    })
    .as_ref()
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` without pulling in a date crate.
fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.subsec_millis();
    let secs = now.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);

    // Howard Hinnant's civil_from_days, shifted to an era starting 0000-03-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn write(level: &str, message: &str) {
    let Some(path) = path() else { return };
    if let Some(parent) = path.parent() {
        let _ = create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "{} [{}] pid={} {}",
            timestamp(),
            level,
            process::id(),
            message
        );
    }
}

pub fn info(message: &str) {
    write("info", message);
}

pub fn warn(message: &str) {
    write("warn", message);
}

/// Records a failure. `source` attributes the blame: `rift` for IPC/Mach
/// failures originating in Rift, `rift-ergo` for our own logic.
pub fn error(source: &str, message: &str) {
    write("error", &format!("source={source} {message}"));
}

/// Where the log is being written, for `rift-ergo --log-path`.
pub fn describe() -> String {
    path().map_or_else(
        || "disabled (RIFT_ERGO_LOG=off)".to_string(),
        |path| path.display().to_string(),
    )
}
