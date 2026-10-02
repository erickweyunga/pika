//! Decoding of literal token text into values.
//!
//! The lexer uses these functions to validate literals as it produces them. Later phases use them
//! to obtain the values. Each function receives the exact text of a token (or of a string piece)
//! and reports problems with byte ranges relative to the start of that text.

use std::ops::Range;

use crate::codes;

/// A problem found while decoding a literal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiteralError {
    /// Diagnostic code from [`crate::codes`].
    pub code: &'static str,
    /// Byte range of the problem, relative to the start of the decoded text.
    pub range: Range<usize>,
    /// One-line description of the problem.
    pub message: String,
    /// Optional advice on how to fix it.
    pub help: Option<String>,
}

impl LiteralError {
    fn new(code: &'static str, range: Range<usize>, message: impl Into<String>) -> Self {
        Self {
            code,
            range,
            message: message.into(),
            help: None,
        }
    }

    fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

/// Decodes an integer literal such as `42`, `1_000`, `0xFF`, `0o755` or `0b1010`.
///
/// # Errors
///
/// Returns an error if the literal has no digits, an invalid digit, a trailing `_`, or does not
/// fit in a `u64`.
pub fn parse_int(text: &str) -> Result<u64, LiteralError> {
    let (radix, prefix_len, radix_name) = match text.get(..2) {
        Some("0x") => (16, 2, "hexadecimal"),
        Some("0o") => (8, 2, "octal"),
        Some("0b") => (2, 2, "binary"),
        _ => (10, 0, "decimal"),
    };
    let body = &text[prefix_len..];
    if !body.bytes().any(|b| b != b'_') {
        return Err(LiteralError::new(
            codes::INVALID_INT,
            0..text.len(),
            format!("missing digits after `{}`", &text[..prefix_len]),
        ));
    }
    check_trailing_underscore(text, codes::INVALID_INT)?;
    let mut value: u64 = 0;
    for (i, c) in body.char_indices() {
        if c == '_' {
            continue;
        }
        let Some(digit) = c.to_digit(radix) else {
            let at = prefix_len + i;
            return Err(LiteralError::new(
                codes::INVALID_INT,
                at..at + c.len_utf8(),
                format!("invalid digit `{c}` in {radix_name} literal"),
            ));
        };
        value = value
            .checked_mul(u64::from(radix))
            .and_then(|v| v.checked_add(u64::from(digit)))
            .ok_or_else(|| {
                LiteralError::new(
                    codes::INT_TOO_LARGE,
                    0..text.len(),
                    "integer literal is too large",
                )
                .with_help(format!("the largest integer literal is {}", u64::MAX))
            })?;
    }
    Ok(value)
}

/// Decodes a float literal such as `3.14`, `1e9`, `2.5e-3` or `1_000.5`.
///
/// # Errors
///
/// Returns an error if a digit group ends with `_` or the value is out of range for `f64`.
pub fn parse_float(text: &str) -> Result<f64, LiteralError> {
    for (i, b) in text.bytes().enumerate() {
        if b == b'_' && !text.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit) {
            return Err(LiteralError::new(
                codes::INVALID_FLOAT,
                i..i + 1,
                "`_` must be followed by a digit",
            ));
        }
    }
    let cleaned: String = text.chars().filter(|&c| c != '_').collect();
    match cleaned.parse::<f64>() {
        Ok(value) if value.is_finite() => Ok(value),
        Ok(_) => Err(LiteralError::new(
            codes::INVALID_FLOAT,
            0..text.len(),
            "float literal is out of range",
        )
        .with_help(format!("the largest float is {:e}", f64::MAX))),
        Err(_) => Err(LiteralError::new(
            codes::INVALID_FLOAT,
            0..text.len(),
            "invalid float literal",
        )),
    }
}

/// Duration units, largest first, with their length in nanoseconds.
const DURATION_UNITS: [(&str, i64); 8] = [
    ("w", 7 * 24 * 60 * 60 * 1_000_000_000),
    ("d", 24 * 60 * 60 * 1_000_000_000),
    ("h", 60 * 60 * 1_000_000_000),
    ("m", 60 * 1_000_000_000),
    ("s", 1_000_000_000),
    ("ms", 1_000_000),
    ("us", 1_000),
    ("ns", 1),
];

/// Returns true if `text` starts with the name of a duration unit.
pub(crate) fn starts_with_duration_unit(text: &str) -> bool {
    let letters: &str = &text[..text
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(text.len())];
    DURATION_UNITS.iter().any(|(name, _)| *name == letters)
}

/// Decodes a duration literal such as `500ms` or `1h30m` into nanoseconds.
///
/// A duration is one or more groups of digits followed by a unit (`w d h m s ms us ns`). Units
/// must appear in descending order, each at most once.
///
/// # Errors
///
/// Returns an error with code [`codes::INVALID_SUFFIX`] if the letters after the first number
/// are not a duration unit (for example `10i32`), and [`codes::INVALID_DURATION`] for other
/// problems.
pub fn parse_duration(text: &str) -> Result<i64, LiteralError> {
    let bytes = text.as_bytes();
    let mut pos = 0;
    let mut total: i64 = 0;
    let mut previous_rank: Option<usize> = None;
    while pos < bytes.len() {
        let digits_start = pos;
        while pos < bytes.len() && (bytes[pos].is_ascii_digit() || bytes[pos] == b'_') {
            pos += 1;
        }
        let digits = &text[digits_start..pos];
        let unit_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_alphabetic() {
            pos += 1;
        }
        let unit = &text[unit_start..pos];

        if !digits.starts_with(|c: char| c.is_ascii_digit()) {
            // A `_` directly after a unit, as in `1s_`. The lexer guarantees the literal starts
            // with a digit and contains only letters, digits and `_`.
            return Err(LiteralError::new(
                codes::INVALID_DURATION,
                digits_start..digits_start + 1,
                "`_` must be followed by a digit",
            ));
        }
        let Some(rank) = DURATION_UNITS.iter().position(|(name, _)| *name == unit) else {
            if previous_rank.is_none() {
                let suffix = &text[unit_start..];
                return Err(LiteralError::new(
                    codes::INVALID_SUFFIX,
                    unit_start..text.len(),
                    format!("invalid suffix `{suffix}` on number literal"),
                )
                .with_help(
                    "number literals have no type suffixes; convert with `as`, for example \
                     `(10 as i32)`. Duration units are `w d h m s ms us ns`",
                ));
            }
            if unit.is_empty() {
                return Err(LiteralError::new(
                    codes::INVALID_DURATION,
                    digits_start..pos,
                    format!("missing unit after `{digits}` in duration literal"),
                )
                .with_help("every number in a duration needs a unit, for example `1m30s`"));
            }
            return Err(LiteralError::new(
                codes::INVALID_DURATION,
                unit_start..pos,
                format!("unknown duration unit `{unit}`"),
            )
            .with_help("duration units are `w d h m s ms us ns`"));
        };
        if previous_rank.is_some_and(|previous| rank <= previous) {
            return Err(LiteralError::new(
                codes::INVALID_DURATION,
                unit_start..pos,
                "duration units must be in descending order and appear at most once",
            )
            .with_help("for example `1h30m`, not `30m1h`"));
        }
        previous_rank = Some(rank);

        let value = parse_int(digits).map_err(|mut error| {
            error.range = error.range.start + digits_start..error.range.end + digits_start;
            if error.code == codes::INT_TOO_LARGE {
                error = duration_too_large(text);
            }
            error
        })?;
        total = i64::try_from(value)
            .ok()
            .and_then(|v| v.checked_mul(DURATION_UNITS[rank].1))
            .and_then(|v| v.checked_add(total))
            .ok_or_else(|| duration_too_large(text))?;
    }
    Ok(total)
}

fn duration_too_large(text: &str) -> LiteralError {
    LiteralError::new(
        codes::INVALID_DURATION,
        0..text.len(),
        "duration literal is too large",
    )
    .with_help("durations are limited to about 292 years")
}

fn check_trailing_underscore(text: &str, code: &'static str) -> Result<(), LiteralError> {
    if text.ends_with('_') {
        let end = text.len();
        return Err(LiteralError::new(
            code,
            end - 1..end,
            "number literals cannot end with `_`",
        ));
    }
    Ok(())
}

/// Decodes the text of a string piece (between quotes and interpolations), resolving escape
/// sequences and normalizing line breaks to `\n`.
///
/// Decoding continues past errors so that every problem is reported. The returned string is only
/// meaningful when no errors are returned.
pub fn unescape(text: &str) -> (String, Vec<LiteralError>) {
    let mut out = String::with_capacity(text.len());
    let mut errors = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        match c {
            '\r' => {
                chars.next_if(|&(_, c)| c == '\n');
                out.push('\n');
            }
            '\\' => match chars.next() {
                None => errors.push(LiteralError::new(
                    codes::INVALID_ESCAPE,
                    start..start + 1,
                    "incomplete escape sequence",
                )),
                Some((_, escape)) => match escape {
                    '"' => out.push('"'),
                    '\'' => out.push('\''),
                    '\\' => out.push('\\'),
                    '$' => out.push('$'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '0' => out.push('\0'),
                    '\n' | '\r' => {
                        if escape == '\r' {
                            chars.next_if(|&(_, c)| c == '\n');
                        }
                        while chars.next_if(|&(_, c)| c == ' ' || c == '\t').is_some() {}
                    }
                    'x' => match hex_escape(text, start, &mut chars) {
                        Ok(c) => out.push(c),
                        Err(error) => errors.push(error),
                    },
                    'u' => match unicode_escape(text, start, &mut chars) {
                        Ok(c) => out.push(c),
                        Err(error) => errors.push(error),
                    },
                    other => errors.push(
                        LiteralError::new(
                            codes::INVALID_ESCAPE,
                            start..start + 1 + other.len_utf8(),
                            format!("unknown escape sequence `\\{other}`"),
                        )
                        .with_help(
                            "valid escapes are \\\" \\' \\\\ \\$ \\n \\r \\t \\0 \\xHH \\u{...}",
                        ),
                    ),
                },
            },
            c => out.push(c),
        }
    }
    (out, errors)
}

type CharIter<'a> = std::iter::Peekable<std::str::CharIndices<'a>>;

/// Decodes `\xHH` after the `x`. `start` is the offset of the backslash.
fn hex_escape(text: &str, start: usize, chars: &mut CharIter<'_>) -> Result<char, LiteralError> {
    let mut value = 0;
    for _ in 0..2 {
        let Some((_, c)) = chars.next_if(|(_, c)| c.is_ascii_hexdigit()) else {
            let end = chars.peek().map_or(text.len(), |&(i, _)| i);
            return Err(LiteralError::new(
                codes::INVALID_ESCAPE,
                start..end,
                "`\\x` must be followed by two hexadecimal digits",
            ));
        };
        value = value * 16 + c.to_digit(16).expect("checked hex digit");
    }
    let end = chars.peek().map_or(text.len(), |&(i, _)| i);
    if value > 0x7F {
        return Err(LiteralError::new(
            codes::INVALID_ESCAPE,
            start..end,
            format!("`\\x{value:02X}` is not an ASCII character"),
        )
        .with_help(format!(
            "use `\\u{{{value:X}}}` for the Unicode character U+{value:04X}"
        )));
    }
    Ok(char::from_u32(value).expect("ASCII is a valid char"))
}

/// Decodes `\u{H...}` after the `u`. `start` is the offset of the backslash.
fn unicode_escape(
    text: &str,
    start: usize,
    chars: &mut CharIter<'_>,
) -> Result<char, LiteralError> {
    let malformed = |end: usize| {
        LiteralError::new(
            codes::INVALID_ESCAPE,
            start..end,
            "malformed Unicode escape",
        )
        .with_help("write Unicode escapes as `\\u{1F600}` with 1 to 6 hexadecimal digits")
    };
    let offset_now = |chars: &mut CharIter<'_>| chars.peek().map_or(text.len(), |&(i, _)| i);
    if chars.next_if(|&(_, c)| c == '{').is_none() {
        return Err(malformed(offset_now(chars)));
    }
    let mut value: u32 = 0;
    let mut digits = 0;
    while let Some((_, c)) = chars.next_if(|(_, c)| c.is_ascii_hexdigit()) {
        digits += 1;
        if digits <= 6 {
            value = value * 16 + c.to_digit(16).expect("checked hex digit");
        }
    }
    if chars.next_if(|&(_, c)| c == '}').is_none() || digits == 0 || digits > 6 {
        return Err(malformed(offset_now(chars)));
    }
    let end = offset_now(chars);
    char::from_u32(value).ok_or_else(|| {
        LiteralError::new(
            codes::INVALID_ESCAPE,
            start..end,
            format!("U+{value:X} is not a valid Unicode scalar value"),
        )
    })
}

/// Decodes a character literal, including its quotes, such as `'a'` or `'\n'`.
///
/// # Errors
///
/// Returns the first error if the literal is malformed or does not contain exactly one
/// character.
pub fn parse_char(text: &str) -> Result<char, LiteralError> {
    let inner = text
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .filter(|_| text.len() >= 2)
        .ok_or_else(|| {
            LiteralError::new(
                codes::UNTERMINATED_CHAR,
                0..text.len(),
                "unterminated character literal",
            )
        })?;
    let (value, errors) = unescape(inner);
    if let Some(mut error) = errors.into_iter().next() {
        error.range = error.range.start + 1..error.range.end + 1;
        return Err(error);
    }
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(c),
        (None, _) => Err(LiteralError::new(
            codes::INVALID_CHAR,
            0..text.len(),
            "empty character literal",
        )),
        (Some(_), Some(_)) => Err(LiteralError::new(
            codes::INVALID_CHAR,
            0..text.len(),
            "character literal must contain exactly one character",
        )
        .with_help("use double quotes for strings")),
    }
}

/// Returns the value of a raw string literal, including its `r`, hashes and quotes.
///
/// Line breaks are normalized to `\n`. The text must be a complete raw string token.
pub fn raw_string_value(text: &str) -> String {
    let hashes = text[1..].bytes().take_while(|&b| b == b'#').count();
    let content = &text[1 + hashes + 1..text.len() - 1 - hashes];
    content.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints() {
        assert_eq!(parse_int("0"), Ok(0));
        assert_eq!(parse_int("42"), Ok(42));
        assert_eq!(parse_int("1_000_000"), Ok(1_000_000));
        assert_eq!(parse_int("0xFF"), Ok(255));
        assert_eq!(parse_int("0xff_ff"), Ok(0xFFFF));
        assert_eq!(parse_int("0o755"), Ok(0o755));
        assert_eq!(parse_int("0b1010"), Ok(10));
        assert_eq!(parse_int("0x_1"), Ok(1));
        assert_eq!(parse_int("007"), Ok(7));
        assert_eq!(parse_int("18446744073709551615"), Ok(u64::MAX));
    }

    #[test]
    fn int_errors() {
        let err = parse_int("18446744073709551616").unwrap_err();
        assert_eq!(err.code, codes::INT_TOO_LARGE);
        let err = parse_int("0x").unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            (codes::INVALID_INT, "missing digits after `0x`")
        );
        let err = parse_int("0x_").unwrap_err();
        assert_eq!(err.code, codes::INVALID_INT);
        let err = parse_int("0b102").unwrap_err();
        assert_eq!(err.range, 4..5);
        assert_eq!(err.message, "invalid digit `2` in binary literal");
        let err = parse_int("1_").unwrap_err();
        assert_eq!(err.range, 1..2);
        let err = parse_int("0xFG").unwrap_err();
        assert_eq!(err.message, "invalid digit `G` in hexadecimal literal");
    }

    #[test]
    fn floats() {
        assert_eq!(parse_float("2.75"), Ok(2.75));
        assert_eq!(parse_float("1e9"), Ok(1e9));
        assert_eq!(parse_float("2.5e-3"), Ok(2.5e-3));
        assert_eq!(parse_float("1E+2"), Ok(100.0));
        assert_eq!(parse_float("1_000.5"), Ok(1000.5));
        assert_eq!(parse_float("1_000.000_1"), Ok(1000.0001));
    }

    #[test]
    fn float_errors() {
        assert_eq!(
            parse_float("1e999").unwrap_err().message,
            "float literal is out of range"
        );
        let err = parse_float("1_.5").unwrap_err();
        assert_eq!(err.range, 1..2);
        assert_eq!(parse_float("1.5_").unwrap_err().range, 3..4);
        assert_eq!(parse_float("1_e5").unwrap_err().range, 1..2);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms"), Ok(500_000_000));
        assert_eq!(parse_duration("10s"), Ok(10_000_000_000));
        assert_eq!(parse_duration("1m30s"), Ok(90_000_000_000));
        assert_eq!(parse_duration("2h"), Ok(7_200_000_000_000));
        assert_eq!(parse_duration("1d12h"), Ok(36 * 3_600_000_000_000));
        assert_eq!(parse_duration("1w"), Ok(7 * 24 * 3_600_000_000_000));
        assert_eq!(parse_duration("1s500ms"), Ok(1_500_000_000));
        assert_eq!(parse_duration("3us"), Ok(3_000));
        assert_eq!(parse_duration("7ns"), Ok(7));
        assert_eq!(parse_duration("1_000ms"), Ok(1_000_000_000));
        assert_eq!(parse_duration("0s"), Ok(0));
    }

    #[test]
    fn duration_errors() {
        let err = parse_duration("10i32").unwrap_err();
        assert_eq!(err.code, codes::INVALID_SUFFIX);
        assert_eq!(err.message, "invalid suffix `i32` on number literal");
        assert_eq!(err.range, 2..5);

        let err = parse_duration("30m1h").unwrap_err();
        assert_eq!(err.code, codes::INVALID_DURATION);
        assert_eq!(err.range, 4..5);

        let err = parse_duration("1s1s").unwrap_err();
        assert_eq!(err.code, codes::INVALID_DURATION);

        let err = parse_duration("1h30").unwrap_err();
        assert_eq!(err.message, "missing unit after `30` in duration literal");
        assert_eq!(err.range, 2..4);

        let err = parse_duration("1h30x").unwrap_err();
        assert_eq!(err.message, "unknown duration unit `x`");
        assert_eq!(err.range, 4..5);

        let err = parse_duration("1s_").unwrap_err();
        assert_eq!(err.code, codes::INVALID_DURATION);

        let err = parse_duration("1_s").unwrap_err();
        assert_eq!(err.message, "number literals cannot end with `_`");

        let err = parse_duration("99999999999w").unwrap_err();
        assert_eq!(err.message, "duration literal is too large");
        let err = parse_duration("99999999999999999999999s").unwrap_err();
        assert_eq!(err.message, "duration literal is too large");
    }

    #[test]
    fn escapes() {
        let (s, errors) = unescape(r#"a\"b\\c\$d\n\r\t\0\'"#);
        assert!(errors.is_empty());
        assert_eq!(s, "a\"b\\c$d\n\r\t\0'");

        let (s, errors) = unescape(r"\x41\u{1F600}\u{e9}");
        assert!(errors.is_empty());
        assert_eq!(s, "A\u{1F600}\u{e9}");

        let (s, errors) = unescape("line one \\\n    line two");
        assert!(errors.is_empty());
        assert_eq!(s, "line one line two");

        let (s, errors) = unescape("a\r\nb\rc");
        assert!(errors.is_empty());
        assert_eq!(s, "a\nb\nc");
    }

    #[test]
    fn escape_errors() {
        let (_, errors) = unescape(r"\q and \x4 and \x80 and \u{110000} and \u41 and \u{}");
        let messages: Vec<_> = errors
            .iter()
            .map(|e| (e.range.clone(), e.message.as_str()))
            .collect();
        assert_eq!(
            messages,
            vec![
                (0..2, "unknown escape sequence `\\q`"),
                (7..10, "`\\x` must be followed by two hexadecimal digits"),
                (15..19, "`\\x80` is not an ASCII character"),
                (24..34, "U+110000 is not a valid Unicode scalar value"),
                (39..41, "malformed Unicode escape"),
                (48..52, "malformed Unicode escape"),
            ]
        );
        assert_eq!(
            unescape(r"\u{1234567}").1[0].message,
            "malformed Unicode escape"
        );
    }

    #[test]
    fn chars() {
        assert_eq!(parse_char("'a'"), Ok('a'));
        assert_eq!(parse_char(r"'\n'"), Ok('\n'));
        assert_eq!(parse_char(r"'\''"), Ok('\''));
        assert_eq!(parse_char(r"'\u{1F600}'"), Ok('\u{1F600}'));
        assert_eq!(parse_char("'é'"), Ok('é'));
        assert_eq!(
            parse_char("''").unwrap_err().message,
            "empty character literal"
        );
        assert_eq!(parse_char("'ab'").unwrap_err().code, codes::INVALID_CHAR);
        assert_eq!(parse_char("'a").unwrap_err().code, codes::UNTERMINATED_CHAR);
        assert_eq!(parse_char("'").unwrap_err().code, codes::UNTERMINATED_CHAR);
        assert_eq!(parse_char(r"'\q'").unwrap_err().range, 1..3);
    }

    #[test]
    fn raw_strings() {
        assert_eq!(raw_string_value(r#"r"a\nb""#), r"a\nb");
        assert_eq!(raw_string_value("r##\"say \"hi\"#\"##"), "say \"hi\"#");
        assert_eq!(raw_string_value("r\"a\r\nb\""), "a\nb");
    }
}
