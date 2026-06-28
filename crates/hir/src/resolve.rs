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
    /// A struct field.
    Field,
    /// An enum variant.
    Variant,
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
    /// The declared type, for value members (field/state-var/param/local). `None` for
    /// functions, types, etc. Used as the completion `detail`.
    pub ty: Option<String>,
}

/// The declared type text of a value declaration, for the `ty` field — its first
/// non-`NAME` child node, whitespace-normalized. Only for value kinds.
fn decl_ty(node: &SyntaxNode, kind: DefKind) -> Option<String> {
    use DefKind::*;
    if !matches!(kind, StateVariable | Parameter | Local | Field) {
        return None;
    }
    let ty = node.children().find(|n| n.kind() != SyntaxKind::NAME)?;
    // the type node may carry a leading comment as trivia — drop comments, normalize ws.
    let text: String = ty
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() != SyntaxKind::COMMENT)
        .map(|t| t.text().to_string())
        .collect();
    Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// A top-level declaration of `root` named `name` (contract/function/struct/etc.).
/// `arity` (when set) picks the matching function overload. Used to resolve an imported
/// symbol against the target file's tree (M2 P7).
pub fn top_level_definition(
    root: &SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<Definition> {
    find_named_decl(root.children(), name, arity)
}

/// Pick the definition named `name` from a file's already-collected top-level `defs` (as
/// from [`file_definitions`]), preferring an exact-arity function overload — the cached
/// equivalent of [`top_level_definition`], avoiding a fresh tree walk. `root` is the tree
/// the `defs` point into (for counting parameters).
pub fn select_named(
    defs: &[Definition],
    name: &str,
    arity: Option<usize>,
    root: &SyntaxNode,
) -> Option<Definition> {
    let mut first: Option<&Definition> = None;
    for def in defs {
        if def.name != name {
            continue;
        }
        if let Some(n) = arity {
            if def.kind == DefKind::Function && param_count(&def.full_ptr.to_node(root)) == Some(n)
            {
                return Some(def.clone());
            }
        }
        if first.is_none() {
            first = Some(def);
        }
    }
    first.cloned()
}

/// If `reference` (a `NAME_REF`) is the member side of a qualified access, return the
/// receiver node and the member name. Handles both an expression member access
/// (`recv.member` ⇒ `MEMBER_EXPR`) and a qualified *type* path (`Iface.NestedType` ⇒
/// `PATH_TYPE` with multiple `NAME_REF` segments). `None` for the base/receiver itself.
pub fn member_access(reference: &SyntaxNode) -> Option<(SyntaxNode, String)> {
    use SyntaxKind::*;
    let parent = reference.parent()?;
    match parent.kind() {
        MEMBER_EXPR => {
            let receiver = parent.first_child()?; // the receiver expression
            if &receiver == reference {
                return None; // `reference` is the receiver, not the member
            }
            Some((receiver, ident_text(reference)?))
        }
        PATH_TYPE => {
            // A qualified type `A.B` is `PATH_TYPE` with `NAME_REF` segments. The member
            // is any segment after the first; the preceding segment is the receiver.
            let segments: Vec<SyntaxNode> =
                parent.children().filter(|n| n.kind() == NAME_REF).collect();
            let idx = segments.iter().position(|n| n == reference)?;
            if idx == 0 {
                return None; // the base type, not a member
            }
            Some((segments[idx - 1].clone(), ident_text(reference)?))
        }
        _ => None,
    }
}

/// The simple name of a receiver expression — `X` in `X.member` — when it is a bare
/// identifier (`NAME_REF` / `PATH_EXPR`). `None` for complex receivers (chains, calls).
pub fn receiver_name(receiver: &SyntaxNode) -> Option<String> {
    let name_ref = if receiver.kind() == SyntaxKind::NAME_REF {
        receiver.clone()
    } else {
        receiver
            .children()
            .find(|n| n.kind() == SyntaxKind::NAME_REF)?
    };
    ident_text(&name_ref)
}

/// The declared type name of a variable/parameter/state-variable declaration — `IRoles`
/// in `IRoles roles`. `None` for elementary/mapping/array types (no user path type).
pub fn declared_type_name(decl: &SyntaxNode) -> Option<String> {
    let path_type = decl
        .children()
        .find(|n| n.kind() == SyntaxKind::PATH_TYPE)?;
    let name_ref = path_type
        .descendants()
        .find(|n| n.kind() == SyntaxKind::NAME_REF)?;
    ident_text(&name_ref)
}

/// Look up `member` inside a type definition's body: contract/interface/library
/// members (incl. same-file C3 bases), struct fields, or enum variants. `arity` (when
/// set) picks the matching method overload.
pub fn member_in_type(
    type_def: &SyntaxNode,
    member: &str,
    arity: Option<usize>,
) -> Option<Definition> {
    use SyntaxKind::*;
    match type_def.kind() {
        CONTRACT_DEF => lookup_member(type_def, member, arity),
        STRUCT_DEF | ENUM_DEF => type_def
            .descendants()
            .filter(|n| matches!(n.kind(), STRUCT_FIELD | ENUM_VARIANT))
            .find_map(|n| {
                let name_node = n.children().find(|c| c.kind() == NAME)?;
                (ident_text(&name_node)? == member).then(|| {
                    let kind = if n.kind() == STRUCT_FIELD {
                        DefKind::Field
                    } else {
                        DefKind::Variant
                    };
                    Definition {
                        name: member.to_string(),
                        kind,
                        name_ptr: AstPtr::new(&name_node),
                        full_ptr: AstPtr::new(&n),
                        ty: decl_ty(&n, kind),
                    }
                })
            }),
        _ => None,
    }
}

/// All definitions visible at `node`, walking enclosing scopes outward: locals, params,
/// same-file contract C3 members, file top-level, and Yul bindings. Inner scopes come
/// first (so they shadow). For scope-based completion — the server augments this with
/// cross-file inherited and imported names.
pub fn scope_definitions(node: &SyntaxNode) -> Vec<Definition> {
    let mut out = Vec::new();
    for scope in node.ancestors() {
        collect_scope(&scope, &mut out);
    }
    out
}

fn collect_scope(scope: &SyntaxNode, out: &mut Vec<Definition>) {
    use SyntaxKind::*;
    match scope.kind() {
        SOURCE_FILE => out.extend(scope.children().filter_map(|n| def_for_decl(&n))),
        CONTRACT_DEF => {
            if let Some(root) = scope.ancestors().last() {
                // the contract itself first (private members visible), then bases (whose
                // `private` members are not inherited).
                for (i, c) in c3_linearize(scope, &root).into_iter().enumerate() {
                    out.extend(
                        contract_members(&c)
                            .into_iter()
                            .filter(|d| i == 0 || !is_private(&d.full_ptr.to_node(&root))),
                    );
                }
            }
        }
        FUNCTION_DEF | MODIFIER_DEF | CONSTRUCTOR_DEF => out.extend(
            scope
                .descendants()
                .filter(|n| n.kind() == PARAM)
                .filter_map(|p| make_def(&p, DefKind::Parameter)),
        ),
        BLOCK | FOR_STMT => out.extend(
            scope
                .children()
                .filter(|n| n.kind() == VAR_DECL_STMT)
                .flat_map(|stmt| stmt.children().filter(|n| n.kind() == VAR_DECL))
                .filter_map(|v| make_def(&v, DefKind::Local)),
        ),
        YUL_BLOCK | YUL_FUNCTION_DEF => out.extend(
            yul_candidates(scope)
                .into_iter()
                .filter_map(|(nm, full, kind)| make_yul_def(&nm, &full, kind)),
        ),
        _ => {}
    }
}

/// Every member exposed directly by a type: contract/interface/library members (incl.
/// same-file C3 bases), struct fields, or enum variants. For member completion — the
/// server adds cross-file inherited members for a contract.
pub fn type_members(type_def: &SyntaxNode) -> Vec<Definition> {
    use SyntaxKind::*;
    match type_def.kind() {
        CONTRACT_DEF => match type_def.ancestors().last() {
            Some(root) => c3_linearize(type_def, &root)
                .iter()
                .flat_map(contract_members)
                .collect(),
            None => contract_members(type_def),
        },
        STRUCT_DEF | ENUM_DEF => type_def
            .descendants()
            .filter(|n| matches!(n.kind(), STRUCT_FIELD | ENUM_VARIANT))
            .filter_map(|n| {
                let name_node = n.children().find(|c| c.kind() == NAME)?;
                let kind = if n.kind() == STRUCT_FIELD {
                    DefKind::Field
                } else {
                    DefKind::Variant
                };
                Some(Definition {
                    name: ident_text(&name_node)?,
                    kind,
                    name_ptr: AstPtr::new(&name_node),
                    full_ptr: AstPtr::new(&n),
                    ty: decl_ty(&n, kind),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Every top-level declaration of a file (contracts, free functions, structs, enums,
/// errors, events, user types). For imported-symbol completion.
pub fn file_definitions(root: &SyntaxNode) -> Vec<Definition> {
    root.children().filter_map(|n| def_for_decl(&n)).collect()
}

/// Whether a member declaration is `private` (visible only in the declaring contract,
/// not in derived contracts).
pub fn is_private(decl: &SyntaxNode) -> bool {
    decl.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == SyntaxKind::PRIVATE_KW)
}

/// Whether a member is reachable through *external* access (`instance.member`): a
/// `public`/`external` function, or a `public` state variable (its getter). `internal` /
/// `private` members and modifiers are not.
pub fn is_externally_visible(decl: &SyntaxNode) -> bool {
    use SyntaxKind::{EXTERNAL_KW, FUNCTION_DEF, PUBLIC_KW, STATE_VAR_DEF};
    if !matches!(decl.kind(), FUNCTION_DEF | STATE_VAR_DEF) {
        return false;
    }
    decl.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| matches!(t.kind(), PUBLIC_KW | EXTERNAL_KW))
}

/// Every member declared directly in a contract's own body (no inheritance).
pub fn contract_members(contract: &SyntaxNode) -> Vec<Definition> {
    contract
        .children()
        .find(|n| n.kind() == SyntaxKind::CONTRACT_BODY)
        .into_iter()
        .flat_map(|body| body.children())
        .filter_map(|n| def_for_decl(&n))
        .collect()
}

/// Resolve a `NAME_REF` (or `NAME`) node to its definition within the same file.
/// Returns `None` for builtins/unknowns (and anything needing imports/inheritance).
pub fn resolve(reference: &SyntaxNode) -> Option<Definition> {
    // the `.member` of `receiver.member` must NOT resolve lexically — it belongs to the
    // receiver's type (member resolution), even when a same-named local is in scope.
    if is_member_position(reference) {
        return None;
    }
    let target = ident_text(reference)?;
    // when the reference is a call's callee, prefer the overload with matching arity.
    let arity = call_arity(reference);
    // `ancestors()` yields the node itself first, then each parent up to SOURCE_FILE.
    for scope in reference.ancestors() {
        if let Some(def) = lookup_in_scope(&scope, &target, arity) {
            return Some(def);
        }
    }
    None
}

/// Whether `reference` is the member side of a `MEMBER_EXPR` (`x` in `recv.x`) — i.e.
/// not the receiver (its first child).
fn is_member_position(reference: &SyntaxNode) -> bool {
    reference
        .parent()
        .filter(|p| p.kind() == SyntaxKind::MEMBER_EXPR)
        .is_some_and(|p| p.first_child().as_ref() != Some(reference))
}

/// If `reference` (a `NAME_REF`) is the callee of a call, the number of arguments in
/// that call (for overload resolution). `None` if it is not a callee.
pub fn call_arity(reference: &SyntaxNode) -> Option<usize> {
    use SyntaxKind::*;
    let callee = reference.parent()?; // PATH_EXPR or MEMBER_EXPR
    let call = callee.parent()?;
    if call.kind() != CALL_EXPR || call.first_child().as_ref() != Some(&callee) {
        return None; // not the callee position
    }
    let args = call
        .children()
        .find(|n| matches!(n.kind(), ARG_LIST | NAMED_ARG_LIST))?;
    Some(args.children().count()) // each child node is one argument
}

/// The parameter count of a function-like declaration (its first `PARAM_LIST`).
fn param_count(decl: &SyntaxNode) -> Option<usize> {
    let plist = decl
        .children()
        .find(|n| n.kind() == SyntaxKind::PARAM_LIST)?;
    Some(
        plist
            .children()
            .filter(|n| n.kind() == SyntaxKind::PARAM)
            .count(),
    )
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
            if let Some(def) = def_for_decl(&decl) {
                return Some(def);
            }
            // a Yul declaration name (`let x`, a param, a return) resolves to itself.
            let kind = match decl.kind() {
                SyntaxKind::YUL_VAR_DECL => DefKind::Local,
                SyntaxKind::YUL_PARAM_LIST => DefKind::Parameter,
                SyntaxKind::YUL_FUNCTION_DEF => DefKind::Function,
                _ => return None,
            };
            make_yul_def(&parent, &decl, kind)
        }
        _ => None,
    }
}

/// Look for `name` declared directly in one scope node. `arity` (when set) disambiguates
/// overloaded functions by parameter count. Non-scope nodes yield `None`.
fn lookup_in_scope(scope: &SyntaxNode, name: &str, arity: Option<usize>) -> Option<Definition> {
    use SyntaxKind::*;
    match scope.kind() {
        SOURCE_FILE => find_named_decl(scope.children(), name, arity),
        // Member lookup is inheritance-aware: search the contract and its bases in C3
        // order. Keyed on CONTRACT_DEF (an ancestor of the reference) rather than
        // CONTRACT_BODY so we can reach the inheritance list.
        CONTRACT_DEF => lookup_member(scope, name, arity),
        FUNCTION_DEF | MODIFIER_DEF | CONSTRUCTOR_DEF => find_param(scope, name),
        BLOCK => find_local(scope, name),
        // a `for (T i; …)` init declaration is a direct child of the FOR_STMT, not the
        // body block, so look for it here too.
        FOR_STMT => find_local(scope, name),
        // inline assembly (Yul): `let` declarations + nested `function` defs, and a
        // Yul function's params / `-> r` returns.
        YUL_BLOCK | YUL_FUNCTION_DEF => find_yul_binding(scope, name),
        _ => None,
    }
}

/// A Yul binding named `name` declared directly in a `YUL_BLOCK` (a `let` variable or a
/// nested `function`) or a `YUL_FUNCTION_DEF` (its params / return names).
fn find_yul_binding(scope: &SyntaxNode, name: &str) -> Option<Definition> {
    yul_candidates(scope)
        .into_iter()
        .filter_map(|(nm, full, kind)| make_yul_def(&nm, &full, kind))
        .find(|d| d.name == name)
}

/// `(name node, full decl node, kind)` for each Yul binding a scope introduces.
fn yul_candidates(scope: &SyntaxNode) -> Vec<(SyntaxNode, SyntaxNode, DefKind)> {
    use SyntaxKind::*;
    let mut out = Vec::new();
    match scope.kind() {
        YUL_BLOCK => {
            for n in scope.children() {
                match n.kind() {
                    YUL_VAR_DECL => {
                        // `let a, b := …` declares each NAME child.
                        for nm in n.children().filter(|c| c.kind() == NAME) {
                            out.push((nm, n.clone(), DefKind::Local));
                        }
                    }
                    YUL_FUNCTION_DEF => {
                        if let Some(fname) = n.children().find(|c| c.kind() == NAME) {
                            out.push((fname, n.clone(), DefKind::Function));
                        }
                    }
                    _ => {}
                }
            }
        }
        YUL_FUNCTION_DEF => {
            // params live in YUL_PARAM_LIST; the `-> r, s` return names are NAME children
            // *after* the param list (the NAME before it is the function's own name).
            let mut after_params = false;
            for n in scope.children() {
                match n.kind() {
                    YUL_PARAM_LIST => {
                        after_params = true;
                        for nm in n.children().filter(|c| c.kind() == NAME) {
                            out.push((nm.clone(), nm, DefKind::Parameter));
                        }
                    }
                    NAME if after_params => out.push((n.clone(), n, DefKind::Local)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// Build a [`Definition`] for a Yul binding from its name node + full declaration node.
fn make_yul_def(name_node: &SyntaxNode, full: &SyntaxNode, kind: DefKind) -> Option<Definition> {
    Some(Definition {
        name: ident_text(name_node)?,
        kind,
        name_ptr: AstPtr::new(name_node),
        full_ptr: AstPtr::new(full),
        ty: None, // Yul values are untyped words
    })
}

/// The identifier segments of a type path (`["ICraftV2", "TokenInput"]` for
/// `ICraftV2.TokenInput`).
pub fn path_type_segments(path_type: &SyntaxNode) -> Vec<String> {
    path_type
        .children()
        .filter(|n| n.kind() == SyntaxKind::NAME_REF)
        .filter_map(|n| ident_text(&n))
        .collect()
}

/// The user-defined type node of a variable/param/state-var declaration: the
/// `PATH_TYPE`, or — when `element` is set — the `PATH_TYPE` element of an
/// `ARRAY_TYPE` (for `arr[i]`). `None` for elementary/mapping types or an array used
/// without indexing.
pub fn decl_type_path(decl: &SyntaxNode, element: bool) -> Option<SyntaxNode> {
    use SyntaxKind::*;
    let ty = decl
        .children()
        .find(|n| matches!(n.kind(), PATH_TYPE | ARRAY_TYPE | MAPPING_TYPE))?;
    match ty.kind() {
        PATH_TYPE => (!element).then_some(ty),
        ARRAY_TYPE => {
            let inner = ty.children().find(|n| n.kind() == PATH_TYPE)?;
            element.then_some(inner)
        }
        // `m[k]` on a mapping yields its value type — the `=> V` side (last type child),
        // returned only when it is a user-defined `PATH_TYPE`.
        MAPPING_TYPE if element => {
            let value = ty
                .children()
                .filter(|n| matches!(n.kind(), PATH_TYPE | ARRAY_TYPE | MAPPING_TYPE))
                .last()?;
            (value.kind() == PATH_TYPE).then_some(value)
        }
        _ => None,
    }
}

/// Look up `name` as a member of `contract`, searching the C3-linearized inheritance
/// chain (the contract first, then bases) — so an inherited member resolves, with the
/// most-derived override winning. Same-file bases only (cross-file imports: P-later).
fn lookup_member(contract: &SyntaxNode, name: &str, arity: Option<usize>) -> Option<Definition> {
    let root = contract.ancestors().last()?; // SOURCE_FILE
                                             // The first entry is the contract itself (its `private` members are visible); the rest
                                             // are bases, whose `private` members are NOT inherited.
    for (i, c) in c3_linearize(contract, &root).into_iter().enumerate() {
        let members: Vec<SyntaxNode> = c
            .children()
            .find(|n| n.kind() == SyntaxKind::CONTRACT_BODY)
            .into_iter()
            .flat_map(|body| body.children())
            .filter(|n| i == 0 || !is_private(n))
            .collect();
        if let Some(def) = find_named_decl(members.into_iter(), name, arity) {
            return Some(def);
        }
    }
    None
}

/// Every member a contract resolves through its own body and same-file C3 bases, in
/// lookup order (the contract's own members first, then each base; a base's `private`
/// members are excluded). The pre-collected form of [`lookup_member`] — pair with
/// [`select_named`] for a cached member lookup that avoids re-walking the type per access.
pub fn contract_member_defs(contract: &SyntaxNode) -> Vec<Definition> {
    let Some(root) = contract.ancestors().last() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, c) in c3_linearize(contract, &root).into_iter().enumerate() {
        let body = c.children().find(|n| n.kind() == SyntaxKind::CONTRACT_BODY);
        for n in body.into_iter().flat_map(|b| b.children()) {
            if i != 0 && is_private(&n) {
                continue;
            }
            if let Some(def) = def_for_decl(&n) {
                out.push(def);
            }
        }
    }
    out
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

/// Look up `name` among a single contract's *own* members (its `CONTRACT_BODY`, no
/// inheritance). Used by the server to walk cross-file inheritance one contract at a
/// time. `arity` picks a matching method overload.
pub fn contract_member(
    contract: &SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<Definition> {
    let members = contract
        .children()
        .find(|n| n.kind() == SyntaxKind::CONTRACT_BODY)
        .into_iter()
        .flat_map(|body| body.children());
    find_named_decl(members, name, arity)
}

/// The contract/interface/library named `name` declared at the top level of `root`.
pub fn find_contract(root: &SyntaxNode, name: &str) -> Option<SyntaxNode> {
    resolve_contract(root, name)
}

/// The declared name of a contract/interface/library definition node.
pub fn contract_def_name(contract: &SyntaxNode) -> Option<String> {
    contract_name(contract)
}

/// Names listed in a contract's `is A, B` clause (each base's identifier).
pub fn base_names(contract: &SyntaxNode) -> Vec<String> {
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
/// First child declaration named `name`. When `arity` is set and several overloads
/// share the name, prefer the function whose parameter count matches; otherwise return
/// the first same-named declaration.
fn find_named_decl(
    nodes: impl Iterator<Item = SyntaxNode>,
    name: &str,
    arity: Option<usize>,
) -> Option<Definition> {
    let mut first: Option<Definition> = None;
    for node in nodes {
        let Some(def) = def_for_decl(&node) else {
            continue;
        };
        if def.name != name {
            continue;
        }
        if let Some(n) = arity {
            if def.kind == DefKind::Function && param_count(&node) == Some(n) {
                return Some(def); // exact overload match
            }
        }
        if first.is_none() {
            first = Some(def);
        }
    }
    first
}

/// A parameter of this function/modifier/constructor named `name`. Params live in
/// `PARAM_LIST`s (arguments and returns); a Solidity function body holds no `PARAM`s,
/// so scanning descendants is safe.
fn find_param(scope: &SyntaxNode, name: &str) -> Option<Definition> {
    // `filter_map(..).find(name)` — NOT `find_map(..).filter(name)`, which would stop at
    // the first PARAM and only ever resolve the first parameter.
    scope
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::PARAM)
        .filter_map(|p| make_def(&p, DefKind::Parameter))
        .find(|d| d.name == name)
}

/// A local variable declared directly in this block named `name` (not nested blocks).
fn find_local(block: &SyntaxNode, name: &str) -> Option<Definition> {
    block
        .children()
        .filter(|n| n.kind() == SyntaxKind::VAR_DECL_STMT)
        // a tuple declaration `(A a, B b) = …` has several VAR_DECL children — keep all.
        .flat_map(|stmt| stmt.children().filter(|n| n.kind() == SyntaxKind::VAR_DECL))
        .filter_map(|v| make_def(&v, DefKind::Local))
        .find(|d| d.name == name)
}

/// Build a [`Definition`] for a top-level/member declaration node, or `None` if it is
/// not a named declaration we resolve.
/// The [`Definition`] a declaration node represents (`None` if it is not a declaration).
pub fn definition(node: &SyntaxNode) -> Option<Definition> {
    def_for_decl(node)
}

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
        ty: decl_ty(node, kind),
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
    fn scope_definitions_and_type_members() {
        let src = "contract Base { function inherited() internal {} }\n\
                   contract C is Base { uint256 stateVar; function f(uint256 p) public { uint256 lokal; } }";
        let root = parse(src).syntax();

        // scope at the `lokal` declaration sees locals, params, same-file C3 members,
        // and file top-level.
        let lokal = root
            .descendants()
            .find(|n| {
                n.kind() == SyntaxKind::VAR_DECL
                    && n.descendants().any(|d| {
                        d.kind() == SyntaxKind::NAME && ident_text(&d).as_deref() == Some("lokal")
                    })
            })
            .unwrap();
        let names: Vec<String> = scope_definitions(&lokal)
            .into_iter()
            .map(|d| d.name)
            .collect();
        for want in ["lokal", "p", "stateVar", "f", "inherited", "Base", "C"] {
            assert!(
                names.contains(&want.to_string()),
                "scope missing {want}: {names:?}"
            );
        }

        // type_members(C) = own members + same-file C3 base members.
        let c = root
            .descendants()
            .find(|n| {
                n.kind() == SyntaxKind::CONTRACT_DEF && contract_name(n).as_deref() == Some("C")
            })
            .unwrap();
        let members: Vec<String> = type_members(&c).into_iter().map(|d| d.name).collect();
        assert!(members.contains(&"stateVar".to_string()));
        assert!(members.contains(&"f".to_string()));
        assert!(members.contains(&"inherited".to_string()));
    }

    #[test]
    fn private_base_members_not_inherited() {
        let src = "contract Base { function pub_() internal {} function hid_() private {} }\n\
                   contract C is Base { function f() public {} }";
        let root = parse(src).syntax();
        // the last BLOCK is `C.f`'s body.
        let f_block = root
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::BLOCK)
            .last()
            .unwrap();
        let names: Vec<String> = scope_definitions(&f_block)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(
            names.contains(&"pub_".to_string()),
            "internal base member inherited"
        );
        assert!(
            !names.contains(&"hid_".to_string()),
            "private base member NOT inherited"
        );
    }

    #[test]
    fn member_does_not_resolve_to_same_named_local() {
        // `lib.s` — the member `s` must NOT bind to the local `s` in scope; it belongs
        // to the receiver's type (member resolution handles it). Regression: clicking
        // `s` in `CraftV2Lib.s()` jumped to a same-named local instead of the method.
        let src = "contract C { function f() public { uint256 s = 1; uint256 y = lib.s; } }";
        let root = parse(src).syntax();
        let member = root
            .descendants()
            .find(|n| n.kind() == SyntaxKind::MEMBER_EXPR)
            .and_then(|m| m.children().nth(1)) // [receiver, member]
            .unwrap();
        assert_eq!(member.kind(), SyntaxKind::NAME_REF);
        assert!(resolve(&member).is_none());
        // the receiver `lib` (first child) still resolves lexically — here unknown → None,
        // but a real receiver would bind; the guard must not affect it.
    }

    #[test]
    fn resolves_overload_by_argument_count() {
        let src = "contract C {\n\
            function f() internal {}\n\
            function f(uint a) internal {}\n\
            function f(uint a, uint b) internal {}\n\
            function g() public { f(1, 2); f(1); f(); }\n\
        }";
        let root = parse(src).syntax();
        let resolved = |needle: &str| {
            resolve_at(src, needle)
                .unwrap()
                .full_ptr
                .to_node(&root)
                .text()
                .to_string()
        };
        assert!(resolved("f(1, 2)").contains("uint a, uint b")); // 2-arg → 2-param
        let one = resolved("f(1);");
        assert!(one.contains("(uint a)") && !one.contains("uint b")); // 1-arg → 1-param
        assert!(resolved("f(); }").trim_start().starts_with("function f()")); // 0 → 0
    }

    #[test]
    fn resolves_yul_variables() {
        let src = "contract C { function f() public { assembly {\n\
            let x := 1\n\
            let y := add(x, 2)\n\
            function g(a) -> r { r := mul(a, x) }\n\
        } } }";
        // `x` in `add(x, 2)` → the `let x` declaration
        let d = resolve_at(src, "x, 2").unwrap();
        assert_eq!(d.kind, DefKind::Local);
        assert_eq!(d.name, "x");
        // `a` in `mul(a, x)` → the Yul function parameter
        let d = resolve_at(src, "a, x").unwrap();
        assert_eq!(d.kind, DefKind::Parameter);
        assert_eq!(d.name, "a");
        // `r` in `r := …` → the Yul function return binding
        let d = resolve_at(src, "r := mul").unwrap();
        assert_eq!(d.kind, DefKind::Local);
        assert_eq!(d.name, "r");
        // `x` in `mul(a, x)` (inside the Yul function) → the OUTER `let x` (closure)
        let d = resolve_at(src, "x) }").unwrap();
        assert_eq!(d.name, "x");
    }

    #[test]
    fn resolves_for_loop_variable() {
        // the `for (T i; …)` init variable, used in the condition / body.
        let src = "contract C { function f() public {\n\
            for (uint256 i; i < 10; ++i) { uint256 x = i; }\n\
        } }";
        let d = resolve_at(src, "i < 10").unwrap();
        assert_eq!(d.kind, DefKind::Local);
        assert_eq!(d.name, "i");
        let d = resolve_at(src, "i; }").unwrap(); // `i` in the body `x = i`
        assert_eq!(d.kind, DefKind::Local);
    }

    #[test]
    fn resolves_non_first_param_and_local() {
        // regression: find_param/find_local must scan ALL params/locals, not just the
        // first — a use of the 2nd/3rd param or a later local must resolve.
        let src = "contract C {\n\
            function f(uint a, uint b, uint c) public {\n\
                uint x = 1;\n\
                uint y = 2;\n\
                c = a + b;\n\
                y = x;\n\
            }\n\
        }";
        // 3rd parameter `c` (lhs of `c = a + b`)
        let d = resolve_at(src, "c = a").unwrap();
        assert_eq!(d.kind, DefKind::Parameter);
        assert_eq!(d.name, "c");
        // 2nd parameter `b`
        let d = resolve_at(src, "b;").unwrap();
        assert_eq!(d.kind, DefKind::Parameter);
        assert_eq!(d.name, "b");
        // 2nd local `y` (lhs of `y = x`)
        let d = resolve_at(src, "y = x").unwrap();
        assert_eq!(d.kind, DefKind::Local);
        assert_eq!(d.name, "y");
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
    fn member_resolution_helpers() {
        let src = "library L { function s() internal returns (uint) {} }\n\
            contract C {\n\
                L lib;\n\
                function f() public { L.s(); lib.s(); }\n\
            }";
        let root = parse(src).syntax();

        // the member NAME_REF `s` in `L.s()`
        let s_ref = root
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .find(|n| ident_text(n).as_deref() == Some("s") && member_access(n).is_some())
            .unwrap();
        let (receiver, member) = member_access(&s_ref).unwrap();
        assert_eq!(member, "s");
        assert_eq!(receiver_name(&receiver).as_deref(), Some("L"));

        // member lookup in the library type
        let lib_def = root
            .descendants()
            .find(|n| n.kind() == SyntaxKind::CONTRACT_DEF)
            .unwrap();
        let m = member_in_type(&lib_def, "s", None).unwrap();
        assert_eq!(m.kind, DefKind::Function);
        assert_eq!(m.name, "s");

        // declared type of the state variable `L lib;`
        let var = root
            .descendants()
            .find(|n| n.kind() == SyntaxKind::STATE_VAR_DEF)
            .unwrap();
        assert_eq!(declared_type_name(&var).as_deref(), Some("L"));
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
