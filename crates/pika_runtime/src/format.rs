//! How values are printed by `:put` and string interpolation.

use std::fmt::Write;

/// Formats a 64-bit float.
///
/// Whole numbers keep a `.0` so that floats are recognizable (`3.0`, not `3`). Very large and
/// very small magnitudes use exponent notation (`1e300`, `1.5e-7`).
pub fn f64_to_string(value: f64) -> String {
    float_to_string(value, value.abs(), |v| v.to_string(), |v| format!("{v:e}"))
}

/// Formats a 32-bit float, with the same rules as [`f64_to_string`].
pub fn f32_to_string(value: f32) -> String {
    float_to_string(
        f64::from(value),
        f64::from(value.abs()),
        |_| value.to_string(),
        |_| format!("{value:e}"),
    )
}

fn float_to_string(
    value: f64,
    magnitude: f64,
    plain: impl Fn(f64) -> String,
    exponent: impl Fn(f64) -> String,
) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    if magnitude != 0.0 && !(1e-5..1e16).contains(&magnitude) {
        return exponent(value);
    }
    let text = plain(value);
    if text.contains('.') {
        text
    } else {
        format!("{text}.0")
    }
}

/// Duration units, largest first, with their length in nanoseconds.
const UNITS: [(&str, u64); 8] = [
    ("w", 7 * 24 * 60 * 60 * 1_000_000_000),
    ("d", 24 * 60 * 60 * 1_000_000_000),
    ("h", 60 * 60 * 1_000_000_000),
    ("m", 60 * 1_000_000_000),
    ("s", 1_000_000_000),
    ("ms", 1_000_000),
    ("us", 1_000),
    ("ns", 1),
];

/// Formats a duration in nanoseconds the way it would be written as a literal: `1m30s`,
/// `500ms`, `-2h`, `0s`.
pub fn duration_to_string(nanos: i64) -> String {
    if nanos == 0 {
        return "0s".to_owned();
    }
    let mut out = String::new();
    if nanos < 0 {
        out.push('-');
    }
    let mut rest = nanos.unsigned_abs();
    for (unit, size) in UNITS {
        let count = rest / size;
        if count > 0 {
            out.push_str(&count.to_string());
            out.push_str(unit);
            rest %= size;
        }
    }
    out
}

/// Writes a character as it appears inside a literal, escaping where needed. `quote` is the
/// delimiter of the literal, which is escaped too.
fn push_escaped(out: &mut String, c: char, quote: char) {
    match c {
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\0' => out.push_str("\\0"),
        // `$` starts an interpolation in a string literal.
        '$' if quote == '"' => out.push_str("\\$"),
        c if c == quote => {
            out.push('\\');
            out.push(c);
        }
        c if c.is_control() => {
            let _ = write!(out, "\\u{{{:X}}}", u32::from(c));
        }
        c => out.push(c),
    }
}

/// A string as a literal, in double quotes: how strings appear inside structs.
pub fn quote_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        push_escaped(&mut out, c, '"');
    }
    out.push('"');
    out
}

/// A character as a literal, in single quotes: how characters appear inside structs.
pub fn quote_char(c: char) -> String {
    let mut out = String::from("'");
    push_escaped(&mut out, c, '\'');
    out.push('\'');
    out
}

/// The text printed to standard error when a program panics.
pub fn panic_report(message: &str, file: &str, line: u32, column: u32) -> String {
    format!("panic: {message}\n  at {file}:{line}:{column}\n")
}

/// An error raised by a program, as reported: its message and where it was raised. An error
/// made without `:error` has no location: line 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaisedError {
    /// The message.
    pub message: String,
    /// The source file.
    pub file: String,
    /// The 1-based line, or 0.
    pub line: u32,
    /// The 1-based column.
    pub column: u32,
}

/// The report of an error raised by `main`: the error, then each error that caused it, each
/// with the location where it was raised.
pub fn error_report(errors: &[RaisedError]) -> String {
    let mut report = String::new();
    for (index, error) in errors.iter().enumerate() {
        let lead = if index == 0 { "error" } else { "caused by" };
        writeln!(report, "{lead}: {}", error.message).expect("writing to a String");
        if error.line > 0 {
            writeln!(
                report,
                "  at {}:{}:{}",
                error.file, error.line, error.column
            )
            .expect("writing to a String");
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats() {
        assert_eq!(f64_to_string(3.0), "3.0");
        assert_eq!(f64_to_string(-2.5), "-2.5");
        assert_eq!(f64_to_string(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(f64_to_string(0.0), "0.0");
        assert_eq!(f64_to_string(-0.0), "-0.0");
        assert_eq!(f64_to_string(1e300), "1e300");
        assert_eq!(f64_to_string(1.5e-7), "1.5e-7");
        assert_eq!(f64_to_string(123_456.0), "123456.0");
        assert_eq!(f64_to_string(f64::NAN), "NaN");
        assert_eq!(f64_to_string(f64::NEG_INFINITY), "-inf");
        assert_eq!(f32_to_string(0.1), "0.1");
        assert_eq!(f32_to_string(2.0), "2.0");
        assert_eq!(f32_to_string(1e20), "1e20");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_string("plain"), "\"plain\"");
        assert_eq!(
            quote_string("a\"b\\c$d\n\u{1}"),
            "\"a\\\"b\\\\c\\$d\\n\\u{1}\""
        );
        assert_eq!(quote_string("it's é"), "\"it's é\"");
        assert_eq!(quote_char('x'), "'x'");
        assert_eq!(quote_char('\''), "'\\''");
        assert_eq!(quote_char('$'), "'$'");
        assert_eq!(quote_char('\n'), "'\\n'");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_to_string(0), "0s");
        assert_eq!(duration_to_string(90_000_000_000), "1m30s");
        assert_eq!(duration_to_string(500_000_000), "500ms");
        assert_eq!(duration_to_string(-7_200_000_000_000), "-2h");
        assert_eq!(duration_to_string(1_000_000_001), "1s1ns");
        assert_eq!(duration_to_string(8 * 24 * 3_600_000_000_000), "1w1d");
        assert_eq!(
            duration_to_string(i64::MIN),
            "-15250w1d23h47m16s854ms775us808ns"
        );
    }
}
