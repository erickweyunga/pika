//! Kinds of nodes and tokens in the concrete syntax tree.

use crate::token::TokenKind;

/// The kind of a node or token in the concrete syntax tree.
///
/// Token kinds mirror [`TokenKind`]; node kinds describe the grammar of section 16 of the
/// specification. Debug output uses these names, so renaming a variant changes snapshot tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[allow(missing_docs, reason = "the variants are documented by the grammar")]
pub enum SyntaxKind {
    // Tokens (same order as `TokenKind`).
    Whitespace,
    Comment,
    DocComment,
    LineContinuation,
    Newline,
    Ident,
    Variable,
    Underscore,
    KwAnd,
    KwOr,
    KwIn,
    KwAs,
    KwTrue,
    KwFalse,
    KwNone,
    KwSome,
    KwSelf,
    KwSelfType,
    Int,
    Float,
    Duration,
    Char,
    RawString,
    StringStart,
    StringText,
    StringEnd,
    InterpVar,
    InterpParen,
    InterpBracket,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Semi,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Tilde,
    Amp,
    Pipe,
    Caret,
    Dot,
    Eq,
    Ne,
    Lt,
    Le,
    Shl,
    Gt,
    AndAnd,
    OrOr,
    Arrow,
    Question,
    ErrorToken,

    // Structure.
    SourceFile,
    Block,
    /// Tokens and nodes skipped while recovering from a syntax error.
    Error,

    // Names.
    Name,
    NameRef,
    Path,

    // Commands.
    Call,
    CommandName,
    SomeHead,
    ArgList,
    NamedArg,

    // Statements.
    LocalDecl,
    ConstDecl,
    GlobalDecl,
    SetStmt,
    IfStmt,
    WhileStmt,
    DoWhileStmt,
    ForStmt,
    ForeachStmt,
    MatchStmt,
    MatchArmList,
    MatchArm,
    OnErrorStmt,
    UnsafeBlock,

    // Declarations.
    UseDecl,
    FnDecl,
    ParamList,
    Param,
    RetType,
    TypeAnnotation,
    DefaultValue,
    StructDecl,
    EnumDecl,
    TraitDecl,
    ImplDecl,
    ExternDecl,
    TestDecl,
    MemberList,
    FieldDecl,
    VariantDecl,
    ImplList,
    GenericParamList,
    GenericParam,
    GenericArgList,

    // Types.
    PathType,
    FnType,
    OptionType,

    // Expressions.
    Literal,
    StringLit,
    Interpolation,
    VarExpr,
    PathExpr,
    ParenExpr,
    BracketExpr,
    BraceLit,
    FieldInit,
    MapEntry,
    MemberExpr,
    PrefixExpr,
    BinExpr,
    CastExpr,

    // Patterns.
    WildcardPat,
    BindingPat,
    LiteralPat,
    VariantPat,
    SomePat,
    NonePat,
    ParenPat,
}

impl SyntaxKind {
    /// Returns true for trivia tokens (whitespace and comments).
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::Comment | Self::DocComment | Self::LineContinuation
        )
    }
}

impl From<TokenKind> for SyntaxKind {
    fn from(kind: TokenKind) -> Self {
        match kind {
            TokenKind::Whitespace => Self::Whitespace,
            TokenKind::Comment => Self::Comment,
            TokenKind::DocComment => Self::DocComment,
            TokenKind::LineContinuation => Self::LineContinuation,
            TokenKind::Newline => Self::Newline,
            TokenKind::Ident => Self::Ident,
            TokenKind::Variable => Self::Variable,
            TokenKind::Underscore => Self::Underscore,
            TokenKind::KwAnd => Self::KwAnd,
            TokenKind::KwOr => Self::KwOr,
            TokenKind::KwIn => Self::KwIn,
            TokenKind::KwAs => Self::KwAs,
            TokenKind::KwTrue => Self::KwTrue,
            TokenKind::KwFalse => Self::KwFalse,
            TokenKind::KwNone => Self::KwNone,
            TokenKind::KwSome => Self::KwSome,
            TokenKind::KwSelf => Self::KwSelf,
            TokenKind::KwSelfType => Self::KwSelfType,
            TokenKind::Int => Self::Int,
            TokenKind::Float => Self::Float,
            TokenKind::Duration => Self::Duration,
            TokenKind::Char => Self::Char,
            TokenKind::RawString => Self::RawString,
            TokenKind::StringStart => Self::StringStart,
            TokenKind::StringText => Self::StringText,
            TokenKind::StringEnd => Self::StringEnd,
            TokenKind::InterpVar => Self::InterpVar,
            TokenKind::InterpParen => Self::InterpParen,
            TokenKind::InterpBracket => Self::InterpBracket,
            TokenKind::LParen => Self::LParen,
            TokenKind::RParen => Self::RParen,
            TokenKind::LBracket => Self::LBracket,
            TokenKind::RBracket => Self::RBracket,
            TokenKind::LBrace => Self::LBrace,
            TokenKind::RBrace => Self::RBrace,
            TokenKind::Colon => Self::Colon,
            TokenKind::Semi => Self::Semi,
            TokenKind::Comma => Self::Comma,
            TokenKind::Plus => Self::Plus,
            TokenKind::Minus => Self::Minus,
            TokenKind::Star => Self::Star,
            TokenKind::Slash => Self::Slash,
            TokenKind::Percent => Self::Percent,
            TokenKind::Bang => Self::Bang,
            TokenKind::Tilde => Self::Tilde,
            TokenKind::Amp => Self::Amp,
            TokenKind::Pipe => Self::Pipe,
            TokenKind::Caret => Self::Caret,
            TokenKind::Dot => Self::Dot,
            TokenKind::Eq => Self::Eq,
            TokenKind::Ne => Self::Ne,
            TokenKind::Lt => Self::Lt,
            TokenKind::Le => Self::Le,
            TokenKind::Shl => Self::Shl,
            TokenKind::Gt => Self::Gt,
            TokenKind::AndAnd => Self::AndAnd,
            TokenKind::OrOr => Self::OrOr,
            TokenKind::Arrow => Self::Arrow,
            TokenKind::Question => Self::Question,
            TokenKind::Error | TokenKind::Eof => Self::ErrorToken,
        }
    }
}

/// Marker type connecting [`SyntaxKind`] to the `rowan` syntax tree library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PikaLanguage {}

impl rowan::Language for PikaLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> SyntaxKind {
        assert!(
            raw.0 <= SyntaxKind::ParenPat as u16,
            "invalid syntax kind {}",
            raw.0
        );
        // Discriminants are contiguous from zero; a unit test checks `ALL_KINDS` against them.
        ALL_KINDS[raw.0 as usize]
    }

    fn kind_to_raw(kind: SyntaxKind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind as u16)
    }
}

/// A node in the concrete syntax tree.
pub type SyntaxNode = rowan::SyntaxNode<PikaLanguage>;
/// A token in the concrete syntax tree.
pub type SyntaxToken = rowan::SyntaxToken<PikaLanguage>;
/// A node or a token in the concrete syntax tree.
pub type SyntaxElement = rowan::SyntaxElement<PikaLanguage>;

/// Every syntax kind, indexed by discriminant.
const ALL_KINDS: [SyntaxKind; SyntaxKind::ParenPat as usize + 1] = {
    use SyntaxKind as K;
    [
        K::Whitespace,
        K::Comment,
        K::DocComment,
        K::LineContinuation,
        K::Newline,
        K::Ident,
        K::Variable,
        K::Underscore,
        K::KwAnd,
        K::KwOr,
        K::KwIn,
        K::KwAs,
        K::KwTrue,
        K::KwFalse,
        K::KwNone,
        K::KwSome,
        K::KwSelf,
        K::KwSelfType,
        K::Int,
        K::Float,
        K::Duration,
        K::Char,
        K::RawString,
        K::StringStart,
        K::StringText,
        K::StringEnd,
        K::InterpVar,
        K::InterpParen,
        K::InterpBracket,
        K::LParen,
        K::RParen,
        K::LBracket,
        K::RBracket,
        K::LBrace,
        K::RBrace,
        K::Colon,
        K::Semi,
        K::Comma,
        K::Plus,
        K::Minus,
        K::Star,
        K::Slash,
        K::Percent,
        K::Bang,
        K::Tilde,
        K::Amp,
        K::Pipe,
        K::Caret,
        K::Dot,
        K::Eq,
        K::Ne,
        K::Lt,
        K::Le,
        K::Shl,
        K::Gt,
        K::AndAnd,
        K::OrOr,
        K::Arrow,
        K::Question,
        K::ErrorToken,
        K::SourceFile,
        K::Block,
        K::Error,
        K::Name,
        K::NameRef,
        K::Path,
        K::Call,
        K::CommandName,
        K::SomeHead,
        K::ArgList,
        K::NamedArg,
        K::LocalDecl,
        K::ConstDecl,
        K::GlobalDecl,
        K::SetStmt,
        K::IfStmt,
        K::WhileStmt,
        K::DoWhileStmt,
        K::ForStmt,
        K::ForeachStmt,
        K::MatchStmt,
        K::MatchArmList,
        K::MatchArm,
        K::OnErrorStmt,
        K::UnsafeBlock,
        K::UseDecl,
        K::FnDecl,
        K::ParamList,
        K::Param,
        K::RetType,
        K::TypeAnnotation,
        K::DefaultValue,
        K::StructDecl,
        K::EnumDecl,
        K::TraitDecl,
        K::ImplDecl,
        K::ExternDecl,
        K::TestDecl,
        K::MemberList,
        K::FieldDecl,
        K::VariantDecl,
        K::ImplList,
        K::GenericParamList,
        K::GenericParam,
        K::GenericArgList,
        K::PathType,
        K::FnType,
        K::OptionType,
        K::Literal,
        K::StringLit,
        K::Interpolation,
        K::VarExpr,
        K::PathExpr,
        K::ParenExpr,
        K::BracketExpr,
        K::BraceLit,
        K::FieldInit,
        K::MapEntry,
        K::MemberExpr,
        K::PrefixExpr,
        K::BinExpr,
        K::CastExpr,
        K::WildcardPat,
        K::BindingPat,
        K::LiteralPat,
        K::VariantPat,
        K::SomePat,
        K::NonePat,
        K::ParenPat,
    ]
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_kinds_table_matches_discriminants() {
        for (index, kind) in ALL_KINDS.iter().enumerate() {
            assert_eq!(*kind as usize, index, "ALL_KINDS[{index}] is {kind:?}");
        }
    }
}
