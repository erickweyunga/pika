//! Parser infrastructure: a cursor over the tokens that builds a lossless syntax tree.
//!
//! The grammar itself lives in [`crate::grammar`]. This module provides the primitives it uses:
//! looking ahead while skipping trivia, deciding whether line breaks are significant, building
//! nodes, and reporting errors.

use std::cell::Cell;

use pika_diagnostics::{Diagnostic, Span};
use rowan::{Checkpoint, GreenNode, GreenNodeBuilder};

use crate::codes;
use crate::syntax_kind::{PikaLanguage, SyntaxKind, SyntaxNode};
use crate::token::{Token, TokenKind};

/// The result of parsing a source file: a syntax tree and the problems found.
#[derive(Clone, Debug)]
pub struct Parse {
    green: GreenNode,
    diagnostics: Vec<Diagnostic>,
}

impl Parse {
    /// The root of the syntax tree, a [`SyntaxKind::SourceFile`] node.
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    /// Lexer and parser diagnostics, ordered by position.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Returns true if any diagnostic is an error.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }

    /// The syntax tree formatted one element per line, for debugging and snapshot tests.
    pub fn debug_tree(&self) -> String {
        let mut out = format!("{:#?}", self.syntax());
        // rowan does not end the last line with a newline.
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }
}

/// Parses a whole source file.
///
/// Parsing never fails: syntax errors are reported as diagnostics, and the tree always covers
/// the entire source text (including whitespace and comments), with unparsable regions wrapped
/// in [`SyntaxKind::Error`] nodes.
pub fn parse(source: &str) -> Parse {
    parse_at(source, 0)
}

/// Parses a file that starts at offset `base` in a [`pika_diagnostics::SourceMap`]: its
/// diagnostics have spans from `base`. Spans of its syntax tree are found with
/// [`crate::ast::with_span_base`].
pub fn parse_at(source: &str, base: u32) -> Parse {
    let mut parsed = parse_file(source);
    parsed.diagnostics = parsed
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.shifted(base))
        .collect();
    parsed
}

fn parse_file(source: &str) -> Parse {
    let lexed = crate::lex(source);
    let mut parser = Parser::new(source, &lexed.tokens);
    crate::grammar::source_file(&mut parser);
    let (green, parse_diagnostics) = parser.finish();

    let mut diagnostics = lexed.diagnostics;
    diagnostics.extend(parse_diagnostics);
    diagnostics.sort_by_key(|d| d.primary.span.start);
    Parse { green, diagnostics }
}

/// Maximum number of lookahead calls without consuming a token before the parser is considered
/// stuck. Reaching it is a bug in the grammar, never a property of the input.
const MAX_STEPS_WITHOUT_PROGRESS: u32 = 10_000;

pub(crate) struct Parser<'a> {
    source: &'a str,
    tokens: &'a [Token],
    /// Index of the next token to emit into the tree (possibly trivia).
    pos: usize,
    builder: GreenNodeBuilder<'static>,
    diagnostics: Vec<Diagnostic>,
    /// Whether line breaks are significant, innermost context last.
    newline_modes: Vec<bool>,
    /// Token index at which the last error was reported, to avoid cascades at one position.
    last_error_at: Option<usize>,
    steps: Cell<u32>,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str, tokens: &'a [Token]) -> Self {
        Self {
            source,
            tokens,
            pos: 0,
            builder: GreenNodeBuilder::new(),
            diagnostics: Vec::new(),
            newline_modes: vec![true],
            last_error_at: None,
            steps: Cell::new(0),
        }
    }

    fn finish(self) -> (GreenNode, Vec<Diagnostic>) {
        (self.builder.finish(), self.diagnostics)
    }

    // ----- Lookahead -------------------------------------------------------------------------

    fn newlines_significant(&self) -> bool {
        *self
            .newline_modes
            .last()
            .expect("newline mode stack is never empty")
    }

    fn is_skipped(&self, kind: TokenKind) -> bool {
        kind.is_trivia() || (kind == TokenKind::Newline && !self.newlines_significant())
    }

    /// Index of the `n`th non-skipped token at or after `pos`, or of the `Eof` token.
    fn nth_index(&self, n: usize) -> usize {
        let steps = self.steps.get() + 1;
        assert!(
            steps < MAX_STEPS_WITHOUT_PROGRESS,
            "parser is stuck at token {}",
            self.pos
        );
        self.steps.set(steps);

        let mut seen = 0;
        let mut index = self.pos;
        loop {
            let Some(token) = self.tokens.get(index) else {
                // Only reachable for the empty token list of an oversized file.
                return index;
            };
            if token.kind == TokenKind::Eof {
                return index;
            }
            if !self.is_skipped(token.kind) {
                if seen == n {
                    return index;
                }
                seen += 1;
            }
            index += 1;
        }
    }

    fn nth_token(&self, n: usize) -> Option<Token> {
        self.tokens.get(self.nth_index(n)).copied()
    }

    /// The kind of the `n`th upcoming significant token.
    pub(crate) fn nth(&self, n: usize) -> TokenKind {
        self.nth_token(n).map_or(TokenKind::Eof, |t| t.kind)
    }

    /// The kind of the current significant token.
    pub(crate) fn current(&self) -> TokenKind {
        self.nth(0)
    }

    pub(crate) fn at(&self, kind: TokenKind) -> bool {
        self.current() == kind
    }

    pub(crate) fn nth_at(&self, n: usize, kind: TokenKind) -> bool {
        self.nth(n) == kind
    }

    /// The source text of the `n`th upcoming significant token.
    pub(crate) fn nth_text(&self, n: usize) -> &'a str {
        self.nth_token(n).map_or("", |t| t.text(self.source))
    }

    pub(crate) fn current_text(&self) -> &'a str {
        self.nth_text(0)
    }

    /// Returns true if the current token is an identifier with the given text.
    pub(crate) fn at_contextual(&self, text: &str) -> bool {
        self.at(TokenKind::Ident) && self.current_text() == text
    }

    /// Whether the `n`th upcoming token touches the token before it (see [`Token::joined`]).
    pub(crate) fn nth_joined(&self, n: usize) -> bool {
        self.nth_token(n).is_some_and(|t| t.joined)
    }

    pub(crate) fn at_joined(&self) -> bool {
        self.nth_joined(0)
    }

    /// The span of the current significant token.
    pub(crate) fn current_span(&self) -> Span {
        self.nth_token(0).map_or_else(|| Span::empty(0), |t| t.span)
    }

    /// The span of the `n`th upcoming significant token.
    pub(crate) fn nth_span(&self, n: usize) -> Span {
        self.nth_token(n).map_or_else(|| Span::empty(0), |t| t.span)
    }

    // ----- Tree building ---------------------------------------------------------------------

    fn emit(&mut self, token: Token) {
        let kind = SyntaxKind::from(token.kind);
        self.builder.token(
            <PikaLanguage as rowan::Language>::kind_to_raw(kind),
            token.text(self.source),
        );
    }

    /// Emits skipped tokens (trivia, and line breaks where insignificant) up to the current one.
    fn flush_skipped(&mut self) {
        while let Some(&token) = self.tokens.get(self.pos) {
            if token.kind == TokenKind::Eof || !self.is_skipped(token.kind) {
                break;
            }
            self.emit(token);
            self.pos += 1;
        }
    }

    /// Emits any remaining tokens. Used once at the end of the file.
    pub(crate) fn flush_all(&mut self) {
        while let Some(&token) = self.tokens.get(self.pos) {
            if token.kind != TokenKind::Eof {
                self.emit(token);
            }
            self.pos += 1;
        }
    }

    /// Adds the current token to the tree and advances.
    ///
    /// # Panics
    ///
    /// Panics at the end of the file; callers must check for `Eof` first.
    pub(crate) fn bump(&mut self) {
        self.flush_skipped();
        let token = self.tokens[self.pos];
        assert_ne!(
            token.kind,
            TokenKind::Eof,
            "cannot consume the end of the file"
        );
        self.emit(token);
        self.pos += 1;
        self.steps.set(0);
    }

    /// Consumes the current token if it has the given kind.
    pub(crate) fn eat(&mut self, kind: TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Starts the root node. Unlike [`Parser::start`], leading trivia goes inside it.
    pub(crate) fn start_root(&mut self) {
        assert_eq!(self.pos, 0, "the root node must be started first");
        self.builder
            .start_node(<PikaLanguage as rowan::Language>::kind_to_raw(
                SyntaxKind::SourceFile,
            ));
    }

    pub(crate) fn start(&mut self, kind: SyntaxKind) {
        self.flush_skipped();
        self.builder
            .start_node(<PikaLanguage as rowan::Language>::kind_to_raw(kind));
    }

    pub(crate) fn finish_node(&mut self) {
        self.builder.finish_node();
    }

    pub(crate) fn checkpoint(&mut self) -> Checkpoint {
        self.flush_skipped();
        self.builder.checkpoint()
    }

    /// Starts a node that wraps everything emitted since `checkpoint`.
    pub(crate) fn start_at(&mut self, checkpoint: Checkpoint, kind: SyntaxKind) {
        self.builder.start_node_at(
            checkpoint,
            <PikaLanguage as rowan::Language>::kind_to_raw(kind),
        );
    }

    /// Runs `f` with line breaks significant (`true`, inside blocks) or ignored (`false`,
    /// inside parentheses, brackets and generic argument lists).
    pub(crate) fn with_newlines<R>(
        &mut self,
        significant: bool,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.newline_modes.push(significant);
        let result = f(self);
        self.newline_modes.pop();
        result
    }

    // ----- Errors ----------------------------------------------------------------------------

    /// Records a diagnostic, unless another error was already reported at the current token.
    pub(crate) fn report(&mut self, diagnostic: Diagnostic) {
        let at = self.nth_index(0);
        if self.last_error_at == Some(at) {
            return;
        }
        self.last_error_at = Some(at);
        self.diagnostics.push(diagnostic);
    }

    /// Records a diagnostic regardless of earlier errors at the same position.
    pub(crate) fn report_always(&mut self, diagnostic: Diagnostic) {
        self.last_error_at = Some(self.nth_index(0));
        self.diagnostics.push(diagnostic);
    }

    /// Reports "expected `what`, found ..." at the current token.
    pub(crate) fn error_expected(&mut self, what: &str) {
        let found = self.current().describe();
        let span = self.current_span();
        self.report(Diagnostic::error(
            codes::EXPECTED,
            format!("expected {what}, found {found}"),
            span,
        ));
    }

    /// Consumes the current token if it has the given kind, or reports that it was expected.
    pub(crate) fn expect(&mut self, kind: TokenKind) -> bool {
        if self.eat(kind) {
            return true;
        }
        self.error_expected(kind.describe());
        false
    }

    /// Consumes the closing delimiter `kind` matching the opening delimiter at `opener`, or
    /// reports that the opening delimiter is never closed.
    pub(crate) fn expect_closing(&mut self, kind: TokenKind, opener: Span) -> bool {
        if self.eat(kind) {
            return true;
        }
        let open_text = &self.source[opener.range()];
        let found = self.current();
        let mut diagnostic = Diagnostic::error(
            codes::UNCLOSED_DELIMITER,
            format!("unclosed `{open_text}`"),
            opener,
        )
        .with_label(format!("this `{open_text}` is never closed"));
        if found != TokenKind::Eof {
            diagnostic = diagnostic.with_secondary(
                self.current_span(),
                format!(
                    "expected {} before this {}",
                    kind.describe(),
                    found.describe()
                ),
            );
        }
        self.report(diagnostic);
        false
    }

    /// Wraps the current token in an [`SyntaxKind::Error`] node, unless at the end of the file.
    pub(crate) fn bump_error(&mut self) {
        if self.at(TokenKind::Eof) {
            return;
        }
        self.start(SyntaxKind::Error);
        self.bump();
        self.finish_node();
    }
}
