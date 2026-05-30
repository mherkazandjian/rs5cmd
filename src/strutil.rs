//! String utilities ported from s5cmd's `strutil` package.

use regex::escape;

/// Converts a wildcarded expression to an equivalent regular expression.
/// `?` matches any single character, `*` matches any sequence.
pub fn wildcard_to_regexp(pattern: &str) -> String {
    let quoted = escape(pattern);
    // regex::escape turns `?` into `\?` and `*` into `\*`.
    quoted.replace("\\?", ".").replace("\\*", ".*")
}

/// Enforces that the regex matches the full string.
pub fn match_from_start_to_end(pattern: &str) -> String {
    format!("^{pattern}$")
}

/// Adds the `s` flag so `.` matches newlines, matching Go's `(?s)` prefix.
pub fn add_newline_flag(pattern: &str) -> String {
    format!("(?s){pattern}")
}

/// Converts the first character to uppercase and the rest to lowercase.
pub fn capitalize_first_rune(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_conversion() {
        assert_eq!(wildcard_to_regexp("a/b/test?/c/*.tsv"), "a/b/test./c/.*\\.tsv");
        assert_eq!(wildcard_to_regexp("*"), ".*");
        assert_eq!(wildcard_to_regexp("?"), ".");
    }

    #[test]
    fn capitalize() {
        assert_eq!(capitalize_first_rune("hELLO"), "Hello");
        assert_eq!(capitalize_first_rune(""), "");
    }
}
