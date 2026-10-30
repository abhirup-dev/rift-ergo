//! Suppression lease shared by the workflows and the focus-follow hook.
//!
//! `placement::acquire_display` deliberately focuses windows Rift keeps hidden,
//! which is indistinguishable, from the outside, from the user raising a window
//! in a hidden workspace. Without a shared signal the hook would answer our own
//! focus moves by switching workspaces mid-workflow.
//!
//! The signal is a single timestamp: "ignore focus events until". Every
//! confirmed transaction step pushes it forward, so it stays valid for as long
//! as a workflow is actually driving Rift and expires on its own if the process
//! dies. A short grace period after the last step covers the events Rift
//! publishes as the workflow settles.
//!
//! `RIFT_ERGO_FOCUS_LEASE` overrides the path. A missing or unreadable file
//! means no lease, which is the safe default: the hook runs.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

/// How long a single step vouches for. Long enough to cover Rift's slowest
/// confirmed step, short enough that a killed workflow stops suppressing
/// quickly.
const LEASE_WINDOW_MS: u64 = 2_000;

/// Held after the last step, for the focus events a settling workflow emits.
/// Measured: Rift published one 2.1 s after a workflow returned and the hook
/// answered it by pulling a different workspace forward, so the grace is
/// longer than the longest tail seen. The cost is that a raise you perform
/// yourself within this window of a binding is ignored; the hook only matters
/// when Rift's own auto-switch fails, so that trade is cheap.
const RELEASE_GRACE_MS: u64 = 2_500;

/// A hook that died without cleaning up must not suppress the next one forever.
const SINGLE_FLIGHT_STALE_MS: u64 = 5_000;

fn lease_path() -> PathBuf {
    env::var_os("RIFT_ERGO_FOCUS_LEASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/rift-ergo-focus.lease"))
}

fn lock_path() -> PathBuf {
    lease_path().with_extension("lock")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

fn write_until(until_ms: u64) {
    let path = lease_path();
    let staging = path.with_extension(format!("tmp-{}", process::id()));
    if fs::write(&staging, until_ms.to_string()).is_ok() && fs::rename(&staging, &path).is_err() {
        let _ = fs::remove_file(&staging);
    }
}

fn read_until() -> Option<u64> {
    fs::read_to_string(lease_path())
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Push the lease forward. Called at the start of a focus-moving command and
/// again on every confirmed step, so long workflows stay covered.
pub fn renew() {
    write_until(now_ms() + LEASE_WINDOW_MS);
}

/// Shorten the lease to the settling grace period once the work is done.
pub fn release() {
    write_until(now_ms() + RELEASE_GRACE_MS);
}

/// Whether a workflow is currently driving Rift's focus.
pub fn active() -> bool {
    holds(read_until(), now_ms())
}

/// A missing, unreadable, or expired lease means the hook runs: failing open
/// keeps a stale file from disabling focus-follow indefinitely.
fn holds(until_ms: Option<u64>, now_ms: u64) -> bool {
    until_ms.is_some_and(|until| until > now_ms)
}

/// Exclusive marker for the hook, so two focus events cannot drive two
/// concurrent switches. Released on drop, and broken if a previous holder left
/// it behind.
pub struct SingleFlight {
    path: PathBuf,
}

impl Drop for SingleFlight {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// `None` when another hook invocation holds the marker.
pub fn try_single_flight() -> Option<SingleFlight> {
    try_single_flight_at(lock_path())
}

/// The same exclusion for hooks that keep their own marker, such as the
/// new-window reconciler.
pub fn try_single_flight_at(path: PathBuf) -> Option<SingleFlight> {
    if claim(&path) {
        return Some(SingleFlight { path });
    }
    let stale = fs::read_to_string(&path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u64>().ok())
        .is_none_or(|claimed_at| now_ms().saturating_sub(claimed_at) > SINGLE_FLIGHT_STALE_MS);
    if !stale {
        return None;
    }
    let _ = fs::remove_file(&path);
    claim(&path).then_some(SingleFlight { path })
}

fn claim(path: &PathBuf) -> bool {
    use std::io::Write;

    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    {
        Ok(mut file) => {
            let _ = write!(file, "{}", now_ms());
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_holds_only_until_it_expires() {
        assert!(holds(Some(1_000), 999));
        assert!(!holds(Some(1_000), 1_000));
        assert!(!holds(Some(1_000), 1_001));
        assert!(!holds(None, 1_000));
    }
}
