//! Small syntax-tree text extraction helpers used by server-side semantic features.

/// The identifier text of a `NAME`/`NAME_REF` node.
pub(super) fn node_ident(n: &solsp_syntax::SyntaxNode) -> Option<String> {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == solsp_syntax::SyntaxKind::IDENT)
        .map(|t| t.text().to_string())
}

/// The `(name, type)` of each parameter of a function/constructor (its first
/// `PARAM_LIST`).
pub(super) fn param_name_types(decl: &solsp_syntax::SyntaxNode) -> Vec<(String, String)> {
    use solsp_syntax::SyntaxKind::{PARAM, PARAM_LIST};
    decl.children()
        .find(|n| n.kind() == PARAM_LIST)
        .into_iter()
        .flat_map(|pl| pl.children())
        .filter(|n| n.kind() == PARAM)
        .filter_map(|p| named_type(&p))
        .collect()
}

/// The `(name, type)` of a `PARAM` / `STRUCT_FIELD`: its `NAME` and its type node's text
/// (whitespace-normalized, data-location stripped).
pub(super) fn named_type(decl: &solsp_syntax::SyntaxNode) -> Option<(String, String)> {
    use solsp_syntax::SyntaxKind::NAME;
    let name = decl
        .children()
        .find(|n| n.kind() == NAME)
        .and_then(|n| node_ident(&n))?;
    Some((name, type_text(decl).unwrap_or_default()))
}

/// The declared type of a `PARAM` / `STRUCT_FIELD` / `VAR_DECL` / state-variable node:
/// its first non-`NAME` child node's text (whitespace-normalized; a data-location
/// keyword is a token between the type node and the name, so it is excluded).
pub(super) fn type_text(decl: &solsp_syntax::SyntaxNode) -> Option<String> {
    let ty = decl
        .children()
        .find(|n| n.kind() != solsp_syntax::SyntaxKind::NAME)?;
    Some(node_type_text(&ty))
}

/// The element/value type text of an array or mapping declaration (`T[]` → `T`,
/// `mapping(K => V)` → `V`, including when `V` is itself a mapping). `None` for other types.
pub(super) fn indexed_type_text(decl: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::{ARRAY_TYPE, MAPPING_TYPE, NAME, PATH_TYPE};
    let is_type = |k| matches!(k, PATH_TYPE | ARRAY_TYPE | MAPPING_TYPE);
    let ty = decl.children().find(|n| n.kind() != NAME)?;
    match ty.kind() {
        ARRAY_TYPE => ty
            .children()
            .find(|n| is_type(n.kind()))
            .map(|n| node_type_text(&n)),
        // a mapping's value is its last type child (`=> V`).
        MAPPING_TYPE => ty
            .children()
            .filter(|n| is_type(n.kind()))
            .last()
            .map(|n| node_type_text(&n)),
        _ => None,
    }
}

/// The text of a type node with comment trivia dropped and whitespace normalized, so a
/// `// note\n  address` type node reads as `address`.
pub(super) fn node_type_text(ty: &solsp_syntax::SyntaxNode) -> String {
    let text: String = ty
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() != solsp_syntax::SyntaxKind::COMMENT)
        .map(|t| t.text().to_string())
        .collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
