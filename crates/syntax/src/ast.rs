//! Typed AST: thin, hand-written wrappers over the untyped [`SyntaxNode`]. Each
//! wrapper is a newtype implementing [`AstNode`] plus accessors like `.name()` or
//! `.functions()` (design §3.5).

use crate::{SyntaxKind, SyntaxNode, SyntaxToken};

/// A typed view over a syntax node of a known kind.
pub trait AstNode {
    fn can_cast(kind: SyntaxKind) -> bool
    where
        Self: Sized;
    fn cast(node: SyntaxNode) -> Option<Self>
    where
        Self: Sized;
    fn syntax(&self) -> &SyntaxNode;
}

/// Generate a typed-AST newtype `pub struct $name(SyntaxNode)` over nodes of a
/// single [`SyntaxKind`], plus its [`AstNode`] impl. `can_cast` is a kind check,
/// `cast` wraps iff the kind matches, `syntax` borrows the inner node. This is the
/// one-line-per-wrapper boilerplate eliminator (design §3.5).
macro_rules! ast_node {
    ($name:ident, $kind:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(SyntaxNode);

        impl AstNode for $name {
            fn can_cast(kind: SyntaxKind) -> bool {
                kind == SyntaxKind::$kind
            }
            fn cast(node: SyntaxNode) -> Option<Self> {
                if Self::can_cast(node.kind()) {
                    Some(Self(node))
                } else {
                    None
                }
            }
            fn syntax(&self) -> &SyntaxNode {
                &self.0
            }
        }
    };
}

/// Tiny accessor helpers over rowan's child iterators, shared by every wrapper.
/// Kept in a submodule so the call sites read `support::child(...)` etc. Grown
/// across tasks: `token` (Task 1), `children` (Task 2), `child` (Task 3) — each
/// introduced where first used (a `pub(super) fn` with no caller is a clippy
/// `dead_code` error under `-D warnings`).
mod support {
    use crate::{SyntaxKind, SyntaxNode, SyntaxToken};

    /// The first direct **token** child of `parent` with the given kind.
    pub(super) fn token(parent: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
        parent
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == kind)
    }
}

// ---- names -------------------------------------------------------------------

ast_node!(Name, NAME);
ast_node!(NameRef, NAME_REF);

impl Name {
    /// The single `IDENT` token this defining name wraps (grammar `name(p)` bumps
    /// exactly one `IDENT` inside the `NAME` marker).
    pub fn ident_token(&self) -> Option<SyntaxToken> {
        support::token(self.syntax(), SyntaxKind::IDENT)
    }
    /// The identifier text, owned (allocates; outline builds owned strings anyway).
    pub fn text(&self) -> Option<String> {
        self.ident_token().map(|t| t.text().to_string())
    }
}

impl NameRef {
    /// The single `IDENT` token this referencing name wraps (grammar `name_ref(p)`).
    pub fn ident_token(&self) -> Option<SyntaxToken> {
        support::token(self.syntax(), SyntaxKind::IDENT)
    }
    /// The identifier text, owned.
    pub fn text(&self) -> Option<String> {
        self.ident_token().map(|t| t.text().to_string())
    }
}

// ---- source file -------------------------------------------------------------

ast_node!(SourceFile, SOURCE_FILE);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, SyntaxKind};

    #[test]
    fn casts_source_file_and_reads_a_name() {
        let p = parse("contract C {}");
        // the root green tree casts to SourceFile
        let file = SourceFile::cast(p.syntax()).expect("root casts to SourceFile");
        assert_eq!(file.syntax().kind(), SyntaxKind::SOURCE_FILE);
        // a NAME node anywhere in the tree reads its identifier text through the wrapper
        let name = p
            .syntax()
            .descendants()
            .find_map(Name::cast)
            .expect("the contract has a NAME node");
        assert_eq!(name.text().as_deref(), Some("C"));
        assert_eq!(name.syntax().kind(), SyntaxKind::NAME);
    }
}
