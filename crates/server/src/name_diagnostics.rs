//! Name-resolution diagnostics.

use super::*;

/// Flag identifiers used as values that resolve to no declaration anywhere.
pub(super) fn undefined_name_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{COLON_EQ, NAME_REF, PATH_EXPR, YUL_ASSIGNMENT, YUL_PATH};
    let mut out = Vec::new();
    for nr in root.descendants().filter(|n| n.kind() == NAME_REF) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        if nr.parent().map(|p| p.kind()) != Some(PATH_EXPR) {
            continue;
        }
        let Some(name) = nameref_text(&nr) else {
            continue;
        };
        if !name_defined(state, uri, root, &nr, &name) {
            out.push(type_mismatch(li, &nr, &format!("`{name}` is not defined")));
        }
    }

    for asn in root.descendants().filter(|n| n.kind() == YUL_ASSIGNMENT) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let Some(eq) = asn
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == COLON_EQ)
            .map(|t| t.text_range().start())
        else {
            continue;
        };
        for path in asn
            .children()
            .filter(|n| n.kind() == YUL_PATH && n.text_range().end() <= eq)
        {
            let Some(seg) = path.descendants().find(|n| n.kind() == NAME_REF) else {
                continue;
            };
            let Some(name) = nameref_text(&seg) else {
                continue;
            };
            if !yul_assignment_target_defined(state, uri, root, &seg, &name) {
                out.push(type_mismatch(li, &seg, &format!("`{name}` is not defined")));
            }
        }
    }
    out
}

fn yul_assignment_target_defined(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    target: &solsp_syntax::SyntaxNode,
    name: &str,
) -> bool {
    if let Some(def) = solsp_hir::resolve::resolve(target) {
        let name_node = def.name_ptr.to_node(root);
        if is_yul_binding_name(&name_node)
            && name_node.text_range().start() > target.text_range().start()
        {
            return false;
        }
        return true;
    }
    if solsp_hir::resolve::top_level_definition(root, name, None).is_some() {
        return true;
    }
    if let Some(c) = enclosing_contract(target) {
        if inherited_member(state, uri, root, &c, name, None).is_some() {
            return true;
        }
    }
    cross_file_definition(state, uri, root, name, None).is_some()
}

fn name_defined(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    nr: &solsp_syntax::SyntaxNode,
    name: &str,
) -> bool {
    if is_builtin_name(name) {
        return true;
    }
    if solsp_hir::resolve::resolve(nr).is_some()
        || solsp_hir::resolve::top_level_definition(root, name, None).is_some()
    {
        return true;
    }
    if let Some(c) = enclosing_contract(nr) {
        if inherited_member(state, uri, root, &c, name, None).is_some() {
            return true;
        }
    }
    cross_file_definition(state, uri, root, name, None).is_some()
}

fn is_yul_binding_name(node: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{NAME, YUL_FUNCTION_DEF, YUL_PARAM_LIST, YUL_VAR_DECL};
    node.kind() == NAME
        && node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                YUL_VAR_DECL | YUL_PARAM_LIST | YUL_FUNCTION_DEF
            )
        })
}
