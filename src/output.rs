//! Output mode (human-readable text vs. one JSON object per line).
//!
//! `--json` is a global flag; rather than thread it through every command and
//! every spawned task, it is stored in a process-global and consulted by the
//! emit helpers below. Mirrors s5cmd's `--json` behavior: each result is a
//! single JSON object on its own line.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static JSON: AtomicBool = AtomicBool::new(false);
static DRY_RUN: AtomicBool = AtomicBool::new(false);
static COLOR: AtomicBool = AtomicBool::new(false);

/// Enables/disables JSON output. Call once at startup from the global flag.
pub fn set_json(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

pub fn is_json() -> bool {
    JSON.load(Ordering::Relaxed)
}

/// Color choice requested by the user via `--color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// Resolves the requested color choice once at startup into a process-global
/// flag (mirrors `set_json` above).
///
/// `auto` enables color only when stdout is a TTY and the `NO_COLOR` convention
/// (https://no-color.org) is not in effect. JSON output must always stay clean,
/// so color is force-disabled whenever JSON mode is on — call `set_json` first.
pub fn set_color(choice: ColorChoice) {
    let no_color = std::env::var_os("NO_COLOR")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let enabled = match choice {
        ColorChoice::Never => false,
        ColorChoice::Always => true,
        ColorChoice::Auto => std::io::stdout().is_terminal() && !no_color,
    };
    COLOR.store(enabled && !is_json(), Ordering::Relaxed);
}

pub fn is_color() -> bool {
    COLOR.load(Ordering::Relaxed)
}

const C_RESET: &str = "\x1b[0m";
const C_BLUE: &str = "\x1b[34m";
const C_GREEN: &str = "\x1b[32m";
const C_RED: &str = "\x1b[31m";

/// Colors a directory / common-prefix (blue) when color is enabled, else
/// returns the string unchanged.
pub fn paint_dir(s: &str) -> String {
    if is_color() {
        format!("{C_BLUE}{s}{C_RESET}")
    } else {
        s.to_string()
    }
}

/// Colors an object name (green) when color is enabled, else unchanged.
pub fn paint_object(s: &str) -> String {
    if is_color() {
        format!("{C_GREEN}{s}{C_RESET}")
    } else {
        s.to_string()
    }
}

/// Colors an error fragment (red) when color is enabled, else unchanged.
pub fn paint_error(s: &str) -> String {
    if is_color() {
        format!("{C_RED}{s}{C_RESET}")
    } else {
        s.to_string()
    }
}

/// Marks the process as running under `--dry-run`. Call once at startup from the
/// global flag so result lines can be visibly distinguished from real ops.
pub fn set_dry_run(on: bool) {
    DRY_RUN.store(on, Ordering::Relaxed);
}

pub fn is_dry_run() -> bool {
    DRY_RUN.load(Ordering::Relaxed)
}

/// Emits a successful operation result. Under `--dry-run` the text branch
/// prefixes a `(dry-run) ` marker and the JSON branch adds `"dryRun": true`
/// (omitted otherwise) so dry-run output is never mistaken for a real op.
/// Text:  `[(dry-run) ]op src [dst]`
/// JSON:  `{"operation":op,"success":true,"source":src[,"destination":dst][,"dryRun":true]}`
pub fn op_success(op: &str, src: &str, dst: Option<&str>) {
    let dry = is_dry_run();
    if is_json() {
        let mut v = serde_json::json!({
            "operation": op,
            "success": true,
            "source": src,
        });
        if let Some(d) = dst {
            v["destination"] = serde_json::Value::String(d.to_string());
        }
        if dry {
            v["dryRun"] = serde_json::Value::Bool(true);
        }
        line(format_args!("{v}"));
    } else {
        let marker = if dry { "(dry-run) " } else { "" };
        match dst {
            Some(d) => line(format_args!("{marker}{op} {src} {d}")),
            None => line(format_args!("{marker}{op} {src}")),
        }
    }
}

/// Emits a failed operation result (to stderr in text mode, stdout in JSON mode
/// so it stays machine-parseable alongside successes — matching s5cmd).
pub fn op_error(op: &str, src: &str, dst: Option<&str>, err: &str) {
    if is_json() {
        let mut v = serde_json::json!({
            "operation": op,
            "success": false,
            "source": src,
            "error": err,
        });
        if let Some(d) = dst {
            v["destination"] = serde_json::Value::String(d.to_string());
        }
        line(format_args!("{v}"));
    } else {
        let prefix = paint_error("ERROR");
        match dst {
            Some(d) => err_line(format_args!("{prefix} {op} {src} {d}: {err}")),
            None => err_line(format_args!("{prefix} {op} {src}: {err}")),
        }
    }
}

/// Emits the result of an operation that has a destination but no source URL (`pipe`, which
/// reads stdin), as s5cmd does. Text: `op dst` on success, `ERROR op dst: err` on stderr;
/// JSON: `{"operation":op,"success":true,"destination":dst}`, with `"success":false` and
/// `"error"` for a failure.
pub fn op_result_to(op: &str, dst: &str, err: Option<&str>) {
    if is_json() {
        let mut v = serde_json::json!({
            "operation": op,
            "success": err.is_none(),
            "destination": dst,
        });
        if let Some(e) = err {
            v["error"] = serde_json::Value::String(e.to_string());
        }
        line(format_args!("{v}"));
    } else {
        match err {
            None => line(format_args!("{op} {dst}")),
            Some(e) => err_line(format_args!("{} {op} {dst}: {e}", paint_error("ERROR"))),
        }
    }
}

/// Emits a pre-built JSON value as one line (used by `ls`/`du` in JSON mode).
pub fn json_line(v: serde_json::Value) {
    line(format_args!("{v}"));
}

/// `println!` for result lines: see [`line`].
#[macro_export]
macro_rules! outln {
    () => {
        $crate::output::line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::output::line(format_args!($($arg)*))
    };
}

/// All result lines share one buffered stdout writer: one write(2) per 64 KiB instead of one per
/// line. It is flushed after every line when stdout is a terminal, before any line written with
/// [`err_line`], and before the process exits ([`flush`]).
static OUT: std::sync::OnceLock<std::sync::Mutex<Out>> = std::sync::OnceLock::new();

struct Out {
    w: std::io::BufWriter<std::io::Stdout>,
    tty: bool,
}

/// Writes one result line to stdout. A closed stdout pipe ends the process quietly (#7).
pub fn line(args: std::fmt::Arguments<'_>) {
    use std::io::{IsTerminal, Write};
    let m = OUT.get_or_init(|| {
        std::sync::Mutex::new(Out {
            w: std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout()),
            tty: std::io::stdout().is_terminal(),
        })
    });
    let mut out = m.lock().unwrap_or_else(|e| e.into_inner());
    let mut r = out.w.write_fmt(args).and_then(|()| out.w.write_all(b"\n"));
    if r.is_ok() && out.tty {
        r = out.w.flush();
    }
    if let Err(e) = r {
        stdout_failed(e);
    }
}

/// Writes buffered result lines out. Call before exiting.
pub fn flush() {
    use std::io::Write;
    if let Some(m) = OUT.get() {
        let r = m.lock().unwrap_or_else(|e| e.into_inner()).w.flush();
        if let Err(e) = r {
            stdout_failed(e);
        }
    }
}

/// Flushes what it can from a panic hook, without waiting for the writer's lock (the panic may
/// have happened while it was held) and ignoring errors.
pub fn flush_after_panic() {
    use std::io::Write;
    if let Some(Ok(mut out)) = OUT.get().map(|m| m.try_lock()) {
        let _ = out.w.flush();
    }
}

/// Writes one line to stderr after flushing the buffered result lines, so the two keep their
/// order on a terminal or a shared pipe (`2>&1`). A closed stderr pipe ends the process quietly.
pub fn err_line(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    flush();
    let mut err = std::io::stderr().lock();
    if let Err(e) = err.write_fmt(args).and_then(|()| err.write_all(b"\n")) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            die_of_broken_pipe();
        }
    }
}

/// Maps an error from writing data to stdout (`cat`, `select`): a closed pipe ends the process
/// quietly, anything else is returned.
pub fn stdout_error(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        die_of_broken_pipe();
    }
    anyhow::Error::new(e).context("writing to stdout")
}

fn stdout_failed(e: std::io::Error) -> ! {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        die_of_broken_pipe();
    }
    let _ = std::io::Write::write_fmt(
        &mut std::io::stderr(),
        format_args!("ERROR writing to stdout: {e}\n"),
    );
    std::process::exit(1)
}

/// The reader of stdout (or stderr) went away. End the process the way s5cmd (a Go program) and
/// other Unix tools end, killed by SIGPIPE (exit status 141 in a shell), with no message (#7).
/// Rust ignores SIGPIPE, so a write to a closed pipe returns EPIPE and `println!` panicked.
pub fn die_of_broken_pipe() -> ! {
    #[cfg(unix)]
    // SAFETY: restoring a signal's default disposition and raising it in this thread.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::raise(libc::SIGPIPE);
    }
    std::process::exit(141)
}
