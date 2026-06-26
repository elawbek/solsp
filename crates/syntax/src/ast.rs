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

/// Generate a typed-AST sum type over several [`SyntaxKind`]s (e.g. `Item`,
/// `Type`). `can_cast` is the OR of the variant kinds; `cast` dispatches on
/// `node.kind()` and wraps the matching leaf wrapper. Each `$ty` must itself be an
/// [`AstNode`] (declared via `ast_node!`).
macro_rules! ast_enum {
    ($name:ident { $($variant:ident($ty:ty) = $kind:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant($ty)),+
        }

        impl AstNode for $name {
            fn can_cast(kind: SyntaxKind) -> bool {
                matches!(kind, $(SyntaxKind::$kind)|+)
            }
            fn cast(node: SyntaxNode) -> Option<Self> {
                let res = match node.kind() {
                    $(SyntaxKind::$kind => Self::$variant(<$ty>::cast(node)?),)+
                    _ => return None,
                };
                Some(res)
            }
            fn syntax(&self) -> &SyntaxNode {
                match self {
                    $(Self::$variant(it) => it.syntax()),+
                }
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
    use super::AstNode;
    use crate::{SyntaxKind, SyntaxNode, SyntaxToken};

    /// The first direct **token** child of `parent` with the given kind.
    pub(super) fn token(parent: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
        parent
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == kind)
    }

    /// All direct **node** children of `parent` castable to `N`, in tree order.
    pub(super) fn children<N: AstNode>(parent: &SyntaxNode) -> impl Iterator<Item = N> {
        parent.children().filter_map(N::cast)
    }

    /// The first direct **node** child of `parent` castable to `N`.
    pub(super) fn child<N: AstNode>(parent: &SyntaxNode) -> Option<N> {
        parent.children().find_map(N::cast)
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

// ---- file-level items --------------------------------------------------------

ast_node!(PragmaDirective, PRAGMA_DIRECTIVE);
ast_node!(ImportDirective, IMPORT_DIRECTIVE);
ast_node!(UsingDirective, USING_DIRECTIVE);
ast_node!(ContractDef, CONTRACT_DEF);
ast_node!(FunctionDef, FUNCTION_DEF);
ast_node!(StructDef, STRUCT_DEF);
ast_node!(EnumDef, ENUM_DEF);
ast_node!(EventDef, EVENT_DEF);
ast_node!(ErrorDef, ERROR_DEF);
ast_node!(UserDefinedValueType, USER_DEFINED_VALUE_TYPE);
ast_node!(StateVarDef, STATE_VAR_DEF);

// A top-level item of a source file. Mirrors `grammar.rs::item`'s dispatch: a
// file-level constant is a `STATE_VAR_DEF` (the `IDENT | MAPPING_KW` arm), and a
// free function is a `FUNCTION_DEF`. `MODIFIER_DEF`/`CONSTRUCTOR_DEF` are NOT here
// — they are contract-body-only members.
ast_enum!(Item {
    Pragma(PragmaDirective) = PRAGMA_DIRECTIVE,
    Import(ImportDirective) = IMPORT_DIRECTIVE,
    Using(UsingDirective) = USING_DIRECTIVE,
    Contract(ContractDef) = CONTRACT_DEF,
    Function(FunctionDef) = FUNCTION_DEF,
    Struct(StructDef) = STRUCT_DEF,
    Enum(EnumDef) = ENUM_DEF,
    Event(EventDef) = EVENT_DEF,
    Error(ErrorDef) = ERROR_DEF,
    Udvt(UserDefinedValueType) = USER_DEFINED_VALUE_TYPE,
    StateVar(StateVarDef) = STATE_VAR_DEF,
});

impl SourceFile {
    /// The file's top-level items, in source order (direct `Item`-castable children
    /// of `SOURCE_FILE`).
    pub fn items(&self) -> impl Iterator<Item = Item> {
        support::children(self.syntax())
    }
}

// ---- contract ----------------------------------------------------------------

ast_node!(ContractBody, CONTRACT_BODY);
ast_node!(InheritanceSpecifier, INHERITANCE_SPECIFIER);
ast_node!(ModifierDef, MODIFIER_DEF);
ast_node!(ConstructorDef, CONSTRUCTOR_DEF);

/// Which `contract`-family keyword introduced a `CONTRACT_DEF`. `abstract` is a
/// modifier on a contract (see `is_abstract`), not a distinct kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractKind {
    Contract,
    Interface,
    Library,
}

impl ContractDef {
    /// The contract's defining name (grammar `contract(p)` calls `name(p)` ⇒ a
    /// direct `NAME` child).
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }

    /// `contract` / `interface` / `library`. The introducer keyword is a direct
    /// token child (bumped by `contract(p)` before the marker completes).
    pub fn kind(&self) -> ContractKind {
        if support::token(self.syntax(), SyntaxKind::INTERFACE_KW).is_some() {
            ContractKind::Interface
        } else if support::token(self.syntax(), SyntaxKind::LIBRARY_KW).is_some() {
            ContractKind::Library
        } else {
            ContractKind::Contract
        }
    }

    /// Whether the contract carries the `abstract` modifier (`abstract contract …`).
    pub fn is_abstract(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::ABSTRACT_KW).is_some()
    }

    /// The brace-delimited body, if present (a direct `CONTRACT_BODY` child).
    pub fn body(&self) -> Option<ContractBody> {
        support::child(self.syntax())
    }

    /// The `is A, B(args)` base specifiers (direct `INHERITANCE_SPECIFIER` children).
    pub fn inheritance_specifiers(&self) -> impl Iterator<Item = InheritanceSpecifier> {
        support::children(self.syntax())
    }

    /// Collect all body members of a given typed kind. A contract has at most one
    /// `CONTRACT_BODY`; returning an owned `Vec` keeps the signature borrow-free and
    /// sidesteps `Option`-of-iterator lifetime gymnastics (allocation is negligible —
    /// outline materializes these anyway).
    fn members<N: AstNode>(&self) -> Vec<N> {
        match self.body() {
            Some(body) => support::children(body.syntax()).collect(),
            None => Vec::new(),
        }
    }

    pub fn functions(&self) -> Vec<FunctionDef> {
        self.members()
    }
    pub fn state_vars(&self) -> Vec<StateVarDef> {
        self.members()
    }
    pub fn structs(&self) -> Vec<StructDef> {
        self.members()
    }
    pub fn enums(&self) -> Vec<EnumDef> {
        self.members()
    }
    pub fn events(&self) -> Vec<EventDef> {
        self.members()
    }
    pub fn errors(&self) -> Vec<ErrorDef> {
        self.members()
    }
    pub fn modifiers(&self) -> Vec<ModifierDef> {
        self.members()
    }
    pub fn constructors(&self) -> Vec<ConstructorDef> {
        self.members()
    }
    pub fn user_defined_value_types(&self) -> Vec<UserDefinedValueType> {
        self.members()
    }
}

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

    #[test]
    fn walks_file_level_items() {
        // pragma, a contract, and a file-level struct — the order the grammar emits.
        let src = "pragma solidity ^0.8.20;\ncontract C {}\nstruct S { uint x; }\n";
        let p = parse(src);
        let file = SourceFile::cast(p.syntax()).unwrap();
        let kinds: Vec<SyntaxKind> = file.items().map(|it| it.syntax().kind()).collect();
        assert_eq!(
            kinds,
            vec![
                SyntaxKind::PRAGMA_DIRECTIVE,
                SyntaxKind::CONTRACT_DEF,
                SyntaxKind::STRUCT_DEF,
            ]
        );
        // the enum discriminates the contract variant
        assert!(matches!(file.items().nth(1), Some(Item::Contract(_))));
    }

    #[test]
    fn reads_contract_header_and_member_counts() {
        let src = "abstract contract C is A, B {\n  \
            uint x;\n  \
            function f() public {}\n  \
            function g() public {}\n  \
            struct P { uint a; }\n\
        }";
        let p = parse(src);
        let file = SourceFile::cast(p.syntax()).unwrap();
        let c = file
            .items()
            .find_map(|it| match it {
                Item::Contract(c) => Some(c),
                _ => None,
            })
            .unwrap();
        assert_eq!(c.name().and_then(|n| n.text()).as_deref(), Some("C"));
        assert!(matches!(c.kind(), ContractKind::Contract));
        assert!(c.is_abstract());
        assert_eq!(c.inheritance_specifiers().count(), 2);
        assert!(c.body().is_some());
        assert_eq!(c.functions().len(), 2);
        assert_eq!(c.state_vars().len(), 1);
        assert_eq!(c.structs().len(), 1);
        assert_eq!(c.enums().len(), 0);
        assert_eq!(c.events().len(), 0);
        assert_eq!(c.errors().len(), 0);
        assert_eq!(c.modifiers().len(), 0);
        assert_eq!(c.constructors().len(), 0);
        assert_eq!(c.user_defined_value_types().len(), 0);
    }
}
