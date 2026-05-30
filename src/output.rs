//! Output mode (human-readable text vs. one JSON object per line).
//!
//! `--json` is a global flag; rather than thread it through every command and
//! every spawned task, it is stored in a process-global and consulted by the
//! emit helpers below. Mirrors s5cmd's `--json` behavior: each result is a
//! single JSON object on its own line.

use std::sync::atomic::{AtomicBool, Ordering};

static JSON: AtomicBool = AtomicBool::new(false);
static DRY_RUN: AtomicBool = AtomicBool::new(false);

/// Enables/disables JSON output. Call once at startup from the global flag.
pub fn set_json(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

pub fn is_json() -> bool {
    JSON.load(Ordering::Relaxed)
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
        match dst {
            Some(d) => eprintln!("ERROR {op} {src} {d}: {err}"),
            None => eprintln!("ERROR {op} {src}: {err}"),
        }
    }
}

/// Emits a pre-built JSON value as one line (used by `ls`/`du` in JSON mode).
pub fn json_line(v: serde_json::Value) {
    println!("{v}");
}
