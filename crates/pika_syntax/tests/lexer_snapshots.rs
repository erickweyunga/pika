//! Snapshot tests: every `lexer/*.pk` file is lexed, and the tokens (including trivia) and the
//! rendered diagnostics are compared with the stored snapshot.

use pika_diagnostics::{RenderOptions, render};

#[test]
fn lexer_snapshots() {
    insta::glob!("lexer/*.pk", |path| {
        let source = std::fs::read_to_string(path).expect("readable test input");
        let lexed = pika_syntax::lex(&source);
        let mut snapshot = pika_syntax::dump_tokens(&source, &lexed.tokens, true);
        if !lexed.diagnostics.is_empty() {
            let name = path.file_name().expect("file name").to_string_lossy();
            snapshot.push_str("\n--- diagnostics ---\n");
            snapshot.push_str(&render(
                &lexed.diagnostics,
                &name,
                &source,
                RenderOptions::default(),
            ));
        }
        insta::assert_snapshot!(snapshot);
    });
}
