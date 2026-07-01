//! Hover, completion, and signature-help request handlers.

use super::*;

/// `textDocument/hover` -> the definition's signature + kind as markdown, if any.
pub(super) fn hover(state: &ServerState, params: HoverParams) -> Option<Hover> {
    let pos = params.text_document_position_params;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    if let Some(h) = yul_builtin_hover(&root, offset) {
        return Some(h);
    }
    if let Some(h) = named_arg_hover(state, &uri, &root, offset) {
        return Some(h);
    }
    if let Some((turi, def)) = typed_overload_target(state, &uri, &root, offset) {
        let troot = parse_root(state, &turi)?;
        return Some(markup_hover(
            solsp_ide::navigation::hover_text(&troot, &def),
            None,
        ));
    }
    if let Some(info) = solsp_ide::navigation::hover(&root, offset) {
        return Some(markup_hover(
            info.contents,
            Some(to_proto::range(li, info.range)),
        ));
    }
    if let Some((target_uri, def)) = member_resolve(state, &uri, &root, offset) {
        let troot = parse_root(state, &target_uri)?;
        return Some(markup_hover(
            solsp_ide::navigation::hover_text(&troot, &def),
            None,
        ));
    }
    if let Some((target_uri, def)) = inherited_name_at(state, &uri, &root, offset) {
        let troot = parse_root(state, &target_uri)?;
        return Some(markup_hover(
            solsp_ide::navigation::hover_text(&troot, &def),
            None,
        ));
    }
    if let Some(h) = builtin_member_hover(state, &uri, &root, offset) {
        return Some(h);
    }
    let name = solsp_ide::navigation::name_at(&root, offset)?;
    let arity = arity_at(&root, offset);
    if let Some((turi, def)) = cross_file_definition(state, &uri, &root, &name, arity) {
        let troot = parse_root(state, &turi)?;
        return Some(markup_hover(
            solsp_ide::navigation::hover_text(&troot, &def),
            None,
        ));
    }
    None
}

/// `textDocument/completion` -> member completion after `.`, else scope completion.
pub(super) fn completion(state: &ServerState, params: CompletionParams) -> CompletionResponse {
    CompletionResponse::Array(completion_items(state, &params).unwrap_or_default())
}

fn completion_items(state: &ServerState, params: &CompletionParams) -> Option<Vec<CompletionItem>> {
    let pos = &params.text_document_position;
    let uri = pos.text_document.uri.clone();
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    if is_inside_yul_block(&root, offset) {
        return Some(yul_completion_items(state, &uri, &root, offset));
    }
    if let Some(items) = named_arg_completion(state, &uri, &root, offset) {
        return Some(items);
    }
    if let Some(items) = member_completion(state, &uri, &root, offset) {
        return Some(items);
    }
    Some(scope_completion(state, &uri, &root, offset))
}

fn yul_completion_items(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Vec<CompletionItem> {
    let mut items = yul_builtin_items();
    items.extend(abi_selector_completion_items(state, uri, root, offset));
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| seen.insert(item.label.clone()));
    items
}

fn abi_selector_completion_items(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Vec<CompletionItem> {
    let node = root
        .token_at_offset(offset)
        .left_biased()
        .or_else(|| root.token_at_offset(offset).right_biased())
        .and_then(|token| token.parent())
        .unwrap_or_else(|| root.clone());
    let Some(contract) = enclosing_contract(&node) else {
        return Vec::new();
    };

    let mut items = Vec::new();
    for (_, decl) in abi_selector_decls(state, uri, root, &contract) {
        if let Some(item) = abi_selector_completion_item(&decl) {
            items.push(item);
        }
    }
    items
}

fn abi_selector_decls(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
) -> Vec<(Url, solsp_syntax::SyntaxNode)> {
    use std::collections::{HashSet, VecDeque};

    let mut out = Vec::new();
    let mut visited: HashSet<(Url, String)> = HashSet::new();
    let mut queue = VecDeque::from([(uri.clone(), root.clone(), contract.clone())]);
    while let Some((current_uri, current_root, current_contract)) = queue.pop_front() {
        let key = (
            current_uri.clone(),
            solsp_hir::resolve::contract_def_name(&current_contract).unwrap_or_default(),
        );
        if !visited.insert(key) {
            continue;
        }

        for def in solsp_hir::resolve::contract_members(&current_contract) {
            let decl = def.full_ptr.to_node(&current_root);
            if matches!(
                def.kind,
                solsp_hir::resolve::DefKind::Function
                    | solsp_hir::resolve::DefKind::Event
                    | solsp_hir::resolve::DefKind::Error
            ) {
                out.push((current_uri.clone(), decl));
            }
        }

        for base in solsp_hir::resolve::base_names(&current_contract) {
            let Some((base_uri, base_root, base_node)) =
                resolve_base(state, &current_uri, &current_root, &base)
            else {
                continue;
            };
            queue.push_back((base_uri, base_root, base_node));
        }
    }
    out
}

fn abi_selector_completion_item(decl: &solsp_syntax::SyntaxNode) -> Option<CompletionItem> {
    use solsp_syntax::SyntaxKind::{ERROR_DEF, EVENT_DEF, FUNCTION_DEF};

    let signature = abi::signature(decl)?;
    let (suffix, hex, detail) = match decl.kind() {
        FUNCTION_DEF if solsp_hir::resolve::is_externally_visible(decl) => (
            "selector",
            abi::function_selector_hex(decl)?,
            "function selector",
        ),
        FUNCTION_DEF => return None,
        ERROR_DEF => ("selector", abi::error_selector_hex(decl)?, "error selector"),
        EVENT_DEF => ("topic0", abi::event_topic_hex(decl)?, "event topic0"),
        _ => return None,
    };
    let insert_text = format!("0x{hex}");
    Some(CompletionItem {
        label: format!("{signature}.{suffix}"),
        kind: Some(CompletionItemKind::CONSTANT),
        detail: Some(format!("{detail}: {insert_text}")),
        insert_text: Some(insert_text),
        ..Default::default()
    })
}

fn member_completion(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<Vec<CompletionItem>> {
    let receiver = dotted_receiver(root, offset)?;
    let using_items = using_member_items(state, uri, root, &receiver);
    if let Some(turi) = namespace_target_uri(uri, root, &receiver) {
        if let Some(tfile) = state.file(&turi) {
            let troot = solsp_base_db::parse(state.db(), tfile).syntax();
            let mut defs = solsp_hir::resolve::file_definitions(&troot);
            let mut visited = std::collections::HashSet::new();
            collect_file_exports(state, &turi, &troot, &mut visited, &mut defs);
            return Some(completion_items_from(defs));
        }
    }
    if let Some((turi, tdef)) = resolve_receiver_type(state, uri, root, &receiver) {
        let Some(troot) = parse_root(state, &turi) else {
            return Some(completion_items_from(solsp_hir::resolve::type_members(
                &tdef,
            )));
        };
        let contract_like = matches!(tdef.kind(), solsp_syntax::SyntaxKind::CONTRACT_DEF);
        let library = contract_like && is_library_node(&tdef);
        let is_super = is_super_receiver(&receiver);
        let external = contract_like
            && !library
            && !is_super
            && is_instance_receiver(state, uri, root, &receiver);
        let keep = |node: &solsp_syntax::SyntaxNode| {
            if external {
                solsp_hir::resolve::is_externally_visible(node)
            } else if contract_like {
                !solsp_hir::resolve::is_private(node)
            } else {
                true
            }
        };
        let same_file = if is_super {
            Vec::new()
        } else {
            solsp_hir::resolve::type_members(&tdef)
                .into_iter()
                .filter(|d| keep(&d.full_ptr.to_node(&troot)))
                .collect()
        };
        let mut items = completion_items_from(same_file);
        if contract_like && !library {
            let inherited = if is_super {
                collect_base_members(state, &turi, &troot, &tdef, false)
            } else {
                collect_inherited_members(state, &turi, &troot, &tdef, external)
            };
            items.extend(completion_items_from(inherited));
        }
        if !is_super {
            items.extend(using_items);
        }
        let mut seen = std::collections::HashSet::new();
        items.retain(|i| seen.insert(i.label.clone()));
        return Some(items);
    }
    if let Some(items) = builtin_member_items(&receiver) {
        return Some(items);
    }
    if let Some(items) = type_expr_members(state, uri, root, &receiver) {
        return Some(items);
    }
    if let Some(mut items) = value_type_builtin_members(state, uri, root, &receiver) {
        items.extend(using_items);
        return Some(items);
    }
    if let Some(items) = selector_member(state, uri, root, &receiver) {
        return Some(items);
    }
    if !using_items.is_empty() {
        return Some(using_items);
    }
    Some(Vec::new())
}

fn yul_builtin_hover(root: &solsp_syntax::SyntaxNode, offset: rowan::TextSize) -> Option<Hover> {
    use solsp_syntax::SyntaxKind::{NAME_REF, YUL_BLOCK};
    let nr = root.token_at_offset(offset).find_map(|token| {
        token
            .parent_ancestors()
            .find(|node| node.kind() == NAME_REF)
    })?;
    if nr.ancestors().all(|node| node.kind() != YUL_BLOCK) {
        return None;
    }
    let text = nr.text().to_string();
    let builtin = yul_builtin(text.trim())?;
    Some(markup_hover(
        format!(
            "```yul\n{}\n```\n\n{}\n\n*(Yul builtin)*",
            builtin.signature, builtin.detail
        ),
        None,
    ))
}

fn is_inside_yul_block(root: &solsp_syntax::SyntaxNode, offset: rowan::TextSize) -> bool {
    use solsp_syntax::SyntaxKind::YUL_BLOCK;
    root.token_at_offset(offset)
        .left_biased()
        .or_else(|| root.token_at_offset(offset).right_biased())
        .and_then(|token| token.parent())
        .is_some_and(|node| {
            node.ancestors()
                .any(|ancestor| ancestor.kind() == YUL_BLOCK)
        })
}

fn builtin_member_hover(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<Hover> {
    use solsp_syntax::SyntaxKind::NAME_REF;
    let nr = root
        .token_at_offset(offset)
        .find_map(|t| t.parent_ancestors().find(|n| n.kind() == NAME_REF))?;
    let (receiver, member) = solsp_hir::resolve::member_access(&nr)?;
    let items = builtin_member_items(&receiver)
        .into_iter()
        .chain(value_type_builtin_members(state, uri, root, &receiver))
        .chain(type_expr_members(state, uri, root, &receiver))
        .chain(selector_member(state, uri, root, &receiver))
        .flatten();
    let item = items.into_iter().find(|i| i.label == member)?;
    let text = match item.detail.as_deref() {
        Some(d) if !d.is_empty() => format!("{member}: {d}"),
        _ => member.clone(),
    };
    Some(markup_hover(
        format!("```solidity\n{text}\n```\n\n*(builtin)*"),
        None,
    ))
}

fn selector_member(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<Vec<CompletionItem>> {
    use solsp_hir::resolve::DefKind;
    let def = resolve_receiver_def(state, uri, root, receiver)?;
    let ty = match def.kind {
        DefKind::Error | DefKind::Function => "bytes4",
        DefKind::Event => "bytes32",
        _ => return None,
    };
    Some(synthetic_members(&[("selector", ty, false)]))
}

fn scope_completion(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Vec<CompletionItem> {
    let node = root
        .token_at_offset(offset)
        .left_biased()
        .and_then(|t| t.parent())
        .unwrap_or_else(|| root.clone());
    let mut defs = solsp_hir::resolve::scope_definitions(&node);
    if let Some(contract) = enclosing_contract(&node) {
        defs.extend(collect_inherited_members(
            state, uri, root, &contract, false,
        ));
    }
    defs.extend(imported_symbols(state, uri, root));
    let mut items = completion_items_from(defs);
    items.extend(namespace_alias_items(root));
    items.extend(builtin_items());
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(i.label.clone()));
    items
}

pub(super) fn signature_help(
    state: &ServerState,
    params: SignatureHelpParams,
) -> Option<SignatureHelp> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, CALL_EXPR, COMMA};
    let pos = params.text_document_position_params;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();

    let tok = root.token_at_offset(offset).left_biased()?;
    let arg_list = tok.parent()?.ancestors().find(|n| n.kind() == ARG_LIST)?;
    let call = arg_list.parent()?;
    if call.kind() != CALL_EXPR {
        return None;
    }
    let callee = call.first_child()?;
    let name = callee_display_name(&callee)?;
    let (def_uri, def) = resolve_named_callee(state, &uri, &root, &callee)?;
    let droot = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&droot);
    let candidates = signature_candidates(&def, &def_node, &name, &droot);

    let active = arg_list
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == COMMA && t.text_range().start() < offset)
        .count() as u32;
    let signatures: Vec<SignatureInformation> = candidates
        .iter()
        .map(|(kind, node)| signature_info(&name, *kind, node, active))
        .collect();
    let arg_count = arg_list.children().count();
    let active_sig = candidates
        .iter()
        .position(|(kind, node)| named_arg_fields(*kind, node).len() == arg_count)
        .unwrap_or(0) as u32;
    Some(SignatureHelp {
        signatures,
        active_signature: Some(active_sig),
        active_parameter: Some(active),
    })
}

fn signature_info(
    name: &str,
    kind: solsp_hir::resolve::DefKind,
    node: &solsp_syntax::SyntaxNode,
    active: u32,
) -> SignatureInformation {
    let labels: Vec<String> = named_arg_fields(kind, node)
        .into_iter()
        .map(|(n, t)| match (n.is_empty(), t.is_empty()) {
            (true, _) => t,
            (_, true) => n,
            _ => format!("{t} {n}"),
        })
        .collect();
    let label = format!("{name}({})", labels.join(", "));
    let parameters = labels
        .into_iter()
        .map(|p| ParameterInformation {
            label: ParameterLabel::Simple(p),
            documentation: None,
        })
        .collect();
    SignatureInformation {
        label,
        documentation: None,
        parameters: Some(parameters),
        active_parameter: Some(active),
    }
}
