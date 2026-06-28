//! Named-argument completion, hover, and parameter/field extraction.

use lsp_types::{CompletionItem, CompletionItemKind, Hover, Url};

use crate::state::ServerState;

/// Completion for the key side of a named-argument list (`f({ <here>: … })`): the
/// parameter names of the callee function, the field names of a struct, or a contract's
/// constructor parameters. `None` when not at a named-argument key.
pub(super) fn named_arg_completion(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<Vec<CompletionItem>> {
    use solsp_syntax::SyntaxKind::*;
    let node = root
        .token_at_offset(offset)
        .left_biased()
        .and_then(|t| t.parent())?;
    let nal = node.ancestors().find(|n| n.kind() == NAMED_ARG_LIST)?;
    // bail in the value position (after a `:` on the current argument).
    let mut last_delim = None;
    for t in nal.children_with_tokens().filter_map(|e| e.into_token()) {
        if t.text_range().start() >= offset {
            break;
        }
        match t.kind() {
            COLON => last_delim = Some(COLON),
            COMMA | L_BRACE | L_PAREN => last_delim = Some(t.kind()),
            _ => {}
        }
    }
    if last_delim == Some(COLON) {
        return None; // value position — let scope/member completion handle it
    }
    let (def_uri, def) = named_arg_target(state, uri, root, &nal)?;
    let droot = super::parse_root(state, &def_uri)?;
    let fields = named_arg_fields(def.kind, &def.full_ptr.to_node(&droot));
    // drop keys already supplied in this argument list (the direct NAME children).
    let present: std::collections::HashSet<String> = nal
        .children()
        .filter(|n| n.kind() == NAME)
        .filter_map(|n| super::node_ident(&n))
        .collect();
    Some(
        fields
            .into_iter()
            .filter(|(name, _)| !present.contains(name))
            .map(|(name, ty)| CompletionItem {
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(ty), // the parameter/field type
                label: name,
                ..Default::default()
            })
            .collect(),
    )
}

/// Hover over a named-argument key (`f({ owner_: … })`) → its parameter/field type,
/// shown as `type name`.
pub(super) fn named_arg_hover(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<Hover> {
    use solsp_syntax::SyntaxKind::{IDENT, NAME, NAMED_ARG_LIST};
    let tok = root.token_at_offset(offset).find(|t| t.kind() == IDENT)?;
    let name_node = tok.parent()?;
    // a key is a NAME that is a direct child of the NAMED_ARG_LIST.
    if name_node.kind() != NAME {
        return None;
    }
    let nal = name_node.parent()?;
    if nal.kind() != NAMED_ARG_LIST {
        return None;
    }
    let key = super::node_ident(&name_node)?;
    let (def_uri, def) = named_arg_target(state, uri, root, &nal)?;
    let droot = super::parse_root(state, &def_uri)?;
    let (_, ty) = named_arg_fields(def.kind, &def.full_ptr.to_node(&droot))
        .into_iter()
        .find(|(n, _)| n == &key)?;
    Some(super::markup_hover(
        format!("```solidity\n{ty} {key}\n```"),
        None,
    ))
}

/// Resolve the callee of the call whose named-argument list is `nal` to its declaration.
fn named_arg_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    nal: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    let callee = nal.parent()?.first_child()?;
    super::resolve_named_callee(state, uri, root, &callee)
}

/// The `(name, type)` of each named argument a callee accepts: a function/constructor's
/// parameters, or a struct's fields.
pub(super) fn named_arg_fields(
    kind: solsp_hir::resolve::DefKind,
    node: &solsp_syntax::SyntaxNode,
) -> Vec<(String, String)> {
    use solsp_hir::resolve::DefKind::*;
    use solsp_syntax::SyntaxKind::{CONSTRUCTOR_DEF, STRUCT_FIELD};
    match kind {
        Function | Modifier | Event | Error => super::param_name_types(node),
        Struct => node
            .descendants()
            .filter(|n| n.kind() == STRUCT_FIELD)
            .filter_map(|f| super::named_type(&f))
            .collect(),
        Contract => node
            .descendants()
            .find(|n| n.kind() == CONSTRUCTOR_DEF)
            .map(|c| super::param_name_types(&c))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}
