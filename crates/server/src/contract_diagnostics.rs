//! Contract-level diagnostics and quick-fix helpers.

use super::*;

pub(super) fn abstract_contract_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::CONTRACT_DEF;

    let mut out = Vec::new();
    for contract in root
        .descendants()
        .filter(|node| node.kind() == CONTRACT_DEF)
    {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        if !is_contract_contract(&contract) || is_abstract_contract(&contract) {
            continue;
        }

        let mut missing: Vec<String> = missing_inherited_functions(state, uri, root, &contract)
            .iter()
            .map(|function| function_label(&function.node))
            .collect();
        missing.extend(direct_abstract_function_labels(&contract));
        if missing.is_empty() {
            continue;
        }
        missing.sort();
        missing.dedup();

        let contract_name = solsp_hir::resolve::contract_def_name(&contract)
            .unwrap_or_else(|| "contract".to_string());
        let detail = if missing.len() == 1 {
            format!("missing function: `{}`", missing[0])
        } else {
            format!("missing functions: `{}`", missing.join("`, `"))
        };
        out.push(lsp_types::Diagnostic {
            range: to_proto::range(li, contract_name_range(&contract)),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            source: Some("solsp".to_string()),
            message: format!(
                "Contract `{contract_name}` must be marked abstract or implement {detail}"
            ),
            ..Default::default()
        });
    }
    out
}

pub(super) fn missing_visibility_diagnostics(
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{CONTRACT_DEF, FUNCTION_DEF};
    let mut out = Vec::new();
    for function in root
        .descendants()
        .filter(|node| node.kind() == FUNCTION_DEF)
    {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        if function.ancestors().all(|node| node.kind() != CONTRACT_DEF)
            || function_has_visibility(&function)
        {
            continue;
        }
        out.push(lsp_types::Diagnostic {
            range: to_proto::range(li, function_name_range(&function)),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            source: Some("solsp".to_string()),
            message: format!(
                "Function `{}` has no explicit visibility",
                function_label(&function)
            ),
            ..Default::default()
        });
    }
    out
}

#[derive(Clone)]
pub(super) struct MissingFunction {
    pub(super) name: String,
    pub(super) arity: usize,
    pub(super) node: solsp_syntax::SyntaxNode,
}

pub(super) fn function_has_visibility(function: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{EXTERNAL_KW, INTERNAL_KW, PRIVATE_KW, PUBLIC_KW};
    function
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| {
            matches!(
                token.kind(),
                PUBLIC_KW | EXTERNAL_KW | INTERNAL_KW | PRIVATE_KW
            )
        })
}

pub(super) fn function_has_override(function: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::OVERRIDE_KW;
    function
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| token.kind() == OVERRIDE_KW)
}

pub(super) fn missing_inherited_functions(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
) -> Vec<MissingFunction> {
    use solsp_hir::resolve::DefKind;
    use std::collections::{HashMap, HashSet, VecDeque};

    let mut implemented: HashSet<(String, usize)> = HashSet::new();
    let mut required: HashMap<(String, usize), MissingFunction> = HashMap::new();
    let mut queue: VecDeque<(
        Url,
        solsp_syntax::SyntaxNode,
        solsp_syntax::SyntaxNode,
        bool,
    )> = VecDeque::new();
    let mut visited: HashSet<(Url, String)> = HashSet::new();

    queue.push_back((uri.clone(), root.clone(), contract.clone(), false));
    while let Some((current_uri, current_root, current_contract, is_base)) = queue.pop_front() {
        let key = (
            current_uri.clone(),
            solsp_hir::resolve::contract_def_name(&current_contract).unwrap_or_default(),
        );
        if !visited.insert(key) {
            continue;
        }

        for def in solsp_hir::resolve::contract_members(&current_contract) {
            if def.kind != DefKind::Function {
                continue;
            }
            let node = def.full_ptr.to_node(&current_root);
            let Some(arity) = function_arity(&node) else {
                continue;
            };
            let signature_key = (def.name.clone(), arity);
            if is_abstract_function(&node) {
                if is_base {
                    required.entry(signature_key).or_insert(MissingFunction {
                        name: def.name,
                        arity,
                        node,
                    });
                }
            } else {
                implemented.insert(signature_key);
            }
        }

        for base in solsp_hir::resolve::base_names(&current_contract) {
            if let Some((base_uri, base_root, base_node)) =
                resolve_base(state, &current_uri, &current_root, &base)
            {
                queue.push_back((base_uri, base_root, base_node, true));
            }
        }
    }

    let mut missing: Vec<_> = required
        .into_iter()
        .filter_map(|(key, function)| (!implemented.contains(&key)).then_some(function))
        .collect();
    missing.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.arity.cmp(&right.arity))
    });
    missing
}

fn direct_abstract_function_labels(contract: &solsp_syntax::SyntaxNode) -> Vec<String> {
    use solsp_syntax::SyntaxKind::{CONTRACT_BODY, FUNCTION_DEF};
    contract
        .children()
        .find(|child| child.kind() == CONTRACT_BODY)
        .into_iter()
        .flat_map(|body| body.children())
        .filter(|child| child.kind() == FUNCTION_DEF && is_abstract_function(child))
        .map(|function| function_label(&function))
        .collect()
}

pub(super) fn function_label(function: &solsp_syntax::SyntaxNode) -> String {
    let name = function_name(function).unwrap_or_else(|| "function".to_string());
    let params = param_name_types(function)
        .into_iter()
        .map(|(_, ty)| ty)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({params})")
}

pub(super) fn function_name(function: &solsp_syntax::SyntaxNode) -> Option<String> {
    declaration_name(function)
}

pub(super) fn function_name_range(function: &solsp_syntax::SyntaxNode) -> rowan::TextRange {
    use solsp_syntax::SyntaxKind::{FALLBACK_KW, FUNCTION_KW, IDENT, NAME, RECEIVE_KW};
    if let Some(range) = function
        .children()
        .find(|child| child.kind() == NAME)
        .and_then(|name| {
            name.children_with_tokens()
                .filter_map(|element| element.into_token())
                .find(|token| token.kind() == IDENT)
        })
        .map(|token| token.text_range())
    {
        return range;
    }
    function
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| matches!(token.kind(), FUNCTION_KW | FALLBACK_KW | RECEIVE_KW))
        .map(|token| token.text_range())
        .unwrap_or_else(|| function.text_range())
}

pub(super) fn function_visibility(function: &solsp_syntax::SyntaxNode) -> Option<&'static str> {
    member_visibility(function)
}

pub(super) fn declaration_name(decl: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::NAME;
    decl.children()
        .find(|child| child.kind() == NAME)
        .and_then(|name| node_ident(&name))
}

pub(super) fn declaration_name_range(decl: &solsp_syntax::SyntaxNode) -> rowan::TextRange {
    use solsp_syntax::SyntaxKind::{IDENT, NAME};
    decl.children()
        .find(|child| child.kind() == NAME)
        .and_then(|name| {
            name.children_with_tokens()
                .filter_map(|element| element.into_token())
                .find(|token| token.kind() == IDENT)
        })
        .map(|token| token.text_range())
        .unwrap_or_else(|| decl.text_range())
}

pub(super) fn member_visibility(member: &solsp_syntax::SyntaxNode) -> Option<&'static str> {
    use solsp_syntax::SyntaxKind::{EXTERNAL_KW, INTERNAL_KW, PRIVATE_KW, PUBLIC_KW};
    member
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .find_map(|token| match token.kind() {
            PUBLIC_KW => Some("public"),
            EXTERNAL_KW => Some("external"),
            INTERNAL_KW => Some("internal"),
            PRIVATE_KW => Some("private"),
            _ => None,
        })
}

fn contract_name_range(contract: &solsp_syntax::SyntaxNode) -> rowan::TextRange {
    use solsp_syntax::SyntaxKind::{IDENT, NAME};
    contract
        .children()
        .find(|child| child.kind() == NAME)
        .and_then(|name| {
            name.children_with_tokens()
                .filter_map(|element| element.into_token())
                .find(|token| token.kind() == IDENT)
        })
        .map(|token| token.text_range())
        .unwrap_or_else(|| contract.text_range())
}

pub(super) fn is_contract_contract(contract: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{CONTRACT_KW, INTERFACE_KW, LIBRARY_KW};
    let mut has_contract = false;
    for token in contract
        .children_with_tokens()
        .filter_map(|element| element.into_token())
    {
        match token.kind() {
            CONTRACT_KW => has_contract = true,
            INTERFACE_KW | LIBRARY_KW => return false,
            _ => {}
        }
    }
    has_contract
}

pub(super) fn is_abstract_contract(contract: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::ABSTRACT_KW;
    contract
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| token.kind() == ABSTRACT_KW)
}

pub(super) fn is_abstract_function(function: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{BLOCK, SEMICOLON};
    let has_body = function.children().any(|child| child.kind() == BLOCK);
    let has_semicolon = function
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| token.kind() == SEMICOLON);
    !has_body && has_semicolon
}

pub(super) fn function_arity(function: &solsp_syntax::SyntaxNode) -> Option<usize> {
    use solsp_syntax::SyntaxKind::{PARAM, PARAM_LIST};
    let params = function
        .children()
        .find(|child| child.kind() == PARAM_LIST)?;
    Some(
        params
            .children()
            .filter(|child| child.kind() == PARAM)
            .count(),
    )
}

pub(super) fn contract_body_end_offset(
    contract: &solsp_syntax::SyntaxNode,
) -> Option<rowan::TextSize> {
    use solsp_syntax::SyntaxKind::{CONTRACT_BODY, R_BRACE};
    let body = contract
        .children()
        .find(|child| child.kind() == CONTRACT_BODY)?;
    body.children_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() == R_BRACE)
        .last()
        .map(|token| token.text_range().start())
}

pub(super) fn contract_body_has_members(contract: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::CONTRACT_BODY;
    contract
        .children()
        .find(|child| child.kind() == CONTRACT_BODY)
        .is_some_and(|body| body.children().next().is_some())
}
