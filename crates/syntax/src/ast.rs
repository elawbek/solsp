//! Typed AST: thin, hand-written wrappers over the untyped [`SyntaxNode`]. Each
//! wrapper is a newtype implementing [`AstNode`] plus accessors like `.name()` or
//! `.functions()` (design §3.5).

use crate::{SyntaxKind, SyntaxNode};

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

// TODO(M1 §3.5): SourceFile, ContractDef, FunctionDef, StructDef, ... with typed
// accessors. Consider an `ast_node!` macro to cut the boilerplate; switch to
// `ungrammar` codegen only if the AST grows large enough to warrant it.
