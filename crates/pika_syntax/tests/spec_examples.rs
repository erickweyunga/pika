//! Conformance with the language specification: every `pika` code block in it is checked.
//!
//! The info string after `pika` says what is expected of a block:
//!
//! - `pika`: a complete program fragment that lexes and parses without diagnostics.
//! - `pika syntax-error`: lexes cleanly but demonstrates a syntax error, so parsing fails.
//! - `pika fragment`: not a sequence of statements (a list of literals, a grammar template), so
//!   it is only required to lex cleanly.
//!
//! Examples of type or ownership errors are plain `pika` blocks: they are syntactically valid.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expectation {
    Parses,
    SyntaxError,
    Fragment,
}

struct Block {
    line: usize,
    expectation: Expectation,
    code: String,
}

fn spec_blocks() -> Vec<Block> {
    let spec_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/v0.md");
    let spec = std::fs::read_to_string(&spec_path).expect("spec is readable");
    let mut blocks = Vec::new();
    let mut current: Option<Block> = None;
    for (index, line) in spec.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(block) = &mut current {
            if trimmed.starts_with("```") {
                blocks.push(current.take().expect("inside a block"));
            } else {
                block.code.push_str(line);
                block.code.push('\n');
            }
            continue;
        }
        let expectation = match trimmed {
            "```pika" => Expectation::Parses,
            "```pika syntax-error" => Expectation::SyntaxError,
            "```pika fragment" => Expectation::Fragment,
            other if other.starts_with("```pika") => {
                panic!("line {}: unknown info string `{other}`", index + 1)
            }
            _ => continue,
        };
        current = Some(Block {
            line: index + 2,
            expectation,
            code: String::new(),
        });
    }
    assert!(current.is_none(), "unterminated code block in the spec");
    blocks
}

#[test]
fn spec_examples() {
    let blocks = spec_blocks();
    assert!(
        blocks.len() > 20,
        "expected the spec's examples, found {}",
        blocks.len()
    );

    let mut failures = Vec::new();
    for block in &blocks {
        let lexed = pika_syntax::lex(&block.code);
        if !lexed.diagnostics.is_empty() {
            failures.push(format!(
                "line {}: lexer errors: {:#?}",
                block.line, lexed.diagnostics
            ));
            continue;
        }
        let parse = pika_syntax::parse(&block.code);
        assert_eq!(
            parse.syntax().to_string(),
            block.code,
            "tree is not lossless"
        );
        match block.expectation {
            Expectation::Parses if parse.has_errors() => failures.push(format!(
                "line {}: unexpected syntax errors: {:#?}",
                block.line,
                parse.diagnostics()
            )),
            Expectation::SyntaxError if !parse.has_errors() => failures.push(format!(
                "line {}: block is marked `syntax-error` but parses cleanly",
                block.line
            )),
            _ => {}
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
