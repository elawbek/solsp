use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    SymbolKind, Url,
};

use crate::{
    arg_count, callee_display_name, declaration_name, declaration_name_range, definition_target,
    enclosing_contract, function_name_range, inherited_name_at, is_cheatcode_receiver,
    member_resolve, parse_root, resolve_base, resolve_callee, state::ServerState, to_proto,
    typed_overload_target, RefTarget,
};

pub(crate) fn call_hierarchy_prepare(
    state: &ServerState,
    params: CallHierarchyPrepareParams,
) -> Option<Vec<CallHierarchyItem>> {
    let pos = params.text_document_position_params;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    let item = call_hierarchy_target_at(state, &uri, &root, offset)
        .or_else(|| enclosing_call_hierarchy_item(&uri, &root, li, offset))?;
    Some(vec![item])
}

pub(crate) fn call_hierarchy_incoming(
    state: &ServerState,
    params: CallHierarchyIncomingCallsParams,
) -> Option<Vec<CallHierarchyIncomingCall>> {
    let target = call_hierarchy_ref_target(state, &params.item)?;
    let target_name = params.item.name;
    let mut out: Vec<CallHierarchyIncomingCall> = Vec::new();
    for uri in state.loaded_uris() {
        if state
            .text(&uri)
            .is_some_and(|text| !text.contains(&target_name))
        {
            continue;
        }
        let Some(root) = parse_root(state, &uri) else {
            continue;
        };
        let Some(li) = state.line_index(&uri) else {
            continue;
        };
        for call in root
            .descendants()
            .filter(|node| node.kind() == solsp_syntax::SyntaxKind::CALL_EXPR)
        {
            let Some(callee) = call.first_child() else {
                continue;
            };
            if is_cheatcode_receiver(&callee)
                || callee_display_name(&callee).as_deref() != Some(target_name.as_str())
            {
                continue;
            }
            let Some((turi, def)) = resolve_callee(state, &uri, &root, &callee, arg_count(&call))
            else {
                continue;
            };
            let Some(found) = definition_target(state, turi, &def) else {
                continue;
            };
            if found != target {
                continue;
            }
            let Some(caller) = enclosing_function_like(&call) else {
                continue;
            };
            let Some(from) = call_hierarchy_item_for_decl(&uri, li, &caller) else {
                continue;
            };
            let range = to_proto::range(li, callee.text_range());
            if let Some(existing) = out.iter_mut().find(|call| call.from.data == from.data) {
                existing.from_ranges.push(range);
            } else {
                out.push(CallHierarchyIncomingCall {
                    from,
                    from_ranges: vec![range],
                });
            }
        }
    }
    Some(out)
}

pub(crate) fn call_hierarchy_outgoing(
    state: &ServerState,
    params: CallHierarchyOutgoingCallsParams,
) -> Option<Vec<CallHierarchyOutgoingCall>> {
    let target = call_hierarchy_ref_target(state, &params.item)?;
    let root = parse_root(state, &target.uri)?;
    let li = state.line_index(&target.uri)?;
    let owner = function_like_by_name_range(&root, target.range)?;
    let mut out: Vec<CallHierarchyOutgoingCall> = Vec::new();
    for call in owner
        .descendants()
        .filter(|node| node.kind() == solsp_syntax::SyntaxKind::CALL_EXPR)
    {
        let Some(callee) = call.first_child() else {
            continue;
        };
        if is_cheatcode_receiver(&callee) {
            continue;
        }
        let Some((turi, def)) =
            resolve_callee(state, &target.uri, &root, &callee, arg_count(&call))
        else {
            continue;
        };
        if !call_hierarchy_def_kind(def.kind) {
            continue;
        }
        let Some(to) = call_hierarchy_item_for_def(state, turi, &def) else {
            continue;
        };
        let range = to_proto::range(li, callee.text_range());
        if let Some(existing) = out.iter_mut().find(|call| call.to.data == to.data) {
            existing.from_ranges.push(range);
        } else {
            out.push(CallHierarchyOutgoingCall {
                to,
                from_ranges: vec![range],
            });
        }
    }
    Some(out)
}

pub(crate) fn function_call_graph(
    state: &ServerState,
    params: serde_json::Value,
) -> serde_json::Value {
    let Some((start_uri, start_range)) = function_graph_start(state, &params) else {
        return graph_json(Vec::new(), Vec::new(), "flowchart TD");
    };
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut seen_nodes = std::collections::HashSet::new();
    let mut seen_edges = std::collections::HashSet::new();
    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::from([(start_uri, start_range, 0usize)]);

    while let Some((uri, range, depth)) = queue.pop_front() {
        if depth > 8 || !visited.insert((uri.clone(), range)) {
            continue;
        }
        let Some(root) = parse_root(state, &uri) else {
            continue;
        };
        let Some(li) = state.line_index(&uri) else {
            continue;
        };
        let Some(owner) = function_like_by_name_range(&root, range) else {
            continue;
        };
        let Some(from_id) =
            function_graph_add_node(&mut nodes, &mut seen_nodes, &uri, li, &owner, depth == 0)
        else {
            continue;
        };
        for call in owner
            .descendants()
            .filter(|node| node.kind() == solsp_syntax::SyntaxKind::CALL_EXPR)
        {
            let Some(callee) = call.first_child() else {
                continue;
            };
            if is_cheatcode_receiver(&callee) {
                continue;
            }
            let Some((turi, def)) = resolve_callee(state, &uri, &root, &callee, arg_count(&call))
            else {
                continue;
            };
            if !call_hierarchy_def_kind(def.kind) {
                continue;
            }
            let Some(troot) = parse_root(state, &turi) else {
                continue;
            };
            let Some(tli) = state.line_index(&turi) else {
                continue;
            };
            let target_decl = def.full_ptr.to_node(&troot);
            let Some(to_id) = function_graph_add_node(
                &mut nodes,
                &mut seen_nodes,
                &turi,
                tli,
                &target_decl,
                false,
            ) else {
                continue;
            };
            if seen_edges.insert((from_id.clone(), to_id.clone())) {
                edges.push(serde_json::json!({
                    "from": from_id,
                    "to": to_id,
                    "kind": "calls",
                }));
            }
            queue.push_back((turi, call_hierarchy_name_range(&target_decl), depth + 1));
        }
    }

    let mut mermaid = vec!["flowchart TD".to_string()];
    for node in &nodes {
        if let (Some(id), Some(name)) = (
            node.get("id").and_then(|value| value.as_str()),
            node.get("name").and_then(|value| value.as_str()),
        ) {
            mermaid.push(format!("  {id}[\"{name}\"]"));
        }
    }
    for edge in &edges {
        if let (Some(from), Some(to)) = (
            edge.get("from").and_then(|value| value.as_str()),
            edge.get("to").and_then(|value| value.as_str()),
        ) {
            mermaid.push(format!("  {from} --> {to}"));
        }
    }

    graph_json(nodes, edges, &mermaid.join("\n"))
}

pub(crate) fn inheritance_graph(
    state: &ServerState,
    params: serde_json::Value,
) -> serde_json::Value {
    let focus_uri = params
        .get("textDocument")
        .and_then(|doc| doc.get("uri"))
        .and_then(|uri| uri.as_str())
        .and_then(|uri| Url::parse(uri).ok());
    let focus_contract = focus_uri
        .as_ref()
        .and_then(|uri| inheritance_graph_contract_at(state, uri, &params));
    let visible = focus_contract
        .as_ref()
        .map(|(uri, name)| inheritance_graph_ancestors(state, uri, name));
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut mermaid = vec!["classDiagram".to_string()];
    let mut seen_nodes = std::collections::HashSet::new();
    let mut seen_edges = std::collections::HashSet::new();

    for uri in state.loaded_uris() {
        let Some(root) = parse_root(state, &uri) else {
            continue;
        };
        let Some(li) = state.line_index(&uri) else {
            continue;
        };
        for contract in root
            .children()
            .filter(|node| node.kind() == solsp_syntax::SyntaxKind::CONTRACT_DEF)
        {
            let Some(name) = solsp_hir::resolve::contract_def_name(&contract) else {
                continue;
            };
            if visible
                .as_ref()
                .is_some_and(|set| !set.contains(&(uri.clone(), name.clone())))
            {
                continue;
            }
            let id = inheritance_graph_node_id(&uri, &name);
            if seen_nodes.insert(id.clone()) {
                mermaid.push(format!("  class {id}[\"{name}\"]"));
                nodes.push(serde_json::json!({
                    "id": id,
                    "name": name,
                    "uri": uri,
                    "range": to_proto::range(li, contract.text_range()),
                    "focus": focus_contract.as_ref() == Some(&(uri.clone(), name.clone())),
                }));
            }
            for base in solsp_hir::resolve::base_names(&contract) {
                let (base_uri, base_name) = resolve_base(state, &uri, &root, &base)
                    .and_then(|(base_uri, _, base_node)| {
                        let name = solsp_hir::resolve::contract_def_name(&base_node)?;
                        Some((base_uri, name))
                    })
                    .unwrap_or_else(|| (uri.clone(), base.clone()));
                if visible
                    .as_ref()
                    .is_some_and(|set| !set.contains(&(base_uri.clone(), base_name.clone())))
                {
                    continue;
                }
                let base_id = inheritance_graph_node_id(&base_uri, &base_name);
                if seen_nodes.insert(base_id.clone()) {
                    mermaid.push(format!("  class {base_id}[\"{base_name}\"]"));
                    nodes.push(serde_json::json!({
                        "id": base_id,
                        "name": base_name,
                        "uri": base_uri,
                        "range": lsp_types::Range::default(),
                        "focus": focus_contract.as_ref() == Some(&(base_uri.clone(), base_name.clone())),
                    }));
                }
                let edge_key = format!("{base_id}->{id}");
                if seen_edges.insert(edge_key) {
                    mermaid.push(format!("  {base_id} <|-- {id}"));
                    edges.push(serde_json::json!({
                        "from": id,
                        "to": base_id,
                        "kind": "inherits",
                    }));
                }
            }
        }
    }

    graph_json(nodes, edges, &mermaid.join("\n"))
}

fn function_graph_start(
    state: &ServerState,
    params: &serde_json::Value,
) -> Option<(Url, rowan::TextRange)> {
    let uri = params
        .get("textDocument")
        .and_then(|doc| doc.get("uri"))
        .and_then(|uri| uri.as_str())
        .and_then(|uri| Url::parse(uri).ok())?;
    let line = params.get("position")?.get("line")?.as_u64()?;
    let character = params.get("position")?.get("character")?.as_u64()?;
    let li = state.line_index(&uri)?;
    let position = lsp_types::Position {
        line: u32::try_from(line).ok()?,
        character: u32::try_from(character).ok()?,
    };
    let offset = to_proto::offset(li, position)?;
    let root = parse_root(state, &uri)?;
    if let Some(item) = call_hierarchy_target_at(state, &uri, &root, offset) {
        let target = call_hierarchy_ref_target(state, &item)?;
        return Some((target.uri, target.range));
    }
    let item = enclosing_call_hierarchy_item(&uri, &root, li, offset)?;
    let target = call_hierarchy_ref_target(state, &item)?;
    Some((target.uri, target.range))
}

fn function_graph_add_node(
    nodes: &mut Vec<serde_json::Value>,
    seen_nodes: &mut std::collections::HashSet<String>,
    uri: &Url,
    li: &solsp_ide::LineIndex,
    decl: &solsp_syntax::SyntaxNode,
    focus: bool,
) -> Option<String> {
    let name = call_hierarchy_decl_name(decl)?;
    let contract = enclosing_contract(decl).and_then(|node| declaration_name(&node));
    let label = contract
        .as_ref()
        .map(|contract| format!("{contract}.{name}"))
        .unwrap_or(name);
    let selection = call_hierarchy_name_range(decl);
    let id = function_graph_node_id(uri, selection, &label);
    if seen_nodes.insert(id.clone()) {
        nodes.push(serde_json::json!({
            "id": id,
            "name": label,
            "uri": uri,
            "range": to_proto::range(li, decl.text_range()),
            "focus": focus,
        }));
    }
    Some(id)
}

fn function_graph_node_id(uri: &Url, range: rowan::TextRange, label: &str) -> String {
    let mut id = format!(
        "{}_{}_{}",
        label,
        u32::from(range.start()),
        fx_hash(uri.as_str())
    );
    id.retain(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if id.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        id.insert(0, '_');
    }
    id
}

fn graph_json(
    nodes: Vec<serde_json::Value>,
    edges: Vec<serde_json::Value>,
    mermaid: &str,
) -> serde_json::Value {
    serde_json::json!({
        "mermaid": mermaid,
        "nodes": nodes,
        "edges": edges,
    })
}

fn inheritance_graph_contract_at(
    state: &ServerState,
    uri: &Url,
    params: &serde_json::Value,
) -> Option<(Url, String)> {
    let line = params.get("position")?.get("line")?.as_u64()?;
    let character = params.get("position")?.get("character")?.as_u64()?;
    let li = state.line_index(uri)?;
    let offset = to_proto::offset(
        li,
        lsp_types::Position {
            line: u32::try_from(line).ok()?,
            character: u32::try_from(character).ok()?,
        },
    )?;
    let root = parse_root(state, uri)?;
    let token = root
        .token_at_offset(offset)
        .left_biased()
        .or_else(|| root.token_at_offset(offset).right_biased())?;
    let contract = token
        .parent()?
        .ancestors()
        .find(|node| node.kind() == solsp_syntax::SyntaxKind::CONTRACT_DEF)?;
    let name = solsp_hir::resolve::contract_def_name(&contract)?;
    Some((uri.clone(), name))
}

fn inheritance_graph_ancestors(
    state: &ServerState,
    start_uri: &Url,
    start_name: &str,
) -> std::collections::HashSet<(Url, String)> {
    let mut visible = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::from([(start_uri.clone(), start_name.to_string())]);
    while let Some((uri, name)) = queue.pop_front() {
        if !visible.insert((uri.clone(), name.clone())) {
            continue;
        }
        let Some(root) = parse_root(state, &uri) else {
            continue;
        };
        let Some(contract) = solsp_hir::resolve::find_contract(&root, &name) else {
            continue;
        };
        for base in solsp_hir::resolve::base_names(&contract) {
            let Some((base_uri, _, base_node)) = resolve_base(state, &uri, &root, &base) else {
                queue.push_back((uri.clone(), base));
                continue;
            };
            if let Some(base_name) = solsp_hir::resolve::contract_def_name(&base_node) {
                queue.push_back((base_uri, base_name));
            }
        }
    }
    visible
}

fn inheritance_graph_node_id(uri: &Url, name: &str) -> String {
    let mut id = format!("{name}_{}", fx_hash(uri.as_str()));
    id.retain(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if id.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        id.insert(0, '_');
    }
    id
}

fn fx_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:x}")
}

fn call_hierarchy_target_at(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<CallHierarchyItem> {
    if let Some((turi, def)) = typed_overload_target(state, uri, root, offset) {
        return call_hierarchy_item_for_def(state, turi, &def);
    }
    if let Some(def) = solsp_hir::resolve::definition_at(root, offset) {
        if call_hierarchy_def_kind(def.kind) {
            return call_hierarchy_item_for_def(state, uri.clone(), &def);
        }
    }
    if let Some((turi, def)) = member_resolve(state, uri, root, offset) {
        if call_hierarchy_def_kind(def.kind) {
            return call_hierarchy_item_for_def(state, turi, &def);
        }
    }
    if let Some((turi, def)) = inherited_name_at(state, uri, root, offset) {
        if call_hierarchy_def_kind(def.kind) {
            return call_hierarchy_item_for_def(state, turi, &def);
        }
    }
    None
}

fn call_hierarchy_item_for_def(
    state: &ServerState,
    uri: Url,
    def: &solsp_hir::resolve::Definition,
) -> Option<CallHierarchyItem> {
    if !call_hierarchy_def_kind(def.kind) {
        return None;
    }
    let root = parse_root(state, &uri)?;
    let li = state.line_index(&uri)?;
    let decl = def.full_ptr.to_node(&root);
    call_hierarchy_item_for_decl(&uri, li, &decl)
}

fn call_hierarchy_item_for_decl(
    uri: &Url,
    li: &solsp_ide::LineIndex,
    decl: &solsp_syntax::SyntaxNode,
) -> Option<CallHierarchyItem> {
    if !call_hierarchy_decl_kind(decl.kind()) {
        return None;
    }
    let selection = call_hierarchy_name_range(decl);
    Some(CallHierarchyItem {
        name: call_hierarchy_decl_name(decl)?,
        kind: call_hierarchy_symbol_kind(decl.kind()),
        tags: None,
        detail: enclosing_contract(decl).and_then(|contract| declaration_name(&contract)),
        uri: uri.clone(),
        range: to_proto::range(li, decl.text_range()),
        selection_range: to_proto::range(li, selection),
        data: Some(serde_json::json!({
            "targetStart": u32::from(selection.start()),
            "targetEnd": u32::from(selection.end()),
        })),
    })
}

fn call_hierarchy_ref_target(state: &ServerState, item: &CallHierarchyItem) -> Option<RefTarget> {
    if let Some(data) = &item.data {
        let start = data
            .get("targetStart")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())?;
        let end = data
            .get("targetEnd")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())?;
        return Some(RefTarget {
            uri: item.uri.clone(),
            range: rowan::TextRange::new(start.into(), end.into()),
        });
    }
    let li = state.line_index(&item.uri)?;
    Some(RefTarget {
        uri: item.uri.clone(),
        range: rowan::TextRange::new(
            to_proto::offset(li, item.selection_range.start)?,
            to_proto::offset(li, item.selection_range.end)?,
        ),
    })
}

fn enclosing_call_hierarchy_item(
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    offset: rowan::TextSize,
) -> Option<CallHierarchyItem> {
    let token = root
        .token_at_offset(offset)
        .left_biased()
        .or_else(|| root.token_at_offset(offset).right_biased())?;
    let decl = enclosing_function_like(&token.parent()?)?;
    call_hierarchy_item_for_decl(uri, li, &decl)
}

fn enclosing_function_like(node: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    node.ancestors()
        .find(|ancestor| call_hierarchy_decl_kind(ancestor.kind()))
}

fn function_like_by_name_range(
    root: &solsp_syntax::SyntaxNode,
    range: rowan::TextRange,
) -> Option<solsp_syntax::SyntaxNode> {
    root.descendants()
        .filter(|node| call_hierarchy_decl_kind(node.kind()))
        .find(|node| call_hierarchy_name_range(node) == range)
}

fn call_hierarchy_name_range(decl: &solsp_syntax::SyntaxNode) -> rowan::TextRange {
    use solsp_syntax::SyntaxKind::{CONSTRUCTOR_DEF, FUNCTION_DEF, MODIFIER_DEF};
    match decl.kind() {
        FUNCTION_DEF | CONSTRUCTOR_DEF | MODIFIER_DEF => function_name_range(decl),
        _ => declaration_name_range(decl),
    }
}

fn call_hierarchy_decl_name(decl: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::{CONSTRUCTOR_DEF, FUNCTION_DEF, MODIFIER_DEF};
    match decl.kind() {
        FUNCTION_DEF | MODIFIER_DEF => declaration_name(decl).or_else(|| {
            decl.children_with_tokens()
                .filter_map(|element| element.into_token())
                .find(|token| {
                    matches!(
                        token.kind(),
                        solsp_syntax::SyntaxKind::FALLBACK_KW
                            | solsp_syntax::SyntaxKind::RECEIVE_KW
                    )
                })
                .map(|token| token.text().to_string())
        }),
        CONSTRUCTOR_DEF => Some("constructor".to_string()),
        _ => None,
    }
}

fn call_hierarchy_decl_kind(kind: solsp_syntax::SyntaxKind) -> bool {
    use solsp_syntax::SyntaxKind::{CONSTRUCTOR_DEF, FUNCTION_DEF, MODIFIER_DEF};
    matches!(kind, FUNCTION_DEF | CONSTRUCTOR_DEF | MODIFIER_DEF)
}

fn call_hierarchy_def_kind(kind: solsp_hir::resolve::DefKind) -> bool {
    use solsp_hir::resolve::DefKind::{Function, Modifier};
    matches!(kind, Function | Modifier)
}

fn call_hierarchy_symbol_kind(kind: solsp_syntax::SyntaxKind) -> SymbolKind {
    use solsp_syntax::SyntaxKind::{CONSTRUCTOR_DEF, FUNCTION_DEF, MODIFIER_DEF};
    match kind {
        CONSTRUCTOR_DEF => SymbolKind::CONSTRUCTOR,
        FUNCTION_DEF | MODIFIER_DEF => SymbolKind::FUNCTION,
        _ => SymbolKind::FUNCTION,
    }
}
