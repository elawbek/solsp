//! Workspace references, rename edits, ABI selector references, and reference code lenses.

use super::*;

const LARGE_FILE_CODE_LENS_LINE_LIMIT: usize = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefTarget {
    pub(crate) uri: Url,
    pub(crate) range: rowan::TextRange,
}

/// `textDocument/references` → every loaded reference resolving to the same declaration.
pub(super) fn references(state: &ServerState, params: ReferenceParams) -> Option<Vec<Location>> {
    let started = std::time::Instant::now();
    let pos = params.text_document_position;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    let query_name = solsp_ide::navigation::name_at(&root, offset)?;
    let target = reference_target_at(state, &uri, &root, offset)?;
    let locations = reference_locations(
        state,
        &query_name,
        &target,
        params.context.include_declaration,
        true,
    );
    let count = locations.len();
    crate::perf::log_elapsed(
        || format!("references `{query_name}` -> {count} locations"),
        started,
    );
    Some(locations)
}

/// `textDocument/rename` → workspace edit over every loaded reference to the symbol.
pub(super) fn rename(state: &ServerState, params: RenameParams) -> Option<WorkspaceEdit> {
    if !is_valid_rename_identifier(&params.new_name) {
        return None;
    }
    let pos = params.text_document_position;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    let query_name = solsp_ide::navigation::name_at(&root, offset)?;
    let target = reference_target_at(state, &uri, &root, offset)?;
    let locations = reference_locations(state, &query_name, &target, true, false);
    if locations.is_empty() {
        return None;
    }

    let mut changes: std::collections::HashMap<Url, Vec<TextEdit>> =
        std::collections::HashMap::new();
    for loc in locations {
        changes
            .entry(loc.uri)
            .or_default()
            .push(TextEdit::new(loc.range, params.new_name.clone()));
    }
    if let Some((old_hex, new_hex)) = reference_abi_rename_hex(state, &target, &params.new_name) {
        let new_text = format!("0x{new_hex}");
        for loc in reference_abi_hex_locations(state, &target, &old_hex) {
            changes
                .entry(loc.uri.clone())
                .or_default()
                .push(TextEdit::new(loc.range, new_text.clone()));
        }
    }
    Some(WorkspaceEdit::new(changes))
}

fn is_valid_rename_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return false;
    }
    if !chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric()) {
        return false;
    }
    solsp_syntax::SyntaxKind::from_keyword(name).is_none()
}

pub(crate) fn reference_locations(
    state: &ServerState,
    query_name: &str,
    target: &RefTarget,
    include_declaration: bool,
    include_abi_hex: bool,
) -> Vec<Location> {
    let mut locations = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate_uri in state.loaded_uris() {
        if !state.text_contains(&candidate_uri, query_name) {
            continue;
        }
        let Some(ranges) = state.identifier_ranges(&candidate_uri, query_name) else {
            continue;
        };
        if ranges.is_empty() {
            continue;
        }
        let Some(candidate_file) = state.file(&candidate_uri) else {
            continue;
        };
        let Some(candidate_li) = state.line_index(&candidate_uri) else {
            continue;
        };
        let candidate_root = solsp_base_db::parse(state.db(), candidate_file).syntax();
        for range in ranges {
            let Some(found) =
                reference_target_at(state, &candidate_uri, &candidate_root, range.start())
            else {
                continue;
            };
            if found != *target {
                continue;
            }
            if !include_declaration && candidate_uri == target.uri && range == target.range {
                continue;
            }
            let key = format!(
                "{}:{}..{}",
                candidate_uri,
                u32::from(range.start()),
                u32::from(range.end())
            );
            if seen.insert(key) {
                locations.push(Location {
                    uri: candidate_uri.clone(),
                    range: to_proto::range(candidate_li, range),
                });
            }
        }
    }
    if include_abi_hex {
        if let Some(hex) = reference_abi_hex(state, target) {
            for loc in reference_abi_hex_locations(state, target, &hex) {
                let key = format!(
                    "{}:{}:{}..{}:{}",
                    loc.uri,
                    loc.range.start.line,
                    loc.range.start.character,
                    loc.range.end.line,
                    loc.range.end.character
                );
                if seen.insert(key) {
                    locations.push(loc);
                }
            }
        }
    }
    locations
}

pub(crate) fn has_reference_count_at_least(
    state: &ServerState,
    query_name: &str,
    target: &RefTarget,
    min_count: usize,
    include_declaration: bool,
    include_abi_hex: bool,
) -> bool {
    if min_count == 0 {
        return true;
    }

    let mut count = 0usize;
    let mut seen = std::collections::HashSet::new();
    for candidate_uri in state.loaded_uris() {
        if !state.text_contains(&candidate_uri, query_name) {
            continue;
        }
        let Some(ranges) = state.identifier_ranges(&candidate_uri, query_name) else {
            continue;
        };
        if ranges.is_empty() {
            continue;
        }
        let Some(candidate_file) = state.file(&candidate_uri) else {
            continue;
        };
        let candidate_root = solsp_base_db::parse(state.db(), candidate_file).syntax();
        for range in ranges {
            let Some(found) =
                reference_target_at(state, &candidate_uri, &candidate_root, range.start())
            else {
                continue;
            };
            if found != *target {
                continue;
            }
            if !include_declaration && candidate_uri == target.uri && range == target.range {
                continue;
            }
            let key = (
                candidate_uri.to_string(),
                u32::from(range.start()),
                u32::from(range.end()),
            );
            if seen.insert(key) {
                count += 1;
                if count >= min_count {
                    return true;
                }
            }
        }
    }

    if include_abi_hex {
        if let Some(hex) = reference_abi_hex(state, target) {
            for loc in reference_abi_hex_locations(state, target, &hex) {
                let key = (
                    loc.uri.to_string(),
                    loc.range.start.line,
                    loc.range.start.character,
                );
                if seen.insert(key) {
                    count += 1;
                    if count >= min_count {
                        return true;
                    }
                }
            }
        }
    }

    false
}

fn reference_abi_hex(state: &ServerState, target: &RefTarget) -> Option<String> {
    use solsp_syntax::SyntaxKind::{ERROR_DEF, EVENT_DEF, IDENT};

    let root = parse_root(state, &target.uri)?;
    let token = root
        .token_at_offset(target.range.start())
        .find(|token| token.kind() == IDENT && token.text_range() == target.range)?;
    let decl = token
        .parent_ancestors()
        .find(|node| matches!(node.kind(), ERROR_DEF | EVENT_DEF))?;
    match decl.kind() {
        ERROR_DEF => abi::error_selector_hex(&decl),
        EVENT_DEF => abi::event_topic_hex(&decl),
        _ => None,
    }
}

fn reference_abi_hex_locations(
    state: &ServerState,
    target: &RefTarget,
    hex: &str,
) -> Vec<Location> {
    let Some((target_owner_uri, target_owner_name)) = reference_abi_owner(state, target) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate_uri in state.loaded_uris() {
        let Some(candidate_file) = state.file(&candidate_uri) else {
            continue;
        };
        let Some(candidate_li) = state.line_index(&candidate_uri) else {
            continue;
        };
        let candidate_root = solsp_base_db::parse(state.db(), candidate_file).syntax();
        for range in abi::yul_hex_ranges(&candidate_root, hex) {
            let Some(contract) = node_at_range(&candidate_root, range).and_then(|node| {
                node.ancestors()
                    .find(|ancestor| ancestor.kind() == solsp_syntax::SyntaxKind::CONTRACT_DEF)
            }) else {
                continue;
            };
            if !contract_inherits_target(
                state,
                &candidate_uri,
                &candidate_root,
                &contract,
                &target_owner_uri,
                &target_owner_name,
            ) {
                continue;
            }
            let key = format!(
                "{}:{}..{}",
                candidate_uri,
                u32::from(range.start()),
                u32::from(range.end())
            );
            if seen.insert(key) {
                out.push(Location {
                    uri: candidate_uri.clone(),
                    range: to_proto::range(candidate_li, range),
                });
            }
        }
    }
    out
}

fn reference_abi_owner(state: &ServerState, target: &RefTarget) -> Option<(Url, String)> {
    let root = parse_root(state, &target.uri)?;
    let token = root
        .token_at_offset(target.range.start())
        .find(|token| token.text_range() == target.range)?;
    let owner = token
        .parent_ancestors()
        .find(|node| node.kind() == solsp_syntax::SyntaxKind::CONTRACT_DEF)?;
    let name = solsp_hir::resolve::contract_def_name(&owner)?;
    Some((target.uri.clone(), name))
}

fn node_at_range(
    root: &solsp_syntax::SyntaxNode,
    range: rowan::TextRange,
) -> Option<solsp_syntax::SyntaxNode> {
    root.token_at_offset(range.start())
        .find(|token| token.text_range() == range)
        .and_then(|token| token.parent())
}

fn contract_inherits_target(
    state: &ServerState,
    candidate_uri: &Url,
    candidate_root: &solsp_syntax::SyntaxNode,
    candidate_contract: &solsp_syntax::SyntaxNode,
    target_owner_uri: &Url,
    target_owner_name: &str,
) -> bool {
    use std::collections::{HashSet, VecDeque};

    let Some(candidate_name) = solsp_hir::resolve::contract_def_name(candidate_contract) else {
        return false;
    };
    if candidate_uri == target_owner_uri && candidate_name == target_owner_name {
        return true;
    }

    let mut visited = HashSet::new();
    let mut queue = VecDeque::from([(
        candidate_uri.clone(),
        candidate_root.clone(),
        candidate_contract.clone(),
    )]);
    while let Some((uri, root, contract)) = queue.pop_front() {
        for base in solsp_hir::resolve::base_names(&contract) {
            let Some((base_uri, base_root, base_node)) = resolve_base(state, &uri, &root, &base)
            else {
                continue;
            };
            let Some(base_name) = solsp_hir::resolve::contract_def_name(&base_node) else {
                continue;
            };
            if base_uri == *target_owner_uri && base_name == target_owner_name {
                return true;
            }
            if visited.insert((base_uri.clone(), base_name)) {
                queue.push_back((base_uri, base_root, base_node));
            }
        }
    }
    false
}

fn reference_abi_rename_hex(
    state: &ServerState,
    target: &RefTarget,
    new_name: &str,
) -> Option<(String, String)> {
    use solsp_syntax::SyntaxKind::{ERROR_DEF, EVENT_DEF, IDENT};

    let root = parse_root(state, &target.uri)?;
    let token = root
        .token_at_offset(target.range.start())
        .find(|token| token.kind() == IDENT && token.text_range() == target.range)?;
    let decl = token
        .parent_ancestors()
        .find(|node| matches!(node.kind(), ERROR_DEF | EVENT_DEF))?;
    match decl.kind() {
        ERROR_DEF => Some((
            abi::error_selector_hex(&decl)?,
            abi::error_selector_hex_for_name(&decl, Some(new_name))?,
        )),
        EVENT_DEF => Some((
            abi::event_topic_hex(&decl)?,
            abi::event_topic_hex_for_name(&decl, Some(new_name))?,
        )),
        _ => None,
    }
}

/// `textDocument/codeLens` → inline reference counts above declarations.
pub(super) fn code_lens(state: &ServerState, params: CodeLensParams) -> Option<Vec<CodeLens>> {
    let uri = params.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let reference_counts_enabled = state
        .line_count(&uri)
        .is_none_or(|lines| lines <= LARGE_FILE_CODE_LENS_LINE_LIMIT);
    let root = solsp_base_db::parse(state.db(), file).syntax();

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for decl in root.descendants().filter(is_code_lens_declaration) {
        let Some(name_range) = code_lens_name_range(&decl) else {
            continue;
        };
        let Some(query_name) = declaration_name_for_lens(&decl) else {
            continue;
        };
        if !seen.insert(name_range) {
            continue;
        }
        let target = RefTarget {
            uri: uri.clone(),
            range: name_range,
        };
        let position = to_proto::range(li, name_range).start;
        if reference_counts_enabled {
            out.push(CodeLens {
                range: lsp_types::Range {
                    start: position,
                    end: position,
                },
                command: None,
                data: Some(serde_json::json!({
                    "uri": target.uri,
                    "queryName": query_name,
                    "targetStart": u32::from(target.range.start()),
                    "targetEnd": u32::from(target.range.end()),
                })),
            });
        }
        if let Some(command) = code_lens_graph_command_for_decl(&decl, &uri, position) {
            out.push(CodeLens {
                range: lsp_types::Range {
                    start: position,
                    end: position,
                },
                command: Some(command),
                data: None,
            });
        }
    }
    Some(out)
}

fn is_code_lens_declaration(node: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{
        CONTRACT_DEF, ENUM_DEF, ERROR_DEF, EVENT_DEF, FUNCTION_DEF, MODIFIER_DEF, STATE_VAR_DEF,
        STRUCT_DEF, USER_DEFINED_VALUE_TYPE,
    };
    matches!(
        node.kind(),
        CONTRACT_DEF
            | FUNCTION_DEF
            | MODIFIER_DEF
            | STATE_VAR_DEF
            | STRUCT_DEF
            | ENUM_DEF
            | EVENT_DEF
            | ERROR_DEF
            | USER_DEFINED_VALUE_TYPE
    )
}

fn code_lens_name_range(decl: &solsp_syntax::SyntaxNode) -> Option<rowan::TextRange> {
    use solsp_syntax::SyntaxKind::{CONTRACT_DEF, FUNCTION_DEF, MODIFIER_DEF, NAME};
    match decl.kind() {
        CONTRACT_DEF => decl
            .children()
            .find(|child| child.kind() == NAME)
            .and_then(ident_range),
        FUNCTION_DEF | MODIFIER_DEF => decl
            .children()
            .find(|child| child.kind() == NAME)
            .and_then(ident_range),
        _ => decl
            .children()
            .find_map(|child| (child.kind() == NAME).then(|| ident_range(child)).flatten()),
    }
}

fn ident_range(name: solsp_syntax::SyntaxNode) -> Option<rowan::TextRange> {
    use solsp_syntax::SyntaxKind::IDENT;
    name.children_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| token.kind() == IDENT)
        .map(|token| token.text_range())
}

fn declaration_name_for_lens(decl: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::{CONTRACT_DEF, FUNCTION_DEF, MODIFIER_DEF};
    match decl.kind() {
        CONTRACT_DEF => solsp_hir::resolve::contract_def_name(decl),
        FUNCTION_DEF | MODIFIER_DEF => function_name(decl),
        _ => declaration_name(decl),
    }
}

fn code_lens_graph_command_for_decl(
    decl: &solsp_syntax::SyntaxNode,
    uri: &Url,
    position: lsp_types::Position,
) -> Option<Command> {
    use solsp_syntax::SyntaxKind::{CONTRACT_DEF, FUNCTION_DEF, MODIFIER_DEF};
    let (title, command) = match decl.kind() {
        CONTRACT_DEF => ("inheritance graph", "solsp.showInheritanceGraph"),
        FUNCTION_DEF | MODIFIER_DEF => ("call graph", "solsp.showFunctionCallGraph"),
        _ => return None,
    };
    Some(Command {
        title: title.to_string(),
        command: command.to_string(),
        arguments: Some(vec![serde_json::json!({
            "uri": uri,
            "position": position,
        })]),
    })
}

pub(super) fn code_lens_resolve(state: &ServerState, mut lens: CodeLens) -> CodeLens {
    let Some(data) = lens.data.as_ref() else {
        return lens;
    };
    let Some(uri) = data
        .get("uri")
        .and_then(|value| value.as_str())
        .and_then(|uri| Url::parse(uri).ok())
    else {
        return lens;
    };
    let Some(query_name) = data.get("queryName").and_then(|value| value.as_str()) else {
        return lens;
    };
    let Some(target_start) = data
        .get("targetStart")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
    else {
        return lens;
    };
    let Some(target_end) = data
        .get("targetEnd")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
    else {
        return lens;
    };
    let target = RefTarget {
        uri: uri.clone(),
        range: rowan::TextRange::new(
            rowan::TextSize::from(target_start),
            rowan::TextSize::from(target_end),
        ),
    };
    let locations = reference_locations(state, query_name, &target, true, true);
    let title = match locations.len() {
        1 => "1 reference".to_string(),
        n => format!("{n} references"),
    };
    lens.command = Some(Command {
        title,
        command: "solsp.showReferences".to_string(),
        arguments: Some(vec![serde_json::json!({
            "uri": uri,
            "position": lens.range.start,
            "locations": locations,
        })]),
    });
    lens
}
