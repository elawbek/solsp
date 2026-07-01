//! Usage-oriented diagnostics for declarations that appear unreferenced.

use super::*;

pub(super) fn unused_function_diagnostics(
    state: &ServerState,
    uri: &Url,
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
            || !matches!(function_visibility(&function), Some("private" | "internal"))
        {
            continue;
        }
        let Some(name) = function_name(&function) else {
            continue;
        };
        if overridden_base_function_is_referenced(state, uri, root, &function, &name) {
            continue;
        }
        let target = RefTarget {
            uri: uri.clone(),
            range: function_name_range(&function),
        };
        if has_reference_count_at_least(state, &name, &target, 2, true, false) {
            continue;
        }
        out.push(lsp_types::Diagnostic {
            range: to_proto::range(li, target.range),
            severity: Some(lsp_types::DiagnosticSeverity::WARNING),
            source: Some("solsp".to_string()),
            message: format!("function `{}` is never used", function_label(&function)),
            tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
            ..Default::default()
        });
    }
    out
}

pub(super) fn unused_state_variable_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{CONTRACT_DEF, STATE_VAR_DEF};

    let mut out = Vec::new();
    for var in root
        .descendants()
        .filter(|node| node.kind() == STATE_VAR_DEF)
    {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        if var.ancestors().all(|node| node.kind() != CONTRACT_DEF)
            || matches!(member_visibility(&var), Some("public"))
        {
            continue;
        }
        let Some(name) = declaration_name(&var) else {
            continue;
        };
        if name.to_ascii_lowercase().contains("deprecated") {
            continue;
        }
        let target = RefTarget {
            uri: uri.clone(),
            range: declaration_name_range(&var),
        };
        if has_reference_count_at_least(state, &name, &target, 2, true, false) {
            continue;
        }
        out.push(lsp_types::Diagnostic {
            range: to_proto::range(li, target.range),
            severity: Some(lsp_types::DiagnosticSeverity::WARNING),
            source: Some("solsp".to_string()),
            message: format!("state variable `{name}` is never used"),
            tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
            ..Default::default()
        });
    }
    out
}

pub(super) fn unused_event_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{CONTRACT_DEF, EVENT_DEF};

    let mut out = Vec::new();
    for event in root.descendants().filter(|node| node.kind() == EVENT_DEF) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        if event.ancestors().all(|node| node.kind() != CONTRACT_DEF) {
            continue;
        }
        let Some(name) = declaration_name(&event) else {
            continue;
        };
        let target = RefTarget {
            uri: uri.clone(),
            range: declaration_name_range(&event),
        };
        if has_reference_count_at_least(state, &name, &target, 2, true, true) {
            continue;
        }
        if abi::event_topic_hex(&event).is_some_and(|topic| abi::yul_contains_hex(root, &topic)) {
            continue;
        }
        out.push(lsp_types::Diagnostic {
            range: to_proto::range(li, target.range),
            severity: Some(lsp_types::DiagnosticSeverity::WARNING),
            source: Some("solsp".to_string()),
            message: format!("event `{name}` is never emitted"),
            tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
            ..Default::default()
        });
    }
    out
}

pub(super) fn unused_error_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{CONTRACT_DEF, ERROR_DEF};

    let mut out = Vec::new();
    for error in root.descendants().filter(|node| node.kind() == ERROR_DEF) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        if error.ancestors().all(|node| node.kind() != CONTRACT_DEF) {
            continue;
        }
        let Some(name) = declaration_name(&error) else {
            continue;
        };
        let target = RefTarget {
            uri: uri.clone(),
            range: declaration_name_range(&error),
        };
        if has_reference_count_at_least(state, &name, &target, 2, true, true) {
            continue;
        }
        if abi::error_selector_hex(&error)
            .is_some_and(|selector| abi::yul_contains_hex(root, &selector))
        {
            continue;
        }
        out.push(lsp_types::Diagnostic {
            range: to_proto::range(li, target.range),
            severity: Some(lsp_types::DiagnosticSeverity::WARNING),
            source: Some("solsp".to_string()),
            message: format!("error `{name}` is never used"),
            tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
            ..Default::default()
        });
    }
    out
}

fn overridden_base_function_is_referenced(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    function: &solsp_syntax::SyntaxNode,
    name: &str,
) -> bool {
    if !function_has_override(function) {
        return false;
    }
    let Some(arity) = function_arity(function) else {
        return false;
    };
    let Some(contract) = enclosing_contract(function) else {
        return false;
    };
    let Some((base_uri, base_root, base_def)) =
        overridden_base_function(state, uri, root, &contract, name, arity)
    else {
        return false;
    };
    let target = RefTarget {
        uri: base_uri,
        range: def_name_range(&base_root, &base_def),
    };
    has_reference_count_at_least(state, name, &target, 1, false, false)
}

fn overridden_base_function(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: usize,
) -> Option<(
    Url,
    solsp_syntax::SyntaxNode,
    solsp_hir::resolve::Definition,
)> {
    use std::collections::{HashSet, VecDeque};

    let mut queue: VecDeque<(Url, solsp_syntax::SyntaxNode, solsp_syntax::SyntaxNode)> =
        VecDeque::new();
    let mut visited: HashSet<(Url, String)> = HashSet::new();
    queue.push_back((uri.clone(), root.clone(), contract.clone()));

    while let Some((current_uri, current_root, current_contract)) = queue.pop_front() {
        let key = (
            current_uri.clone(),
            solsp_hir::resolve::contract_def_name(&current_contract).unwrap_or_default(),
        );
        if !visited.insert(key) {
            continue;
        }

        for base in solsp_hir::resolve::base_names(&current_contract) {
            let Some((base_uri, base_root, base_node)) =
                resolve_base(state, &current_uri, &current_root, &base)
            else {
                continue;
            };
            if let Some(def) = solsp_hir::resolve::contract_member(&base_node, name, Some(arity)) {
                return Some((base_uri, base_root, def));
            }
            queue.push_back((base_uri, base_root, base_node));
        }
    }
    None
}
