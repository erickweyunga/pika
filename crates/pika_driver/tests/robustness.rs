//! Property tests: analysis never panics, and every diagnostic points inside the source.
//!
//! Random programs are built from lines of the test corpus that parse on their own, some of them
//! wrapped in function bodies. This yields well-formed syntax, so name resolution and type
//! checking actually run, with arbitrary combinations of declarations, scopes and types.

use std::fmt::Write;
use std::path::Path;
use std::sync::OnceLock;

use proptest::prelude::*;

fn corpus_lines() -> &'static [String] {
    static LINES: OnceLock<Vec<String>> = OnceLock::new();
    LINES.get_or_init(load_corpus_lines)
}

fn load_corpus_lines() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/check");
    let mut lines = Vec::new();
    for entry in std::fs::read_dir(dir).expect("corpus directory") {
        let source = std::fs::read_to_string(entry.expect("entry").path()).expect("readable");
        lines.extend(
            source
                .lines()
                .filter(|line| !pika_syntax::parse(line).has_errors())
                .map(str::to_owned),
        );
    }
    lines.sort();
    lines.dedup();
    lines
}

/// Builds a program from corpus lines; a `true` flag starts a new function around the lines
/// that follow it.
fn program(lines: &[String], picks: &[(prop::sample::Index, bool)]) -> String {
    let mut source = String::new();
    let mut in_function = false;
    for (index, (line, starts_function)) in picks.iter().enumerate() {
        if *starts_function {
            if in_function {
                source.push_str("}\n");
            }
            writeln!(source, ":fn f{index} p:i64 -> i64 do={{").expect("writing to a String");
            in_function = true;
        }
        source.push_str(line.get(lines));
        source.push('\n');
    }
    if in_function {
        source.push_str("}\n");
    }
    source
}

fn check_invariants(source: &str) {
    let analysis = pika_driver::check_source(source);
    let map = &analysis.sources.map;
    for diagnostic in &analysis.diagnostics {
        for span in std::iter::once(diagnostic.primary.span)
            .chain(diagnostic.secondary.iter().map(|label| label.span))
        {
            // Each span is within one file of the program, at boundaries of its characters.
            let (file, local) = map.local(span);
            let text = &map.file(file).text;
            assert!(local.end as usize <= text.len(), "{diagnostic:?}");
            assert!(text.is_char_boundary(local.start as usize));
            assert!(text.is_char_boundary(local.end as usize));
        }
    }
    // Describing the types must not panic either.
    pika_driver::describe_types(&analysis);
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2048)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn spliced_corpus_lines(
        picks in prop::collection::vec((any::<prop::sample::Index>(), prop::bool::weighted(0.15)), 0..40)
    ) {
        let source = program(corpus_lines(), &picks);
        check_invariants(&source);
    }

    #[test]
    fn arbitrary_text(source in "(?s).{0,200}") {
        check_invariants(&source);
    }
}
