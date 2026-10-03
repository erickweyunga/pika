//! Every Pika file of the repository that parses is formatted: the formatter must keep its
//! meaning (which `pika_fmt::format` verifies) and give the same result when run again. The
//! standard library must already be formatted.

use std::path::{Path, PathBuf};

fn find(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir)
        .expect("readable directory")
        .flatten()
    {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if !name.to_string_lossy().starts_with('.') && name != "target" {
                find(&path, files);
            }
        } else if path.extension().is_some_and(|e| e == "pk") {
            files.push(path);
        }
    }
}

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn every_file_formats_and_is_stable() {
    let mut files = Vec::new();
    find(&repository(), &mut files);
    assert!(files.len() > 50, "the corpus is found");
    for file in files {
        let source = std::fs::read_to_string(&file).expect("readable file");
        match pika_fmt::format(&source) {
            Ok(formatted) => {
                let again = pika_fmt::format(&formatted).expect("formatted output formats");
                assert_eq!(formatted, again, "not idempotent: {}", file.display());
            }
            Err(pika_fmt::FormatError::Syntax(_)) => {}
            Err(pika_fmt::FormatError::Changed) => {
                panic!("formatting changes the meaning of {}", file.display())
            }
        }
    }
}

#[test]
fn standard_library_is_formatted() {
    let mut files = Vec::new();
    find(&repository().join("std"), &mut files);
    for file in files {
        let source = std::fs::read_to_string(&file).expect("readable file");
        let formatted = pika_fmt::format(&source).expect("the standard library formats");
        assert_eq!(
            source,
            formatted,
            "run `pika fmt std`: {} is not formatted",
            file.display()
        );
    }
}

/// `source` with the space between tokens changed: each run of spaces or tabs, except before
/// a comment, becomes `widths[i]` spaces (or a tab, for 0), cycling through `widths`, and
/// spaces are added at the end of every other line.
fn respace(source: &str, widths: &[u8]) -> String {
    let lexed = pika_syntax::lex(source);
    let mut out = String::new();
    let mut next = 0;
    for (index, token) in lexed.tokens.iter().enumerate() {
        let text = &source[token.span.start as usize..token.span.end as usize];
        // The space before a comment is kept as written, so it stays.
        let before_comment = lexed.tokens.get(index + 1).is_some_and(|t| {
            matches!(
                t.kind,
                pika_syntax::TokenKind::Comment | pika_syntax::TokenKind::DocComment
            )
        });
        match token.kind {
            pika_syntax::TokenKind::Whitespace if before_comment => out.push_str(text),
            pika_syntax::TokenKind::Whitespace => {
                let width = widths[next % widths.len()];
                next += 1;
                if width == 0 {
                    out.push('\t');
                } else {
                    out.push_str(&" ".repeat(usize::from(width)));
                }
            }
            pika_syntax::TokenKind::Newline => {
                if next % 2 == 0 {
                    out.push_str("  ");
                }
                next += 1;
                out.push_str(text);
            }
            _ => out.push_str(text),
        }
    }
    out
}

#[test]
fn the_layout_does_not_depend_on_space() {
    let mut files = Vec::new();
    find(&repository(), &mut files);
    for (index, file) in files.iter().enumerate() {
        let source = std::fs::read_to_string(file).expect("readable file");
        let Ok(formatted) = pika_fmt::format(&source) else {
            continue;
        };
        let widths: Vec<u8> = (0..7)
            .map(|i| u8::try_from((index + i * 3) % 5).unwrap_or(1))
            .collect();
        let respaced = respace(&source, &widths);
        let reformatted = pika_fmt::format(&respaced)
            .unwrap_or_else(|error| panic!("{} with other space: {error:?}", file.display()));
        assert_eq!(
            formatted,
            reformatted,
            "{} depends on space",
            file.display()
        );
    }
}
