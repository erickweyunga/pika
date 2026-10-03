//! Reports show source files without their comments.

use pika_diagnostics::{Diagnostic, RenderOptions, SourceMap, Span, render_map};

#[test]
fn comments_are_left_out_of_reports() {
    let text = "## Doubles.\n:local s \"a # b\"   # the note\n:put 'é' # ünïcode\n";
    let mut map = SourceMap::default();
    let file = pika_syntax::add_file(&mut map, "main.pk", text);
    let shown = map.file(file).shown();
    assert_eq!(shown.len(), text.len());
    let lines: Vec<&str> = shown.lines().map(str::trim_end).collect();
    assert_eq!(lines, ["", ":local s \"a # b\"", ":put 'é'"]);
}

#[test]
fn rendered_snippets_have_no_comments() {
    let text = ":local a 1   # one\n:put $b      # use it\n";
    let mut map = SourceMap::default();
    pika_syntax::add_file(&mut map, "main.pk", text);
    let start = u32::try_from(text.find("$b").unwrap()).unwrap();
    let diagnostic = Diagnostic::error("E0000", "unknown variable", Span::new(start, start + 2));
    let rendered = render_map(&[diagnostic], &map, RenderOptions::default());
    assert!(rendered.contains(":put $b\n"), "{rendered}");
    assert!(!rendered.contains("use it"), "{rendered}");
}
