//! RLIMIT_NOFILE awareness (upstream s5cmd issue #390).
//!
//! With high parallelism, rs5cmd can open many file descriptors at once (one
//! per concurrent transfer, plus the S3 client's connection pool, local files,
//! stdio, the progress bar, etc.). If the process `RLIMIT_NOFILE` soft limit is
//! too low, transfers fail with `EMFILE` ("Too many open files").
//!
//! At startup we, on Unix, (1) try to raise the soft limit toward the hard
//! limit, and (2) estimate the descriptors the configured parallelism needs and
//! print ONE actionable warning if the (post-raise) soft limit is still below
//! that. A raw `EMFILE` surfaced mid-run is also annotated with the same hint
//! (see [`crate::error`]).
//!
//! No external crate is used: the Docker build resolves dependencies offline,
//! so we declare the tiny `getrlimit`/`setrlimit` FFI ourselves (Linux/glibc,
//! which is the build/run target). The pure threshold logic is platform
//! independent and unit-tested.

/// Per-descriptor headroom reserved on top of the worker estimate for stdio,
/// the S3 connection pool, log/progress, DNS sockets, etc.
#[cfg(unix)]
const FD_HEADROOM: u64 = 64;

/// Estimate the number of file descriptors a run may need from the effective
/// parallelism: each of `numworkers` concurrent transfers can hold a local file
/// descriptor plus up to `concurrency` multipart connections, plus a fixed
/// headroom. Uses saturating arithmetic so extreme flag values never panic.
#[cfg(unix)]
pub fn needed_descriptors(numworkers: usize, concurrency: usize) -> u64 {
    let numworkers = numworkers.max(1) as u64;
    // At least one connection per worker even if --concurrency is 0/1, plus the
    // worker's own local file fd.
    let per_worker = (concurrency.max(1) as u64).saturating_add(1);
    numworkers
        .saturating_mul(per_worker)
        .saturating_add(FD_HEADROOM)
}

/// Pure, testable threshold check.
///
/// Returns `Some(warning)` when the soft `RLIMIT_NOFILE` limit (`soft`) is below
/// the number of descriptors we estimate we need (`needed`); otherwise `None`.
/// The message names `RLIMIT_NOFILE` and gives an actionable hint.
#[cfg(unix)]
pub fn nofile_warning(soft: u64, needed: u64) -> Option<String> {
    if soft >= needed {
        return None;
    }
    Some(format!(
        "warning: open-file limit RLIMIT_NOFILE (soft = {soft}) is below the estimated \
         {needed} file descriptors needed for the current parallelism; you may hit \
         \"Too many open files\" (EMFILE) errors. Raise it with `ulimit -n {needed}` (or \
         higher), or reduce --numworkers/--concurrency."
    ))
}

/// Actionable hint appended to a raw `EMFILE` error encountered mid-run.
pub const EMFILE_HINT: &str = "the OS open-file limit (RLIMIT_NOFILE) was exhausted; \
    raise it with `ulimit -n <N>` or reduce --numworkers/--concurrency";

/// At startup, try to raise the soft `RLIMIT_NOFILE` limit toward the hard
/// limit, then warn once (to stderr) if it is still likely too low for the
/// configured parallelism. Best-effort: never fails the program.
#[cfg(unix)]
pub fn setup_nofile_limits(numworkers: usize, concurrency: usize) {
    // Probe current limits.
    let (soft, hard) = match get_nofile() {
        Some(pair) => pair,
        None => return,
    };

    // (1) Raise the soft limit toward the hard limit.
    let mut effective_soft = soft;
    if soft < hard {
        if set_nofile(hard, hard) {
            effective_soft = hard;
        } else if let Some((new_soft, _)) = get_nofile() {
            effective_soft = new_soft;
        }
    }

    // (2) Warn once if the effective soft limit is still below the estimate.
    let needed = needed_descriptors(numworkers, concurrency);
    if let Some(msg) = nofile_warning(effective_soft, needed) {
        eprintln!("{msg}");
    }
}

/// No-op on non-Unix platforms.
#[cfg(not(unix))]
pub fn setup_nofile_limits(_numworkers: usize, _concurrency: usize) {}

// ---------------------------------------------------------------------------
// Minimal, dependency-free FFI for getrlimit/setrlimit on Linux/glibc.
//
// The test image builds fully offline and cannot resolve new crates, so we
// avoid `libc`/`rlimit` and declare just what we need. On Linux glibc:
// `rlim_t` is `unsigned long` (u64 on LP64), `RLIMIT_NOFILE` is 7, and
// `RLIM_INFINITY` is `~0`.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod sys {
    pub type RlimT = u64;
    pub const RLIMIT_NOFILE: i32 = 7;
    pub const RLIM_INFINITY: RlimT = RlimT::MAX;

    #[repr(C)]
    pub struct Rlimit {
        pub rlim_cur: RlimT,
        pub rlim_max: RlimT,
    }

    extern "C" {
        pub fn getrlimit(resource: i32, rlim: *mut Rlimit) -> i32;
        pub fn setrlimit(resource: i32, rlim: *const Rlimit) -> i32;
    }
}

/// Current `(soft, hard)` `RLIMIT_NOFILE`, or `None` on error. `RLIM_INFINITY`
/// is normalized to `u64::MAX`.
#[cfg(target_os = "linux")]
fn get_nofile() -> Option<(u64, u64)> {
    let mut rl = sys::Rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into a fully initialized `Rlimit`; the resource
    // id is a valid constant.
    let rc = unsafe { sys::getrlimit(sys::RLIMIT_NOFILE, &mut rl) };
    if rc != 0 {
        return None;
    }
    Some((norm(rl.rlim_cur), norm(rl.rlim_max)))
}

/// Set the `RLIMIT_NOFILE` soft/hard limits; returns true on success.
/// `u64::MAX` is mapped back to `RLIM_INFINITY`.
#[cfg(target_os = "linux")]
fn set_nofile(soft: u64, hard: u64) -> bool {
    let rl = sys::Rlimit {
        rlim_cur: denorm(soft),
        rlim_max: denorm(hard),
    };
    // SAFETY: `rl` is a fully initialized `Rlimit`; the resource id is valid.
    let rc = unsafe { sys::setrlimit(sys::RLIMIT_NOFILE, &rl) };
    rc == 0
}

#[cfg(target_os = "linux")]
fn norm(v: sys::RlimT) -> u64 {
    if v == sys::RLIM_INFINITY {
        u64::MAX
    } else {
        v
    }
}

#[cfg(target_os = "linux")]
fn denorm(v: u64) -> sys::RlimT {
    if v == u64::MAX {
        sys::RLIM_INFINITY
    } else {
        v
    }
}

// On non-Linux Unix we have no portable, dependency-free syscall bindings, so
// probing/raising is skipped (best-effort). The public API and the threshold
// logic stay available and testable on every Unix.
#[cfg(all(unix, not(target_os = "linux")))]
fn get_nofile() -> Option<(u64, u64)> {
    None
}

#[cfg(all(unix, not(target_os = "linux")))]
fn set_nofile(_soft: u64, _hard: u64) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn below_threshold_returns_warning() {
        let w = nofile_warning(64, 1280);
        assert!(w.is_some(), "expected a warning when soft < needed");
        let msg = w.unwrap();
        assert!(msg.contains("RLIMIT_NOFILE"), "names the limit: {msg}");
        assert!(msg.contains("ulimit -n"), "suggests ulimit: {msg}");
    }

    #[test]
    fn at_threshold_returns_none() {
        assert!(
            nofile_warning(1280, 1280).is_none(),
            "no warning when soft == needed"
        );
    }

    #[test]
    fn above_threshold_returns_none() {
        assert!(
            nofile_warning(65536, 1280).is_none(),
            "no warning when soft > needed"
        );
    }

    #[test]
    fn estimate_scales_with_workers_and_concurrency() {
        // 256 workers * (5 + 1) + 64 headroom = 1600
        assert_eq!(needed_descriptors(256, 5), 1600);
        // headroom is always added even for trivial parallelism
        assert_eq!(needed_descriptors(1, 1), 2 + FD_HEADROOM);
        // zero is clamped to at least 1 worker / 1 connection
        assert_eq!(needed_descriptors(0, 0), 2 + FD_HEADROOM);
    }

    #[test]
    fn estimate_does_not_overflow() {
        // saturating arithmetic must not panic on extreme inputs
        assert_eq!(needed_descriptors(usize::MAX, usize::MAX), u64::MAX);
    }

    #[test]
    fn defaults_below_typical_1024_limit_warn() {
        // With the shipped defaults (--numworkers 256, --concurrency 8) the
        // estimate far exceeds a common 1024 soft limit, so a warning is shown.
        let needed = needed_descriptors(256, 8);
        assert!(needed > 1024, "default estimate should exceed 1024: {needed}");
        assert!(nofile_warning(1024, needed).is_some());
    }
}
