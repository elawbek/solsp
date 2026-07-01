//! Cross-file inheritance traversal helpers.

use super::*;

/// All members inherited by `contract` from its base contracts across files (BFS,
/// diamond-safe). Each contract contributes its own direct members.
pub(super) fn collect_inherited_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    external_only: bool,
) -> Vec<solsp_hir::resolve::Definition> {
    collect_inherited_members_impl(state, uri, root, contract, external_only, true)
}

/// All members reachable through `super`: direct and transitive base contracts only.
pub(super) fn collect_base_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    external_only: bool,
) -> Vec<solsp_hir::resolve::Definition> {
    collect_inherited_members_impl(state, uri, root, contract, external_only, false)
}

fn collect_inherited_members_impl(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    external_only: bool,
    include_self: bool,
) -> Vec<solsp_hir::resolve::Definition> {
    use std::collections::{HashSet, VecDeque};
    let mut visited: HashSet<(Url, String)> = HashSet::new();
    let mut queue: VecDeque<(
        Url,
        solsp_syntax::SyntaxNode,
        solsp_syntax::SyntaxNode,
        bool,
    )> = VecDeque::new();
    let mut out = Vec::new();
    if include_self {
        queue.push_back((uri.clone(), root.clone(), contract.clone(), false));
    } else {
        for base in solsp_hir::resolve::base_names(contract) {
            if let Some((bu, br, bn)) = resolve_base(state, uri, root, &base) {
                queue.push_back((bu, br, bn, true));
            }
        }
    }
    while let Some((u, r, c, is_base)) = queue.pop_front() {
        let key = (
            u.clone(),
            solsp_hir::resolve::contract_def_name(&c).unwrap_or_default(),
        );
        if !visited.insert(key) {
            continue;
        }
        for def in solsp_hir::resolve::contract_members(&c) {
            let node = def.full_ptr.to_node(&r);
            if external_only {
                if !solsp_hir::resolve::is_externally_visible(&node) {
                    continue;
                }
            } else if is_base && solsp_hir::resolve::is_private(&node) {
                continue;
            }
            out.push(def);
        }
        for base in solsp_hir::resolve::base_names(&c) {
            if let Some((bu, br, bn)) = resolve_base(state, &u, &r, &base) {
                queue.push_back((bu, br, bn, true));
            }
        }
    }
    out
}

/// Whether user type `a` is `b` or has `b` somewhere in its inheritance.
pub(super) fn is_subtype(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    a: &str,
    b: &str,
) -> bool {
    use std::collections::{HashSet, VecDeque};
    if a == b {
        return true;
    }
    let Some((auri, anode)) = resolve_type_by_name(state, uri, root, a, None) else {
        return true;
    };
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue = VecDeque::from([(auri, anode)]);
    while let Some((u, c)) = queue.pop_front() {
        let Some(cr) = parse_root(state, &u) else {
            continue;
        };
        for base in solsp_hir::resolve::base_names(&c) {
            if base == b {
                return true;
            }
            if visited.insert(base.clone()) {
                if let Some((buri, _, bnode)) = resolve_base(state, &u, &cr, &base) {
                    queue.push_back((buri, bnode));
                }
            }
        }
    }
    false
}
