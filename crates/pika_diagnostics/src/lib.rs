//! Source spans and compiler diagnostics shared by every phase of the Pika compiler.

mod line_index;
mod render;
mod source_map;
mod span;

pub use line_index::LineIndex;
pub use render::{RenderOptions, render_map};
pub use source_map::{FileId, Location, SourceFile, SourceMap};
pub use span::Span;

/// How serious a diagnostic is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Severity {
    /// The program is invalid. Compilation cannot succeed.
    Error,
    /// The program is valid but probably not what was intended.
    Warning,
}

/// A span of source code highlighted by a diagnostic, with an optional message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    /// The highlighted source range.
    pub span: Span,
    /// Text shown next to the highlighted range. May be empty.
    pub message: String,
}

/// A message produced by the compiler about a source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// How serious the problem is.
    pub severity: Severity,
    /// Stable identifier of the diagnostic, such as `E0001`.
    pub code: &'static str,
    /// One-line summary of the problem.
    pub message: String,
    /// The main location of the problem.
    pub primary: Label,
    /// Additional related locations.
    pub secondary: Vec<Label>,
    /// Advice on how to fix the problem.
    pub help: Option<String>,
}

impl Diagnostic {
    /// Creates an error diagnostic whose primary label has no message.
    pub fn error(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            primary: Label {
                span,
                message: String::new(),
            },
            secondary: Vec::new(),
            help: None,
        }
    }

    /// Creates a warning diagnostic whose primary label has no message.
    pub fn warning(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(code, message, span)
        }
    }

    /// Sets the message of the primary label.
    #[must_use]
    pub fn with_label(mut self, message: impl Into<String>) -> Self {
        self.primary.message = message.into();
        self
    }

    /// Adds a secondary label.
    #[must_use]
    pub fn with_secondary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.secondary.push(Label {
            span,
            message: message.into(),
        });
        self
    }

    /// Sets the help text.
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// The diagnostic with its spans moved by `base` bytes: from offsets in a file to
    /// offsets in a [`SourceMap`] where the file starts at `base`.
    #[must_use]
    pub fn shifted(mut self, base: u32) -> Self {
        let shift = |span: Span| Span::new(span.start + base, span.end + base);
        self.primary.span = shift(self.primary.span);
        for label in &mut self.secondary {
            label.span = shift(label.span);
        }
        self
    }

    /// Returns true if this diagnostic is an error.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}
