//! Property tests: the lexer and parser never panic, the tokens always cover the input exactly,
//! and the syntax tree always reproduces the input.

use pika_syntax::{TokenKind, lex};
use proptest::prelude::*;

fn check_invariants(source: &str) {
    let parse = pika_syntax::parse(source);
    assert_eq!(
        parse.syntax().to_string(),
        source,
        "syntax tree is not lossless"
    );
    for diagnostic in parse.diagnostics() {
        let span = diagnostic.primary.span;
        assert!(span.end as usize <= source.len());
        assert!(source.is_char_boundary(span.start as usize));
        assert!(source.is_char_boundary(span.end as usize));
    }

    let lexed = lex(source);
    let tokens = &lexed.tokens;

    let eof = tokens.last().expect("at least the Eof token");
    assert_eq!(eof.kind, TokenKind::Eof);
    assert_eq!(eof.span.start as usize, source.len());
    assert!(eof.span.is_empty());

    let mut expected_start = 0;
    for (i, token) in tokens.iter().enumerate() {
        assert_eq!(
            token.span.start as usize, expected_start,
            "gap or overlap at token {i}"
        );
        assert!(source.is_char_boundary(token.span.end as usize));
        if token.kind != TokenKind::Eof {
            assert!(!token.span.is_empty(), "empty token {token:?}");
        }
        expected_start = token.span.end as usize;

        let after_significant =
            i > 0 && !tokens[i - 1].kind.is_trivia() && tokens[i - 1].kind != TokenKind::Newline;
        let expected_joined =
            after_significant && !token.kind.is_trivia() && token.kind != TokenKind::Eof;
        assert_eq!(
            token.joined, expected_joined,
            "joined flag of token {i}: {token:?}"
        );
    }
    let rebuilt: String = tokens.iter().map(|t| t.text(source)).collect();
    assert_eq!(rebuilt, source);

    if tokens.iter().any(|t| t.kind == TokenKind::Error) {
        assert!(
            !lexed.diagnostics.is_empty(),
            "error token without a diagnostic"
        );
    }
    for diagnostic in &lexed.diagnostics {
        let span = diagnostic.primary.span;
        assert!(span.end as usize <= source.len());
        assert!(source.is_char_boundary(span.start as usize));
        assert!(source.is_char_boundary(span.end as usize));
    }
}

/// Number of cases per property: 4096 by default, or `PROPTEST_CASES` for deeper runs.
fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4096)
}

/// Characters that exercise every lexer path, weighted towards Pika's syntax.
const PIKA_LIKE: &str =
    "[a-zA-Z0-9_ \t\n\r\"'$#\\\\(){}\\[\\]:;,+\\-*/%!~&|^.=<>@?`éß😀\u{FEFF}]{0,120}";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn pika_like_input(source in PIKA_LIKE) {
        check_invariants(&source);
    }

    #[test]
    fn arbitrary_unicode(source in "(?s).{0,120}") {
        check_invariants(&source);
    }

    #[test]
    fn pika_token_soup(source in token_soup()) {
        check_invariants(&source);
    }
}

/// Words and punctuation of Pika, combined at random to stress the parser's error recovery.
const VOCABULARY: &[&str] = &[
    ":local",
    ":const",
    ":global",
    ":set",
    ":if",
    ":while",
    ":do",
    ":for",
    ":foreach",
    ":match",
    ":onerror",
    ":unsafe",
    ":fn",
    ":struct",
    ":enum",
    ":trait",
    ":use",
    ":extern",
    ":test",
    ":put",
    ":return",
    ":",
    "x",
    "Point",
    "Self",
    "self",
    "mut",
    "owned",
    "raises",
    "fn",
    "do",
    "else",
    "in",
    "from",
    "to",
    "impl",
    "$x",
    "$p",
    "/std/math",
    "/",
    "1",
    "-2",
    "1.5",
    "10s",
    "'c'",
    "\"s $x\"",
    "\"$(",
    "r\"raw\"",
    "true",
    "none",
    "some",
    "_",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    "<",
    ">",
    "=",
    "->",
    ",",
    ";",
    "?",
    "+",
    "-",
    "*",
    ".",
    "and",
    "as",
    "i64",
    "List<i64>",
    "\n",
    "\n",
    "# comment\n",
    "\\\n",
];

fn token_soup() -> impl Strategy<Value = String> {
    prop::collection::vec((prop::sample::select(VOCABULARY), prop::bool::ANY), 0..80).prop_map(
        |words| {
            words
                .into_iter()
                .map(|(word, joined)| {
                    if joined {
                        word.to_owned()
                    } else {
                        format!(" {word}")
                    }
                })
                .collect()
        },
    )
}

#[test]
fn edge_cases() {
    for source in [
        "", "\u{FEFF}", "\"", "\"$(", "\"$[", "\"$(\"", "'", "'\\", "\\", "$", "r#", "r#\"", "0x",
        "1e", "1e+", "1.", "\r", "\r\n", "\"\\", "\"\\\r\n", "\"$(()\"", "\"$(]\"",
    ] {
        check_invariants(source);
    }
}
