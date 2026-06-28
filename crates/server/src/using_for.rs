//! Support for Solidity `using L for T` directives.

use lsp_types::{CompletionItem, Url};

use crate::state::ServerState;

/// Parse a `USING_DIRECTIVE` into `(library, target)` — `target` is `None` for `for *`.
/// The `using { f, g } for T` form (no single library) is skipped.
fn parse_using(node: &solsp_syntax::SyntaxNode) -> Option<(String, Option<String>)> {
    use solsp_syntax::SyntaxKind::{FOR_KW, IDENT, STAR, USING_KW};
    let toks: Vec<_> = node
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !matches!(t.kind(), solsp_syntax::SyntaxKind::WHITESPACE))
        .collect();
    let using_pos = toks.iter().position(|t| t.kind() == USING_KW)?;
    let lib_tok = toks.get(using_pos + 1)?;
    if lib_tok.kind() != IDENT {
        return None; // `using { … } for T`
    }
    let for_pos = toks.iter().position(|t| t.kind() == FOR_KW)?;
    let target_tok = toks.get(for_pos + 1)?;
    let target = match target_tok.kind() {
        STAR => None,
        IDENT => Some(target_tok.text().to_string()),
        _ => return None,
    };
    Some((lib_tok.text().to_string(), target))
}

/// The `using L for T` directives in scope at `node`: the enclosing contract's and the
/// file's.
fn using_directives(node: &solsp_syntax::SyntaxNode) -> Vec<(String, Option<String>)> {
    use solsp_syntax::SyntaxKind::{CONTRACT_BODY, SOURCE_FILE, USING_DIRECTIVE};
    node.ancestors()
        .filter(|n| matches!(n.kind(), CONTRACT_BODY | SOURCE_FILE))
        .flat_map(|n| n.children())
        .filter(|c| c.kind() == USING_DIRECTIVE)
        .filter_map(|c| parse_using(&c))
        .collect()
}

/// The type name of a receiver value: a user type's name, or an elementary type's text.
fn receiver_type_name(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<String> {
    if let Some((_, tdef)) = super::resolve_receiver_type(state, uri, root, receiver) {
        return solsp_hir::resolve::contract_def_name(&tdef);
    }
    super::receiver_value_info(state, uri, root, receiver).map(|(t, _)| t)
}

/// Resolve `value.member` through a `using L for T` directive: the library function
/// (the receiver is its implicit first argument). `None` if no directive attaches it.
pub(super) fn using_member(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
    member: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    let type_name = receiver_type_name(state, uri, root, receiver)?;
    for (lib, target) in using_directives(receiver) {
        if target.as_deref().is_none_or(|t| t == type_name) {
            if let Some((luri, lnode)) = super::resolve_type_by_name(state, uri, root, &lib, None) {
                // the call's args plus the implicit receiver argument.
                let def = super::member_lookup(state, &luri, &lnode, member, arity.map(|a| a + 1))
                    .or_else(|| super::member_lookup(state, &luri, &lnode, member, None));
                if let Some(def) = def {
                    return Some((luri, def));
                }
            }
        }
    }
    None
}

/// Completion items for the library functions a `using L for T` directive attaches to the
/// receiver's type.
pub(super) fn using_member_items(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Vec<CompletionItem> {
    let Some(type_name) = receiver_type_name(state, uri, root, receiver) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (lib, target) in using_directives(receiver) {
        if target.as_deref().is_none_or(|t| t == type_name) {
            if let Some((luri, lnode)) = super::resolve_type_by_name(state, uri, root, &lib, None) {
                let Some(lroot) = super::parse_root(state, &luri) else {
                    continue;
                };
                let funcs: Vec<_> = solsp_hir::resolve::type_members(&lnode)
                    .into_iter()
                    .filter(|d| {
                        d.kind == solsp_hir::resolve::DefKind::Function
                            && !solsp_hir::resolve::is_private(&d.full_ptr.to_node(&lroot))
                    })
                    .collect();
                out.extend(super::completion_items_from(funcs));
            }
        }
    }
    out
}
