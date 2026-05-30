//! A tiny progress-bar helper for the count of completed transfers.
//!
//! Bars render to stderr (the `indicatif` default), so they never corrupt
//! stdout — which carries normal output and, crucially, `--json` machine
//! output. The bar is a no-op in JSON mode, and `indicatif` itself auto-hides
//! when stderr is not a terminal, so callers can always construct one and
//! call `inc`/`finish` unconditionally.

use indicatif::{ProgressBar, ProgressStyle};

/// A counting progress bar that is a no-op in JSON mode (and hidden by
/// `indicatif` when stderr is not a TTY). `inc`/`finish` are safe to call
/// regardless of whether a bar was actually created.
pub struct Progress(Option<ProgressBar>);

impl Progress {
    /// Creates a progress bar sized to `total` items, labeled with `op`
    /// (e.g. `"cp"`, `"mv"`, `"sync"`). Returns a no-op bar in JSON mode.
    pub fn new(total: u64, op: &str) -> Progress {
        if crate::output::is_json() {
            return Progress(None);
        }
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template("{spinner} {msg} [{bar:30}] {pos}/{len} ({eta})")
                .unwrap()
                .progress_chars("=>-"),
        );
        pb.set_message(op.to_string());
        Progress(Some(pb))
    }

    /// Advances the bar by `n` completed items.
    pub fn inc(&self, n: u64) {
        if let Some(p) = &self.0 {
            p.inc(n);
        }
    }

    /// Clears the bar from the terminal.
    pub fn finish(&self) {
        if let Some(p) = &self.0 {
            p.finish_and_clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_mode_is_noop() {
        crate::output::set_json(true);
        let p = Progress::new(10, "cp");
        // No bar is created in JSON mode; these must not panic.
        p.inc(1);
        p.finish();
        assert!(p.0.is_none());
        crate::output::set_json(false);
    }
}
