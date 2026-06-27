//! Single-file name resolution (M2 P3). Given a reference (`NAME_REF`), walk the
//! enclosing lexical scopes outward — block locals → function params → contract
//! members → file items — and return the first matching declaration. No imports or
//! inheritance yet (P4/P5 extend this); no name *resolution database* yet — this is a
//! pure function over one file's tree, which the go-to-def/hover features (P5) drive.

use crate::AstPtr;
use solsp_syntax::{
    ast::{AstNode, ContractDef, ContractKind},
    SyntaxKind, SyntaxNode, SyntaxToken,
};

/// What a resolved [`Definition`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Contract,
    Interface,
    Library,
    Function,
    Modifier,
    StateVariable,
    Struct,
    Enum,
    Event,
    Error,
    UserType,
    Parameter,
    Local,
}

/// A resolved declaration: where it is named (go-to-def target) and its full extent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub name: String,
    pub kind: DefKind,
    /// The `NAME` node of the declaration — the go-to-def selection target.
    pub name_ptr: AstPtr,
    /// The whole declaration node.
    pub full_ptr: AstPtr,
}

/// Resolve a `NAME_REF` (or `NAME`) node to its definition within the same file.
/// Returns `None` for builtins/unknowns (and anything needing imports/inheritance).
pub fn resolve(reference: &SyntaxNode) -> Option<Definition> {
    let target = ident_text(reference)?;
    // `ancestors()` yields the node itself first, then each parent up to SOURCE_FILE.
    for scope in reference.ancestors() {
        if let Some(def) = lookup_in_scope(&scope, &target) {
            return Some(def);
        }
    }
    None
}

/// Resolve whatever identifier sits at `offset` (e.g. the LSP cursor). A reference
/// resolves to its definition; a definition resolves to itself (go-to-def on a decl).
pub fn definition_at(root: &SyntaxNode, offset: rowan::TextSize) -> Option<Definition> {
    let token = ident_at(root, offset)?;
    let parent = token.parent()?;
    match parent.kind() {
        SyntaxKind::NAME_REF => resolve(&parent),
        // A `NAME` is itself a declaration's name — go-to-def lands on its own decl.
        SyntaxKind::NAME => {
            let decl = parent.parent()?;
            def_for_decl(&decl)
        }
        _ => None,
    }
}

/// Look for `name` declared directly in one scope node. Non-scope nodes yield `None`.
fn lookup_in_scope(scope: &SyntaxNode, name: &str) -> Option<Definition> {
    use SyntaxKind::*;
    match scope.kind() {
        SOURCE_FILE => find_named_decl(scope.children(), name),
        CONTRACT_BODY => find_named_decl(scope.children(), name),
        FUNCTION_DEF | MODIFIER_DEF | CONSTRUCTOR_DEF => find_param(scope, name),
        BLOCK => find_local(scope, name),
        _ => None,
    }
}

/// First child declaration named `name` (file items / contract members).
fn find_named_decl(nodes: impl Iterator<Item = SyntaxNode>, name: &str) -> Option<Definition> {
    nodes
        .filter_map(|n| def_for_decl(&n))
        .find(|d| d.name == name)
}

/// A parameter of this function/modifier/constructor named `name`. Params live in
/// `PARAM_LIST`s (arguments and returns); a Solidity function body holds no `PARAM`s,
/// so scanning descendants is safe.
fn find_param(scope: &SyntaxNode, name: &str) -> Option<Definition> {
    scope
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::PARAM)
        .find_map(|p| make_def(&p, DefKind::Parameter))
        .filter(|d| d.name == name)
}

/// A local variable declared directly in this block named `name` (not nested blocks).
fn find_local(block: &SyntaxNode, name: &str) -> Option<Definition> {
    block
        .children()
        .filter(|n| n.kind() == SyntaxKind::VAR_DECL_STMT)
        .filter_map(|stmt| stmt.children().find(|n| n.kind() == SyntaxKind::VAR_DECL))
        .find_map(|v| make_def(&v, DefKind::Local))
        .filter(|d| d.name == name)
}

/// Build a [`Definition`] for a top-level/member declaration node, or `None` if it is
/// not a named declaration we resolve.
fn def_for_decl(node: &SyntaxNode) -> Option<Definition> {
    use SyntaxKind::*;
    let kind = match node.kind() {
        CONTRACT_DEF => match ContractDef::cast(node.clone())?.kind() {
            ContractKind::Contract => DefKind::Contract,
            ContractKind::Interface => DefKind::Interface,
            ContractKind::Library => DefKind::Library,
        },
        FUNCTION_DEF => DefKind::Function,
        MODIFIER_DEF => DefKind::Modifier,
        STATE_VAR_DEF => DefKind::StateVariable,
        STRUCT_DEF => DefKind::Struct,
        ENUM_DEF => DefKind::Enum,
        EVENT_DEF => DefKind::Event,
        ERROR_DEF => DefKind::Error,
        USER_DEFINED_VALUE_TYPE => DefKind::UserType,
        PARAM => DefKind::Parameter,
        VAR_DECL => DefKind::Local,
        _ => return None,
    };
    make_def(node, kind)
}

/// Assemble a [`Definition`] from a declaration node and its kind, reading the name
/// from the declaration's `NAME` child. `None` for unnamed declarations.
fn make_def(node: &SyntaxNode, kind: DefKind) -> Option<Definition> {
    let name_node = node.children().find(|n| n.kind() == SyntaxKind::NAME)?;
    let name = ident_text(&name_node)?;
    Some(Definition {
        name,
        kind,
        name_ptr: AstPtr::new(&name_node),
        full_ptr: AstPtr::new(node),
    })
}

/// The `IDENT` token text inside a `NAME`/`NAME_REF` node.
fn ident_text(node: &SyntaxNode) -> Option<String> {
    ident_token(node).map(|t| t.text().to_string())
}

fn ident_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT)
}

/// The `IDENT` token at `offset` (picking the identifier side of a boundary).
fn ident_at(root: &SyntaxNode, offset: rowan::TextSize) -> Option<SyntaxToken> {
    root.token_at_offset(offset)
        .find(|t| t.kind() == SyntaxKind::IDENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solsp_syntax::parse;

    /// Resolve the identifier at the first occurrence of `needle` in `src`.
    fn resolve_at(src: &str, needle: &str) -> Option<Definition> {
        let root = parse(src).syntax();
        let offset = src.find(needle).expect("needle present") as u32;
        definition_at(&root, rowan::TextSize::from(offset))
    }

    #[test]
    fn resolves_param_local_and_member() {
        let src = "contract C {\n\
            uint256 stored;\n\
            function f(uint256 amount) public {\n\
                uint256 tmp = amount;\n\
                stored = tmp;\n\
            }\n\
        }";
        // `amount` (rhs of tmp) → the parameter
        let d = resolve_at(src, "amount;").unwrap();
        assert_eq!(d.kind, DefKind::Parameter);
        assert_eq!(d.name, "amount");

        // `tmp` (rhs of stored) → the local
        let d = resolve_at(src, "tmp;").unwrap();
        assert_eq!(d.kind, DefKind::Local);

        // `stored` (lhs) → the state variable (contract member)
        let d = resolve_at(src, "stored =").unwrap();
        assert_eq!(d.kind, DefKind::StateVariable);
    }

    #[test]
    fn resolves_callee_and_type_to_file_and_member_decls() {
        let src = "struct Point { uint256 x; }\n\
            contract C {\n\
            function helper() internal {}\n\
            function g() public {\n\
                helper();\n\
                Point memory p;\n\
            }\n\
        }";
        // call `helper()` → the contract member function
        let d = resolve_at(src, "helper();").unwrap();
        assert_eq!(d.kind, DefKind::Function);
        assert_eq!(d.name, "helper");

        // type `Point` → the top-level struct
        let d = resolve_at(src, "Point memory").unwrap();
        assert_eq!(d.kind, DefKind::Struct);
        assert_eq!(d.name, "Point");
    }

    #[test]
    fn unknown_name_is_unresolved_and_decl_resolves_to_itself() {
        let src = "contract C { function f() public { bogus(); } }";
        assert!(resolve_at(src, "bogus").is_none());

        // go-to-def on the declaration name `C` returns the contract itself.
        let d = resolve_at(src, "C {").unwrap();
        assert_eq!(d.kind, DefKind::Contract);
        assert_eq!(d.name, "C");
    }

    #[test]
    fn name_ptr_points_at_the_declaration_name() {
        let src = "contract C { uint256 stored; function f() public { stored = 1; } }";
        let root = parse(src).syntax();
        let d = resolve_at(src, "stored = 1").unwrap();
        let name_node = d.name_ptr.to_node(&root);
        assert_eq!(name_node.kind(), SyntaxKind::NAME);
        assert_eq!(name_node.text().to_string().trim(), "stored");
    }
}
