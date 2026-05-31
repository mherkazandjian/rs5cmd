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
        println!("{v}");
    } else {
        let marker = if dry { "(dry-run) " } else { "" };
        match dst {
            Some(d) => println!("{marker}{op} {src} {d}"),
            None => println!("{marker}{op} {src}"),
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
        println!("{v}");
    } else {
        let prefix = paint_error("ERROR");
        match dst {
            Some(d) => eprintln!("{prefix} {op} {src} {d}: {err}"),
            None => eprintln!("{prefix} {op} {src}: {err}"),
        }
    }
}

/// Emits a pre-built JSON value as one line (used by `ls`/`du` in JSON mode).
pub fn json_line(v: serde_json::Value) {
    println!("{v}");
}
