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
        // Member lookup is inheritance-aware: search the contract and its bases in C3
        // order. Keyed on CONTRACT_DEF (an ancestor of the reference) rather than
        // CONTRACT_BODY so we can reach the inheritance list.
        CONTRACT_DEF => lookup_member(scope, name),
        FUNCTION_DEF | MODIFIER_DEF | CONSTRUCTOR_DEF => find_param(scope, name),
        BLOCK => find_local(scope, name),
        _ => None,
    }
}

/// Look up `name` as a member of `contract`, searching the C3-linearized inheritance
/// chain (the contract first, then bases) — so an inherited member resolves, with the
/// most-derived override winning. Same-file bases only (cross-file imports: P-later).
fn lookup_member(contract: &SyntaxNode, name: &str) -> Option<Definition> {
    let root = contract.ancestors().last()?; // SOURCE_FILE
    for c in c3_linearize(contract, &root) {
        let members = c
            .children()
            .find(|n| n.kind() == SyntaxKind::CONTRACT_BODY)
            .into_iter()
            .flat_map(|body| body.children());
        if let Some(def) = find_named_decl(members, name) {
            return Some(def);
        }
    }
    None
}

/// The C3 linearization (MRO) of a contract: itself followed by its bases in the order
/// member lookup should consult them. Resolves base names in file scope; unresolved or
/// cyclic bases are dropped so the result is always finite.
fn c3_linearize(contract: &SyntaxNode, root: &SyntaxNode) -> Vec<SyntaxNode> {
    fn lin(c: &SyntaxNode, root: &SyntaxNode, on_stack: &mut Vec<String>) -> Vec<SyntaxNode> {
        let cname = contract_name(c);
        if let Some(n) = &cname {
            if on_stack.contains(n) {
                return Vec::new(); // cycle: stop
            }
            on_stack.push(n.clone());
        }
        let bases: Vec<SyntaxNode> = base_names(c)
            .iter()
            .filter_map(|b| resolve_contract(root, b))
            .collect();
        // sequences to merge: each base's own linearization, then the base list itself.
        let mut seqs: Vec<Vec<SyntaxNode>> = bases.iter().map(|b| lin(b, root, on_stack)).collect();
        seqs.push(bases);
        let mut result = vec![c.clone()];
        result.extend(c3_merge(seqs));
        if cname.is_some() {
            on_stack.pop();
        }
        result
    }
    lin(contract, root, &mut Vec::new())
}

/// The C3 merge: repeatedly take the head of the first sequence that does not appear in
/// the tail of any sequence. On an inconsistent hierarchy, stop early (stay total).
fn c3_merge(mut seqs: Vec<Vec<SyntaxNode>>) -> Vec<SyntaxNode> {
    let mut out = Vec::new();
    loop {
        seqs.retain(|s| !s.is_empty());
        if seqs.is_empty() {
            return out;
        }
        let mut picked = None;
        for s in &seqs {
            let head = &s[0];
            let in_tail = seqs.iter().any(|o| o[1..].contains(head));
            if !in_tail {
                picked = Some(head.clone());
                break;
            }
        }
        let Some(head) = picked else {
            return out; // inconsistent: bail with what we have
        };
        for s in &mut seqs {
            s.retain(|n| n != &head);
        }
        out.push(head);
    }
}

/// Names listed in a contract's `is A, B` clause (each base's identifier).
fn base_names(contract: &SyntaxNode) -> Vec<String> {
    let Some(c) = ContractDef::cast(contract.clone()) else {
        return Vec::new();
    };
    c.inheritance_specifiers()
        .filter_map(|spec| {
            spec.syntax()
                .descendants()
                .find(|n| n.kind() == SyntaxKind::NAME_REF)
                .and_then(|nr| ident_text(&nr))
        })
        .collect()
}

/// Find a top-level contract/interface/library named `name` in the file.
fn resolve_contract(root: &SyntaxNode, name: &str) -> Option<SyntaxNode> {
    root.children()
        .find(|n| n.kind() == SyntaxKind::CONTRACT_DEF && contract_name(n).as_deref() == Some(name))
}

fn contract_name(contract: &SyntaxNode) -> Option<String> {
    let name = contract.children().find(|n| n.kind() == SyntaxKind::NAME)?;
    ident_text(&name)
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
    fn resolves_inherited_member_through_base() {
        let src = "contract Base {\n\
            uint256 balance;\n\
            function ping() internal {}\n\
        }\n\
        contract C is Base {\n\
            function use() public {\n\
                ping();\n\
                balance = 1;\n\
            }\n\
        }";
        // `ping()` is defined only on Base → resolves through inheritance.
        let d = resolve_at(src, "ping();").unwrap();
        assert_eq!(d.kind, DefKind::Function);
        assert_eq!(d.name, "ping");
        // inherited state variable
        let d = resolve_at(src, "balance = 1").unwrap();
        assert_eq!(d.kind, DefKind::StateVariable);
    }

    #[test]
    fn diamond_inheritance_resolves_and_override_wins() {
        // D ← B, C ← A. A::f overridden in B. C3 MRO of D is [D, C, B, A]; a call to
        // f() in D must resolve to the most-derived override (B::f), not A::f.
        let src = "contract A { function f() internal virtual {} }\n\
        contract B is A { function f() internal override {} }\n\
        contract C is A {}\n\
        contract D is B, C {\n\
            function go() public { f(); }\n\
        }";
        let d = resolve_at(src, "f();").unwrap();
        assert_eq!(d.kind, DefKind::Function);
        // the resolved f() is B's (its full range starts at B's override, not A's)
        let root = parse(src).syntax();
        let node = d.full_ptr.to_node(&root);
        let b_off = src.find("contract B").unwrap();
        let c_off = src.find("contract C").unwrap();
        let f_off: usize = node.text_range().start().into();
        assert!(f_off > b_off && f_off < c_off, "override should be B::f");
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
