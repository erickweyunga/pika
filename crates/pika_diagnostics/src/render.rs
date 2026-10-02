use ariadne::{Config, IndexType, Label as AriadneLabel, Report, ReportKind};

use crate::{Diagnostic, Severity, SourceMap, Span};

/// Options controlling how diagnostics are rendered.
#[derive(Clone, Copy, Debug, Default)]
pub struct RenderOptions {
    /// Emit ANSI color codes.
    pub color: bool,
}

/// Renders diagnostics for one source file as human-readable text.
///
/// # Panics
///
/// Panics if a diagnostic's span lies outside `source`.
pub fn render(
    diagnostics: &[Diagnostic],
    file_name: &str,
    source: &str,
    options: RenderOptions,
) -> String {
    let mut map = SourceMap::default();
    map.add(file_name, source);
    render_map(diagnostics, &map, options)
}

/// Renders diagnostics about the files of `map` as human-readable text. Each label is shown
/// in its own file.
///
/// # Panics
///
/// Panics if a diagnostic's span lies outside the files of `map`.
pub fn render_map(diagnostics: &[Diagnostic], map: &SourceMap, options: RenderOptions) -> String {
    let mut out = Vec::new();
    let mut cache = ariadne::sources(
        map.files()
            .map(|(_, file)| (file.name.clone(), file.text.clone())),
    );
    let place = |span: Span| {
        let (file, local) = map.local(span);
        (map.file(file).name.clone(), local.range())
    };
    for diagnostic in diagnostics {
        let kind = match diagnostic.severity {
            Severity::Error => ReportKind::Error,
            Severity::Warning => ReportKind::Warning,
        };
        let config = Config::default()
            .with_index_type(IndexType::Byte)
            .with_color(options.color);
        let mut report = Report::build(kind, place(diagnostic.primary.span))
            .with_config(config)
            .with_code(diagnostic.code)
            .with_message(&diagnostic.message);
        // Ariadne only underlines labels that have a message, so an unlabeled primary span
        // repeats the diagnostic's message.
        let primary_message = if diagnostic.primary.message.is_empty() {
            &diagnostic.message
        } else {
            &diagnostic.primary.message
        };
        report = report.with_label(
            AriadneLabel::new(place(diagnostic.primary.span))
                .with_message(primary_message)
                .with_order(0),
        );
        for (order, label) in (1..).zip(&diagnostic.secondary) {
            report = report.with_label(
                AriadneLabel::new(place(label.span))
                    .with_message(&label.message)
                    .with_order(order),
            );
        }
        if let Some(help) = &diagnostic.help {
            report = report.with_help(help);
        }
        report
            .finish()
            .write(&mut cache, &mut out)
            .expect("writing to a Vec cannot fail");
    }
    String::from_utf8(out).expect("ariadne produces UTF-8")
}
