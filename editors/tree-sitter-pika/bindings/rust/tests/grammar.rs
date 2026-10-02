//! Checks of the tree-sitter grammar against the reference parser in `pika_syntax`, and of the
//! Zed extension's queries and manifests against the grammar.
//!
//! The tree-sitter grammar is more lenient than the reference parser, so the comparison goes
//! one way: whatever the reference parser accepts, the grammar must parse without errors.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use proptest::prelude::*;
use tree_sitter::{Language, Node, Parser, Query, Tree};

fn language() -> Language {
    tree_sitter_pika::LANGUAGE.into()
}

fn parse(source: &str) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&language())
        .expect("the grammar is compatible with the tree-sitter library");
    parser.parse(source, None).expect("parsing succeeds")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn zed_dir() -> PathBuf {
    repo_root().join("editors/zed")
}

/// The first `ERROR` or `MISSING` node in the tree, if any.
fn first_error(node: Node<'_>) -> Option<Node<'_>> {
    if !node.has_error() {
        return None;
    }
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).find_map(first_error)
}

/// Describes the first error in the tree-sitter parse of `source`, if there is one.
fn grammar_error(source: &str) -> Option<String> {
    let tree = parse(source);
    first_error(tree.root_node()).map(|node| {
        let start = node.start_position();
        let line = source.lines().nth(start.row).unwrap_or_default();
        format!(
            "{} at {}:{}\n  {line}\n{}",
            if node.is_missing() {
                format!("missing `{}`", node.kind())
            } else {
                "unexpected input".to_owned()
            },
            start.row + 1,
            start.column + 1,
            node.to_sexp(),
        )
    })
}

/// Every `.pk` file in the repository's sources, tests and standard library.
fn pika_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("directory is readable") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                walk(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "pk") {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    for dir in ["crates", "std", "editors"] {
        walk(&repo_root().join(dir), &mut files);
    }
    files.sort();
    files
}

/// The plain `pika` code blocks of the specification, which must parse without errors.
fn spec_examples() -> Vec<(usize, String)> {
    let spec = std::fs::read_to_string(repo_root().join("docs/spec/v0.md")).expect("spec");
    let mut blocks = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, line) in spec.lines().enumerate() {
        if let Some((_, code)) = &mut current {
            if line.trim().starts_with("```") {
                blocks.push(current.take().expect("inside a block"));
            } else {
                code.push_str(line);
                code.push('\n');
            }
        } else if line.trim() == "```pika" {
            current = Some((index + 2, String::new()));
        }
    }
    blocks
}

#[test]
fn parses_every_file_the_reference_parser_accepts() {
    let mut checked = 0;
    let mut failures = Vec::new();
    for path in pika_files() {
        let source = std::fs::read_to_string(&path).expect("source file is readable");
        if pika_syntax::parse(&source).has_errors() {
            continue;
        }
        checked += 1;
        if let Some(error) = grammar_error(&source) {
            failures.push(format!("{}: {error}", path.display()));
        }
    }
    assert!(
        checked > 80,
        "expected the repository's Pika files, found {checked}"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn parses_the_examples_of_the_specification() {
    let examples = spec_examples();
    assert!(
        examples.len() > 20,
        "expected the spec's examples, found {}",
        examples.len()
    );
    let failures: Vec<String> = examples
        .iter()
        .filter_map(|(line, code)| {
            grammar_error(code).map(|error| format!("spec line {line}: {error}"))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// ----- Zed extension --------------------------------------------------------------------------

/// Highlight names that Zed themes style. A capture may add suffixes (`function.method`), which
/// fall back to the name before them.
const ZED_HIGHLIGHTS: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "constant",
    "constructor",
    "embedded",
    "emphasis",
    "enum",
    "function",
    "hint",
    "keyword",
    "label",
    "link_text",
    "link_uri",
    "none",
    "number",
    "operator",
    "predictive",
    "preproc",
    "primary",
    "property",
    "punctuation",
    "string",
    "tag",
    "text",
    "title",
    "type",
    "variable",
    "variant",
];

/// The captures each query file of a Zed language may use, besides `_`-prefixed ones.
fn allowed_captures(query: &str) -> Option<&'static [&'static str]> {
    Some(match query {
        "brackets" => &["open", "close"],
        "indents" => &["indent", "start", "end", "outdent"],
        "outline" => &[
            "item",
            "name",
            "context",
            "context.extra",
            "annotation",
            "open",
            "close",
        ],
        "runnables" => &["run"],
        "textobjects" => &[
            "function.around",
            "function.inside",
            "class.around",
            "class.inside",
            "comment.around",
            "comment.inside",
        ],
        _ => return None,
    })
}

#[test]
fn zed_queries_compile_and_use_known_captures() {
    let dir = zed_dir().join("languages/pika");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("language directory") {
        let path = entry.expect("directory entry").path();
        if path.extension().is_none_or(|extension| extension != "scm") {
            continue;
        }
        let name = path
            .file_stem()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let source = std::fs::read_to_string(&path).expect("query is readable");
        let query = Query::new(&language(), &source)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        for capture in query.capture_names() {
            if capture.starts_with('_') {
                continue;
            }
            let known = match (name.as_str(), allowed_captures(&name)) {
                ("highlights", _) => {
                    ZED_HIGHLIGHTS.contains(&capture.split('.').next().expect("a segment"))
                }
                (_, Some(allowed)) => allowed.contains(capture),
                // Override scopes are named freely.
                (_, None) => true,
            };
            assert!(known, "{}: unknown capture `@{capture}`", path.display());
        }
        checked += 1;
    }
    assert!(
        checked >= 7,
        "expected the extension's queries, found {checked}"
    );
}

#[test]
fn zed_manifests_refer_to_this_grammar() {
    let read = |path: &Path| -> toml::Table {
        let text = std::fs::read_to_string(path).expect("manifest is readable");
        text.parse()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    };
    let extension = read(&zed_dir().join("extension.toml"));
    let grammar = &extension["grammars"]["pika"];
    let grammar_dir = repo_root().join(grammar["path"].as_str().expect("grammar path"));
    assert!(
        grammar_dir.join("src/parser.c").is_file() && grammar_dir.join("src/scanner.c").is_file(),
        "`grammars.pika.path` must name the directory of this grammar",
    );
    let config = read(&zed_dir().join("languages/pika/config.toml"));
    assert_eq!(config["grammar"].as_str(), Some("pika"));
    assert_eq!(config["path_suffixes"].as_array().map(Vec::len), Some(1));
    assert_eq!(config["path_suffixes"][0].as_str(), Some("pk"));
}

// ----- Properties -----------------------------------------------------------------------------

/// Lines of the repository's Pika files that the reference parser accepts on their own.
fn corpus_lines() -> &'static [String] {
    static LINES: OnceLock<Vec<String>> = OnceLock::new();
    LINES.get_or_init(|| {
        let mut lines: Vec<String> = pika_files()
            .iter()
            .flat_map(|path| {
                let source = std::fs::read_to_string(path).expect("source file is readable");
                source.lines().map(str::to_owned).collect::<Vec<_>>()
            })
            .filter(|line| !line.trim().is_empty() && !pika_syntax::parse(line).has_errors())
            .collect();
        lines.sort();
        lines.dedup();
        lines
    })
}

/// Builds a program from corpus lines; a `true` flag starts a new block around the lines that
/// follow it, so that statements are also checked inside braces.
fn program(lines: &[String], picks: &[(prop::sample::Index, bool)]) -> String {
    let mut source = String::new();
    let mut in_block = false;
    for (index, (line, starts_block)) in picks.iter().enumerate() {
        if *starts_block {
            if in_block {
                source.push_str("}\n");
            }
            writeln!(source, ":fn f{index} p:i64 -> i64 do={{").expect("writing to a String");
            in_block = true;
        }
        source.push_str(line.get(lines));
        source.push('\n');
    }
    if in_block {
        source.push_str("}\n");
    }
    source
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|cases| cases.parse().ok()).unwrap_or(4096),
    ))]

    /// Programs made of valid lines parse without errors whenever the reference parser accepts
    /// them.
    #[test]
    fn agrees_with_the_reference_parser(
        picks in prop::collection::vec((any::<prop::sample::Index>(), prop::bool::weighted(0.1)), 1..40),
    ) {
        let source = program(corpus_lines(), &picks);
        if !pika_syntax::parse(&source).has_errors() {
            let error = grammar_error(&source);
            prop_assert!(error.is_none(), "{}\n\n{source}", error.unwrap_or_default());
        }
    }

    /// Any input, valid or not, parses to a tree that covers it, without hanging or crashing.
    #[test]
    fn parses_any_input(source in r#"(:?[a-z]{1,4}|\$\w+|[0-9]+[a-z]*|[ \t\r\n]|["'#\\$(){}\[\]<>=;,.:/?\-+*!~&|^%]|r#*"|\$[(\[]){0,80}"#) {
        let tree = parse(&source);
        let root = tree.root_node();
        prop_assert!(root.end_byte() <= source.len());
    }
}
