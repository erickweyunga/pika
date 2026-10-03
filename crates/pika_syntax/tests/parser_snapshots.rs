//! Snapshot tests: every `parser/*.pk` file is parsed, and the syntax tree and the rendered
//! diagnostics are compared with the stored snapshot. Files named `ok_*` must parse without
//! diagnostics; files named `err_*` must produce at least one.

use pika_diagnostics::{RenderOptions, SourceMap, render_map};

#[test]
fn parser_snapshots() {
    insta::glob!("parser/*.pk", |path| {
        let source = std::fs::read_to_string(path).expect("readable test input");
        let parse = pika_syntax::parse(&source);
        assert_eq!(parse.syntax().to_string(), source, "tree is not lossless");

        let name = path.file_name().expect("file name").to_string_lossy();
        if name.starts_with("ok_") {
            assert!(
                parse.diagnostics().is_empty(),
                "{name} should parse cleanly: {:#?}",
                parse.diagnostics()
            );
        } else {
            assert!(
                name.starts_with("err_"),
                "test files must start with `ok_` or `err_`"
            );
            assert!(
                !parse.diagnostics().is_empty(),
                "{name} should produce diagnostics"
            );
        }

        let mut snapshot = parse.debug_tree();
        if !parse.diagnostics().is_empty() {
            snapshot.push_str("\n--- diagnostics ---\n");
            let mut map = SourceMap::default();
            pika_syntax::add_file(&mut map, name.as_ref(), source.as_str());
            snapshot.push_str(&render_map(
                parse.diagnostics(),
                &map,
                RenderOptions::default(),
            ));
        }
        insta::assert_snapshot!(snapshot);
    });
}
