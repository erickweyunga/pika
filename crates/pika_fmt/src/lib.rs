//! The Pika formatter: rewrites a source file in its canonical layout.
//!
//! The formatter only changes the space between tokens. It keeps every line break between
//! statements, every comment, and whether each pair of tokens is joined (written without
//! space between them), which the language gives meaning to; the few changes of joining it
//! makes are ones that never change meaning, such as the spaces just inside parentheses. As a
//! guarantee, the result is parsed again and must have the same syntax tree, apart from space
//! and comments, as the source.
//!
//! The canonical layout:
//!
//! - Lines are indented by four spaces for each bracket (`{`, `(` or `[`) left open by the
//!   lines before; a line that starts by closing a bracket is indented like the line that
//!   opened it, and a line after a `\` continuation by one more level.
//! - Tokens on a line are separated by one space or joined, as written, except that there is
//!   no space just inside `(`, `[`, `$(` and `$[` or before `)`, `]`, `;` and `,`, one space
//!   after `;` and `,` (except in `impl=A,B` and `:foreach i,x`), one space around binary
//!   operators, one space just inside the braces of a block written on one line, and none just
//!   inside the braces of a collection or struct literal.
//! - `&&` and `||` are written `and` and `or`.
//! - There is no space at the end of a line, at most one blank line in a row, no blank line
//!   just inside a bracket, and the file ends with one line break.
//! - The space before a comment at the end of a line is kept, so that comments aligned by
//!   their author stay aligned.

use pika_diagnostics::Diagnostic;
use pika_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

/// Why a file cannot be formatted.
#[derive(Debug)]
pub enum FormatError {
    /// The file has syntax errors.
    Syntax(Vec<Diagnostic>),
    /// Formatting would change the meaning of the file. This is a bug of the formatter; the
    /// file is left as it is.
    Changed,
}

/// The canonical layout of `source`.
///
/// # Errors
///
/// Fails if `source` has syntax errors, or, through a bug of the formatter, if the layout
/// would change what it means.
pub fn format(source: &str) -> Result<String, FormatError> {
    let parse = pika_syntax::parse(source);
    if parse.has_errors() {
        return Err(FormatError::Syntax(parse.diagnostics().to_vec()));
    }
    let formatted = layout(&parse.syntax());
    let reparsed = pika_syntax::parse(&formatted);
    if reparsed.has_errors() || skeleton(&parse.syntax()) != skeleton(&reparsed.syntax()) {
        return Err(FormatError::Changed);
    }
    Ok(formatted)
}

/// The parts of a syntax tree that carry meaning: its nodes and tokens, without space,
/// comments and line breaks (which separate statements, as the nodes show), with `&&` and
/// `||` as `and` and `or`.
fn skeleton(root: &SyntaxNode) -> Vec<(SyntaxKind, String)> {
    let mut parts = Vec::new();
    for event in root.preorder_with_tokens() {
        match event {
            rowan::WalkEvent::Enter(rowan::NodeOrToken::Node(node)) => {
                parts.push((node.kind(), String::new()));
            }
            rowan::WalkEvent::Leave(rowan::NodeOrToken::Node(node)) => {
                parts.push((node.kind(), "end".to_owned()));
            }
            rowan::WalkEvent::Enter(rowan::NodeOrToken::Token(token)) => {
                let kind = token.kind();
                if kind.is_trivia() || kind == SyntaxKind::Newline {
                    continue;
                }
                let (kind, text) = canonical_token(&token);
                parts.push((kind, text.to_owned()));
            }
            rowan::WalkEvent::Leave(rowan::NodeOrToken::Token(_)) => {}
        }
    }
    parts
}

/// A token as the canonical layout writes it.
fn canonical_token(token: &SyntaxToken) -> (SyntaxKind, &str) {
    match token.kind() {
        SyntaxKind::AndAnd => (SyntaxKind::KwAnd, "and"),
        SyntaxKind::OrOr => (SyntaxKind::KwOr, "or"),
        kind => (kind, token.text()),
    }
}

/// Builds the lines of the canonical layout, token by token.
#[derive(Default)]
struct Layout {
    /// The finished lines, each with its indentation.
    lines: Vec<String>,
    /// The text of the current line so far, without its indentation.
    line: String,
    /// The indentation of the current line, once its first token is known.
    indent: usize,
    /// For each bracket open so far, the indentation of the line that opened it.
    open: Vec<usize>,
    /// Blank lines seen since the last line with text.
    blank_lines: usize,
    /// Whether space separated the next token from the previous one.
    space: bool,
    /// The space before the next token, as written, for a comment at the end of a line.
    written_space: String,
    /// The previous token on the line.
    previous: Option<SyntaxToken>,
    /// Whether the previous line ended with `\`, which continues it on this line.
    continued: bool,
    /// The last token on the current line that is not space or a comment.
    last_code: Option<SyntaxKind>,
    /// Whether the last line with text ended by opening a bracket.
    opened: bool,
}

fn layout(root: &SyntaxNode) -> String {
    let mut layout = Layout::default();
    for token in root
        .descendants_with_tokens()
        .filter_map(rowan::NodeOrToken::into_token)
    {
        layout.token(&token);
    }
    layout.end_line();
    let mut text = layout.lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

impl Layout {
    fn token(&mut self, token: &SyntaxToken) {
        match token.kind() {
            SyntaxKind::Whitespace => {
                self.space = true;
                self.written_space.push_str(token.text());
            }
            SyntaxKind::Newline => self.end_line(),
            SyntaxKind::LineContinuation => {
                self.line.push_str(" \\");
                self.end_line();
                self.continued = true;
            }
            SyntaxKind::Comment | SyntaxKind::DocComment => {
                if self.line.is_empty() {
                    self.start_line(None);
                } else {
                    let written = std::mem::take(&mut self.written_space);
                    self.line
                        .push_str(if written.is_empty() { " " } else { &written });
                }
                self.line.push_str(token.text().trim_end());
                self.space = false;
            }
            kind => self.code(token, kind),
        }
    }

    /// A token that is not space or a comment.
    fn code(&mut self, token: &SyntaxToken, kind: SyntaxKind) {
        if self.line.is_empty() {
            self.start_line(Some(kind));
        } else if self.space_before(token) {
            self.line.push(' ');
        }
        let (_, text) = canonical_token(token);
        self.line.push_str(text);
        if is_opener(kind) {
            self.open.push(self.indent);
        } else if is_closer(kind) {
            self.open.pop();
        }
        self.previous = Some(token.clone());
        self.last_code = Some(kind);
        self.space = false;
        self.written_space.clear();
    }

    /// Starts a line with a token of kind `first` (`None` for a comment): sets its indentation
    /// and writes the blank lines kept before it.
    fn start_line(&mut self, first: Option<SyntaxKind>) {
        let closes = first.is_some_and(is_closer);
        self.indent = match self.open.last() {
            Some(&opener) if closes => opener,
            Some(&opener) => opener + 1,
            None => 0,
        } + usize::from(self.continued && !closes);
        self.continued = false;
        // No blank line at the start of the file, just inside a bracket, or before a line
        // that closes one; at most one anywhere else.
        if self.blank_lines > 0 && !self.lines.is_empty() && !self.opened && !closes {
            self.lines.push(String::new());
        }
        self.blank_lines = 0;
        self.previous = None;
    }

    /// Finishes the current line; an empty one is a blank line.
    fn end_line(&mut self) {
        if self.line.is_empty() {
            self.blank_lines += 1;
        } else {
            let line = std::mem::take(&mut self.line);
            self.lines
                .push(format!("{}{line}", "    ".repeat(self.indent)));
            self.opened = self.last_code.is_some_and(is_opener);
        }
        self.last_code = None;
        self.space = false;
        self.written_space.clear();
        self.previous = None;
    }

    /// Whether one space separates the previous token on the line from `token`.
    fn space_before(&self, token: &SyntaxToken) -> bool {
        let Some(previous) = &self.previous else {
            return false;
        };
        let parent_kind = |t: &SyntaxToken| t.parent().map(|p| p.kind());
        let literal_brace = |t: &SyntaxToken| parent_kind(t) == Some(SyntaxKind::BraceLit);
        // The tokens of a binary expression that are not in its operands are its operator.
        let operator = |t: &SyntaxToken| parent_kind(t) == Some(SyntaxKind::BinExpr);
        match (previous.kind(), token.kind()) {
            // `and` and `or` written as `&&` and `||` may have been joined to their operands.
            (SyntaxKind::AndAnd | SyntaxKind::OrOr, _)
            | (_, SyntaxKind::AndAnd | SyntaxKind::OrOr) => true,
            // An empty pair of braces, and space just inside other brackets.
            (SyntaxKind::LBrace, SyntaxKind::RBrace)
            | (
                SyntaxKind::LParen
                | SyntaxKind::LBracket
                | SyntaxKind::InterpParen
                | SyntaxKind::InterpBracket,
                _,
            )
            | (
                _,
                SyntaxKind::RParen | SyntaxKind::RBracket | SyntaxKind::Semi | SyntaxKind::Comma,
            ) => false,
            // `impl=A,B` is one argument, and `:foreach i,x` names its variables without space.
            (SyntaxKind::Comma, _)
                if matches!(
                    parent_kind(previous),
                    Some(SyntaxKind::ImplList | SyntaxKind::ForeachStmt)
                ) =>
            {
                false
            }
            (SyntaxKind::Semi | SyntaxKind::Comma, _) => true,
            (SyntaxKind::LBrace, _) => !literal_brace(previous),
            (_, SyntaxKind::RBrace) => !literal_brace(token),
            // An operator written as two joined tokens, as `>` `=`, stays as written.
            _ if operator(previous) && operator(token) => self.space,
            _ if operator(previous) || operator(token) => true,
            _ => self.space,
        }
    }
}

fn is_opener(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::LParen
            | SyntaxKind::LBracket
            | SyntaxKind::LBrace
            | SyntaxKind::InterpParen
            | SyntaxKind::InterpBracket
    )
}

fn is_closer(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::RParen | SyntaxKind::RBracket | SyntaxKind::RBrace
    )
}
