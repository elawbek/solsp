//! `solsp-server` library: the LSP protocol layer (capabilities, dispatch loop,
//! handlers) over the pure `solsp-ide` features. The `solsp-server` binary is a thin
//! shim around [`run`]; integration tests drive the same code over an in-memory
//! transport (design §5, §6).

use anyhow::Result;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as _,
};
use lsp_types::request::{
    CodeActionRequest, CodeLensRequest, CodeLensResolve, Completion, DocumentSymbolRequest,
    GotoDefinition, HoverRequest, References, Rename, Request as _, SemanticTokensFullRequest,
    SignatureHelpRequest,
};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
    CodeLens, CodeLensParams, Command, CompletionItem, CompletionItemKind, CompletionParams,
    CompletionResponse, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverParams, Location, ParameterInformation, ParameterLabel,
    ReferenceParams, RenameParams, SemanticTokensParams, SemanticTokensResult, SignatureHelp,
    SignatureHelpParams, SignatureInformation, TextEdit, Url, WorkspaceEdit,
};

mod abi;
mod builtins;
mod capabilities;
mod completion_items;
mod diagnostics;
mod import_surface;
mod named_args;
mod protocol;
pub mod state;
mod syntax_utils;
pub mod to_proto;
pub mod typecheck;
mod using_for;

pub use capabilities::server_capabilities;

use builtins::{
    builtin_items, builtin_member_items, is_builtin_name, is_fixed_bytes, is_integer_type_name,
    synthetic_members, yul_builtin, yul_builtin_items,
};
use completion_items::completion_items_from;
use diagnostics::{publish_diagnostics, send_diagnostics};
use import_surface::{collect_file_exports, imported_symbols, namespace_alias_items};
use named_args::{named_arg_completion, named_arg_fields, named_arg_hover};
use protocol::{apply_change, extract_err_response, extract_notification, markup_hover};
use state::ServerState;
use syntax_utils::{
    indexed_type_text, named_type, node_ident, node_type_text, param_name_types, type_text,
};
use using_for::{using_member, using_member_items};

/// Run the main loop until the client shuts the connection down. Assumes the
/// `initialize`/`initialized` handshake has already completed.
pub fn run(connection: &Connection) -> Result<()> {
    run_with_root(connection, None)
}

/// Like [`run`], but first pre-loads every `.sol` file under `workspace_root` so the first
/// open of any file is already parsed (its imports too). The main binary passes the
/// editor's workspace root; tests pass `None`.
pub fn run_with_root(
    connection: &Connection,
    workspace_root: Option<std::path::PathBuf>,
) -> Result<()> {
    let mut state = ServerState::default();
    // Project files to warm and diagnose in the background, one per idle tick so the whole
    // project's problems appear in the editor's tree without ever blocking the loop (the db
    // is `!Send`, so this cooperative scan replaces a worker thread). A real request always
    // preempts scanning; a file's own open/save still refreshes it.
    let mut scan_queue = workspace_root
        .map(|root| state::collect_sol_files(&root))
        .unwrap_or_default();
    let mut scan_pos = 0usize;

    loop {
        let msg = if scan_pos < scan_queue.len() {
            match connection.receiver.try_recv() {
                Ok(msg) => msg,
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    // idle: warm + diagnose the next file, then re-check for messages.
                    let uri = scan_queue[scan_pos].clone();
                    scan_pos += 1;
                    state.ensure_loaded(&uri);
                    state.load_import_graph(&uri);
                    publish_diagnostics(
                        connection,
                        &state,
                        &uri,
                        true,
                        Some(std::time::Duration::from_millis(150)),
                    )?;
                    if scan_pos >= scan_queue.len() {
                        scan_queue = Vec::new(); // done — free the list
                    }
                    continue;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
            }
        } else {
            match connection.receiver.recv() {
                Ok(msg) => msg,
                Err(_) => return Ok(()),
            }
        };
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                // A panicking handler must not take the whole server down: catch it and
                // reply with an error so the session keeps working.
                let id = req.id.clone();
                let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle_request(&state, req)
                }))
                .unwrap_or_else(|_| {
                    eprintln!("solsp: request handler panicked (id={id})");
                    Response::new_err(
                        id,
                        ErrorCode::InternalError as i32,
                        "internal error (handler panicked)".to_string(),
                    )
                });
                connection.sender.send(Message::Response(resp))?;
            }
            Message::Notification(not) => {
                handle_notification(connection, &mut state, not)?;
            }
            Message::Response(_resp) => {}
        }
    }
}

/// Answer a request, or reply `MethodNotFound` for anything we do not handle. Both
/// handlers degrade gracefully: an unknown document yields an empty result.
fn handle_request(state: &ServerState, req: Request) -> Response {
    // Capture the id up front: `extract` consumes `req`, and a `JsonError` from it
    // carries no id, so we must remember the one to echo on the error reply.
    let id = req.id.clone();
    match req.method.as_str() {
        DocumentSymbolRequest::METHOD => {
            match req.extract::<DocumentSymbolParams>(DocumentSymbolRequest::METHOD) {
                Ok((id, params)) => Response::new_ok(id, document_symbols(state, params)),
                Err(e) => extract_err_response(id, e),
            }
        }
        SemanticTokensFullRequest::METHOD => {
            match req.extract::<SemanticTokensParams>(SemanticTokensFullRequest::METHOD) {
                Ok((id, params)) => Response::new_ok(id, semantic_tokens(state, params)),
                Err(e) => extract_err_response(id, e),
            }
        }
        GotoDefinition::METHOD => match req.extract::<GotoDefinitionParams>(GotoDefinition::METHOD)
        {
            Ok((id, params)) => Response::new_ok(id, goto_definition(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        References::METHOD => match req.extract::<ReferenceParams>(References::METHOD) {
            Ok((id, params)) => Response::new_ok(id, references(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        Rename::METHOD => match req.extract::<RenameParams>(Rename::METHOD) {
            Ok((id, params)) => Response::new_ok(id, rename(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        CodeLensRequest::METHOD => match req.extract::<CodeLensParams>(CodeLensRequest::METHOD) {
            Ok((id, params)) => Response::new_ok(id, code_lens(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        CodeLensResolve::METHOD => match req.extract::<CodeLens>(CodeLensResolve::METHOD) {
            Ok((id, lens)) => Response::new_ok(id, code_lens_resolve(state, lens)),
            Err(e) => extract_err_response(id, e),
        },
        CodeActionRequest::METHOD => {
            match req.extract::<CodeActionParams>(CodeActionRequest::METHOD) {
                Ok((id, params)) => Response::new_ok(id, code_action(state, params)),
                Err(e) => extract_err_response(id, e),
            }
        }
        HoverRequest::METHOD => match req.extract::<HoverParams>(HoverRequest::METHOD) {
            Ok((id, params)) => Response::new_ok(id, hover(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        Completion::METHOD => match req.extract::<CompletionParams>(Completion::METHOD) {
            Ok((id, params)) => Response::new_ok(id, completion(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        SignatureHelpRequest::METHOD => {
            match req.extract::<SignatureHelpParams>(SignatureHelpRequest::METHOD) {
                Ok((id, params)) => Response::new_ok(id, signature_help(state, params)),
                Err(e) => extract_err_response(id, e),
            }
        }
        _ => Response::new_err(
            id,
            ErrorCode::MethodNotFound as i32,
            format!("unhandled request: {}", req.method),
        ),
    }
}

/// `textDocument/documentSymbol` → nested outline (empty if the doc is not open).
fn document_symbols(state: &ServerState, params: DocumentSymbolParams) -> DocumentSymbolResponse {
    let uri = params.text_document.uri;
    let symbols = match (state.file(&uri), state.line_index(&uri)) {
        (Some(file), Some(li)) => {
            let root = solsp_base_db::parse(state.db(), file).syntax();
            let bare = solsp_ide::document_symbols::document_symbols(&root);
            to_proto::document_symbols(&bare, li)
        }
        _ => Vec::new(),
    };
    DocumentSymbolResponse::Nested(symbols)
}

/// `textDocument/semanticTokens/full` → delta-encoded tokens (or `None` if unopened).
fn semantic_tokens(
    state: &ServerState,
    params: SemanticTokensParams,
) -> Option<SemanticTokensResult> {
    let uri = params.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let parse = solsp_base_db::parse(state.db(), file);
    let bare = solsp_ide::semantic_tokens::semantic_tokens(&parse.syntax());
    let tokens = to_proto::semantic_tokens(&bare, file.text(state.db()), li);
    Some(SemanticTokensResult::Tokens(tokens))
}

/// `textDocument/definition` → the declaration's name range, as a same-file
/// `Location` (or `None` if nothing resolves under the cursor).
fn goto_definition(
    state: &ServerState,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let pos = params.text_document_position_params;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    // 0. an overloaded call where argument *types* (not just count) pick the overload —
    //    must run before same-file arity-only resolution, which would pick the wrong one.
    if let Some((turi, def)) = typed_overload_target(state, &uri, &root, offset) {
        let troot = parse_root(state, &turi)?;
        let tli = state.line_index(&turi)?;
        let range = to_proto::range(tli, def_name_range(&troot, &def));
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: turi,
            range,
        }));
    }
    // 1. same-file resolution.
    if let Some(target) = solsp_ide::navigation::goto_definition(&root, offset) {
        let range = to_proto::range(li, target);
        return Some(GotoDefinitionResponse::Scalar(Location { uri, range }));
    }
    // 2. member access `receiver.member` → resolve via the receiver's type.
    if let Some((target_uri, def)) = member_resolve(state, &uri, &root, offset) {
        let troot = parse_root(state, &target_uri)?;
        let tli = state.line_index(&target_uri)?;
        let range = to_proto::range(tli, def_name_range(&troot, &def));
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range,
        }));
    }
    // 2b. a bare name inherited from a cross-file base contract (e.g. a forge-std `Test`
    //     helper or `vm`).
    if let Some((target_uri, def)) = inherited_name_at(state, &uri, &root, offset) {
        let troot = parse_root(state, &target_uri)?;
        let tli = state.line_index(&target_uri)?;
        let range = to_proto::range(tli, def_name_range(&troot, &def));
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range,
        }));
    }
    // 3. an imported top-level symbol (a use site, or a name inside `{ ... }`) → jump
    //    into the target file.
    if let Some(name) = solsp_ide::navigation::name_at(&root, offset) {
        let arity = arity_at(&root, offset);
        if let Some((target_uri, range)) = cross_file_target(state, &uri, &root, &name, arity) {
            let tli = state.line_index(&target_uri)?;
            return Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range: to_proto::range(tli, range),
            }));
        }
    }
    // 3. the cursor is inside an import directive (its path string / `from`) → open
    //    the imported file at its start.
    let imp = solsp_hir::imports::import_at(&root, offset)?;
    let target_uri = state::resolve_import_uri(&uri, &imp.path)?;
    state.file(&target_uri)?; // ensure it is loaded
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: target_uri,
        range: lsp_types::Range::default(),
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefTarget {
    uri: Url,
    range: rowan::TextRange,
}

/// `textDocument/references` → every loaded reference resolving to the same declaration.
fn references(state: &ServerState, params: ReferenceParams) -> Option<Vec<Location>> {
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
    eprintln!(
        "solsp: references `{query_name}` -> {} locations in {:?}",
        locations.len(),
        started.elapsed()
    );
    Some(locations)
}

/// `textDocument/rename` → workspace edit over every loaded reference to the symbol.
fn rename(state: &ServerState, params: RenameParams) -> Option<WorkspaceEdit> {
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

fn abstract_contract_diagnostics(
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

fn missing_visibility_diagnostics(
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

fn unused_function_diagnostics(
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
        if reference_locations(state, &name, &target, true, false).len() > 1 {
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

fn unused_state_variable_diagnostics(
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
        if reference_locations(state, &name, &target, true, false).len() > 1 {
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

fn unused_event_diagnostics(
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
        if reference_locations(state, &name, &target, true, true).len() > 1 {
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

fn unused_error_diagnostics(
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
        if reference_locations(state, &name, &target, true, true).len() > 1 {
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
    !reference_locations(state, name, &target, false, false).is_empty()
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

/// `textDocument/codeAction` → quick fixes for concrete contracts that still owe
/// abstract/interface functions.
fn code_action(state: &ServerState, params: CodeActionParams) -> Option<CodeActionResponse> {
    let uri = params.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, params.range.start)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    let contract = contract_at_offset(&root, offset)?;
    if !is_contract_contract(&contract) {
        return Some(Vec::new());
    }

    let mut actions = Vec::new();
    if let Some(function) = function_at_offset(&root, offset) {
        if !function_has_visibility(&function) {
            actions_extend_visibility(&mut actions, &uri, li, &function);
        }
    }

    let missing = missing_inherited_functions(state, &uri, &root, &contract);
    let needs_abstract = !missing.is_empty() || has_direct_abstract_function(&contract);

    if !missing.is_empty() {
        if let Some(edit) = implement_missing_edit(&uri, li, &contract, &missing) {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Implement missing inherited functions".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(edit),
                is_preferred: Some(true),
                ..Default::default()
            }));
        }
    }

    if needs_abstract && !is_abstract_contract(&contract) {
        if let Some(edit) = mark_abstract_edit(&uri, li, &contract) {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Mark contract abstract".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(edit),
                ..Default::default()
            }));
        }
    }

    Some(actions)
}

#[derive(Clone)]
struct MissingFunction {
    name: String,
    arity: usize,
    node: solsp_syntax::SyntaxNode,
}

fn contract_at_offset(
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::CONTRACT_DEF;
    if let Some(token) = root.token_at_offset(offset).find(|t| {
        let range = t.text_range();
        range.start() <= offset && offset <= range.end()
    }) {
        if let Some(contract) = token.parent().and_then(|n| {
            n.ancestors()
                .find(|ancestor| ancestor.kind() == CONTRACT_DEF)
        }) {
            return Some(contract);
        }
    }
    root.descendants()
        .filter(|node| node.kind() == CONTRACT_DEF)
        .find(|node| {
            let range = node.text_range();
            range.start() <= offset && offset <= range.end()
        })
}

fn function_at_offset(
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::FUNCTION_DEF;
    if let Some(token) = root.token_at_offset(offset).find(|token| {
        let range = token.text_range();
        range.start() <= offset && offset <= range.end()
    }) {
        if let Some(function) = token.parent().and_then(|node| {
            node.ancestors()
                .find(|ancestor| ancestor.kind() == FUNCTION_DEF)
        }) {
            return Some(function);
        }
    }
    root.descendants()
        .filter(|node| node.kind() == FUNCTION_DEF)
        .find(|node| {
            let range = node.text_range();
            range.start() <= offset && offset <= range.end()
        })
}

fn actions_extend_visibility(
    actions: &mut CodeActionResponse,
    uri: &Url,
    li: &solsp_ide::LineIndex,
    function: &solsp_syntax::SyntaxNode,
) {
    for visibility in ["public", "external", "internal", "private"] {
        if let Some(edit) = add_visibility_edit(uri, li, function, visibility) {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Add {visibility} visibility"),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(edit),
                ..Default::default()
            }));
        }
    }
}

fn add_visibility_edit(
    uri: &Url,
    li: &solsp_ide::LineIndex,
    function: &solsp_syntax::SyntaxNode,
    visibility: &str,
) -> Option<WorkspaceEdit> {
    let offset = visibility_insert_offset(function)?;
    let position = to_proto::range(li, rowan::TextRange::new(offset, offset)).start;
    let mut changes = std::collections::HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit::new(
            lsp_types::Range {
                start: position,
                end: position,
            },
            format!(" {visibility}"),
        )],
    );
    Some(WorkspaceEdit::new(changes))
}

fn visibility_insert_offset(function: &solsp_syntax::SyntaxNode) -> Option<rowan::TextSize> {
    use solsp_syntax::SyntaxKind::PARAM_LIST;
    function
        .children()
        .find(|child| child.kind() == PARAM_LIST)
        .map(|params| params.text_range().end())
}

fn function_has_visibility(function: &solsp_syntax::SyntaxNode) -> bool {
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

fn function_has_override(function: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::OVERRIDE_KW;
    function
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| token.kind() == OVERRIDE_KW)
}

fn missing_inherited_functions(
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

fn function_label(function: &solsp_syntax::SyntaxNode) -> String {
    let name = function_name(function).unwrap_or_else(|| "function".to_string());
    let params = param_name_types(function)
        .into_iter()
        .map(|(_, ty)| ty)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({params})")
}

fn function_name(function: &solsp_syntax::SyntaxNode) -> Option<String> {
    declaration_name(function)
}

fn function_name_range(function: &solsp_syntax::SyntaxNode) -> rowan::TextRange {
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

fn function_visibility(function: &solsp_syntax::SyntaxNode) -> Option<&'static str> {
    member_visibility(function)
}

fn declaration_name(decl: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::NAME;
    decl.children()
        .find(|child| child.kind() == NAME)
        .and_then(|name| node_ident(&name))
}

fn declaration_name_range(decl: &solsp_syntax::SyntaxNode) -> rowan::TextRange {
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

fn member_visibility(member: &solsp_syntax::SyntaxNode) -> Option<&'static str> {
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

fn implement_missing_edit(
    uri: &Url,
    li: &solsp_ide::LineIndex,
    contract: &solsp_syntax::SyntaxNode,
    missing: &[MissingFunction],
) -> Option<WorkspaceEdit> {
    let insert_offset = contract_body_end_offset(contract)?;
    let position = to_proto::range(li, rowan::TextRange::new(insert_offset, insert_offset)).start;
    let mut new_text = String::new();
    let separator = if contract_body_has_members(contract) {
        "\n\n    "
    } else {
        "\n    "
    };
    for (index, function) in missing.iter().enumerate() {
        if index == 0 {
            new_text.push_str(separator);
        } else {
            new_text.push_str("\n\n    ");
        }
        new_text.push_str(&function_stub(&function.node)?);
    }
    new_text.push('\n');

    let mut changes = std::collections::HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit::new(
            lsp_types::Range {
                start: position,
                end: position,
            },
            new_text,
        )],
    );
    Some(WorkspaceEdit::new(changes))
}

fn mark_abstract_edit(
    uri: &Url,
    li: &solsp_ide::LineIndex,
    contract: &solsp_syntax::SyntaxNode,
) -> Option<WorkspaceEdit> {
    use solsp_syntax::SyntaxKind::CONTRACT_KW;
    let contract_kw = contract
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| token.kind() == CONTRACT_KW)?;
    let offset = contract_kw.text_range().start();
    let position = to_proto::range(li, rowan::TextRange::new(offset, offset)).start;
    let mut changes = std::collections::HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit::new(
            lsp_types::Range {
                start: position,
                end: position,
            },
            "abstract ".to_string(),
        )],
    );
    Some(WorkspaceEdit::new(changes))
}

fn function_stub(function: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::{
        BLOCK, L_BRACE, OVERRIDE_KW, RETURNS_KW, SEMICOLON, VIRTUAL_KW,
    };
    let mut tokens = Vec::new();
    let mut saw_override = false;
    let mut inserted_override = false;

    for token in function
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
    {
        match token.kind() {
            SEMICOLON | L_BRACE => break,
            BLOCK => break,
            VIRTUAL_KW => continue,
            OVERRIDE_KW => saw_override = true,
            RETURNS_KW if !saw_override && !inserted_override => {
                tokens.push((OVERRIDE_KW, "override".to_string()));
                inserted_override = true;
            }
            _ => {}
        }
        if !token.kind().is_trivia() {
            tokens.push((token.kind(), token.text().to_string()));
        }
    }
    if tokens.is_empty() {
        return None;
    }
    if !saw_override && !inserted_override {
        tokens.push((OVERRIDE_KW, "override".to_string()));
    }

    let mut signature = join_solidity_tokens(tokens);
    signature.push_str(" {\n        revert(\"Not implemented\");\n    }");
    Some(signature)
}

fn join_solidity_tokens(tokens: Vec<(solsp_syntax::SyntaxKind, String)>) -> String {
    use solsp_syntax::SyntaxKind::{COMMA, DOT, L_BRACK, L_PAREN, RETURNS_KW, R_BRACK, R_PAREN};
    let mut out = String::new();
    let mut prev_kind = None;
    let mut prev_text = String::new();

    for (kind, text) in tokens {
        if !out.is_empty() && needs_space_between(prev_kind, &prev_text, kind) {
            out.push(' ');
        }
        out.push_str(&text);
        prev_kind = Some(kind);
        prev_text = text;
    }

    fn needs_space_between(
        prev_kind: Option<solsp_syntax::SyntaxKind>,
        prev_text: &str,
        kind: solsp_syntax::SyntaxKind,
    ) -> bool {
        let Some(prev_kind) = prev_kind else {
            return false;
        };
        if matches!(kind, COMMA | R_PAREN | R_BRACK | DOT) {
            return false;
        }
        if matches!(prev_kind, L_PAREN | L_BRACK | DOT) {
            return false;
        }
        if kind == L_PAREN && prev_text != "returns" {
            return false;
        }
        if prev_kind == RETURNS_KW {
            return true;
        }
        true
    }

    out
}

fn has_direct_abstract_function(contract: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{CONTRACT_BODY, FUNCTION_DEF};
    contract
        .children()
        .find(|child| child.kind() == CONTRACT_BODY)
        .into_iter()
        .flat_map(|body| body.children())
        .any(|child| child.kind() == FUNCTION_DEF && is_abstract_function(&child))
}

fn is_contract_contract(contract: &solsp_syntax::SyntaxNode) -> bool {
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

fn is_abstract_contract(contract: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::ABSTRACT_KW;
    contract
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| token.kind() == ABSTRACT_KW)
}

fn is_abstract_function(function: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{BLOCK, SEMICOLON};
    let has_body = function.children().any(|child| child.kind() == BLOCK);
    let has_semicolon = function
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| token.kind() == SEMICOLON);
    !has_body && has_semicolon
}

fn function_arity(function: &solsp_syntax::SyntaxNode) -> Option<usize> {
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

fn contract_body_end_offset(contract: &solsp_syntax::SyntaxNode) -> Option<rowan::TextSize> {
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

fn contract_body_has_members(contract: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::CONTRACT_BODY;
    contract
        .children()
        .find(|child| child.kind() == CONTRACT_BODY)
        .is_some_and(|body| body.children().next().is_some())
}

fn reference_locations(
    state: &ServerState,
    query_name: &str,
    target: &RefTarget,
    include_declaration: bool,
    include_abi_hex: bool,
) -> Vec<Location> {
    let mut locations = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate_uri in state.loaded_uris() {
        if state
            .text(&candidate_uri)
            .is_some_and(|text| !text.contains(query_name))
        {
            continue;
        }
        let Some(candidate_file) = state.file(&candidate_uri) else {
            continue;
        };
        let Some(candidate_li) = state.line_index(&candidate_uri) else {
            continue;
        };
        let candidate_root = solsp_base_db::parse(state.db(), candidate_file).syntax();
        for token in candidate_root
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == solsp_syntax::SyntaxKind::IDENT)
            .filter(|t| t.text() == query_name)
        {
            let Some(found) = reference_target_at(
                state,
                &candidate_uri,
                &candidate_root,
                token.text_range().start(),
            ) else {
                continue;
            };
            if found != *target {
                continue;
            }
            if !include_declaration
                && candidate_uri == target.uri
                && token.text_range() == target.range
            {
                continue;
            }
            let key = format!(
                "{}:{}..{}",
                candidate_uri,
                u32::from(token.text_range().start()),
                u32::from(token.text_range().end())
            );
            if seen.insert(key) {
                locations.push(Location {
                    uri: candidate_uri.clone(),
                    range: to_proto::range(candidate_li, token.text_range()),
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
fn code_lens(state: &ServerState, params: CodeLensParams) -> Option<Vec<CodeLens>> {
    let uri = params.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for token in root
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == solsp_syntax::SyntaxKind::IDENT)
    {
        let Some(def) = solsp_hir::resolve::definition_at(&root, token.text_range().start()) else {
            continue;
        };
        if !is_code_lens_definition(def.kind) || def_name_range(&root, &def) != token.text_range() {
            continue;
        }
        if !seen.insert(token.text_range()) {
            continue;
        }
        let target = RefTarget {
            uri: uri.clone(),
            range: token.text_range(),
        };
        let position = to_proto::range(li, token.text_range()).start;
        out.push(CodeLens {
            range: lsp_types::Range {
                start: position,
                end: position,
            },
            command: None,
            data: Some(serde_json::json!({
                "uri": target.uri,
                "queryName": token.text(),
                "targetStart": u32::from(target.range.start()),
                "targetEnd": u32::from(target.range.end()),
            })),
        });
    }
    Some(out)
}

fn code_lens_resolve(state: &ServerState, mut lens: CodeLens) -> CodeLens {
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

fn is_code_lens_definition(kind: solsp_hir::resolve::DefKind) -> bool {
    use solsp_hir::resolve::DefKind::*;
    !matches!(kind, Local | Parameter | Field | Variant)
}

/// `textDocument/hover` → the definition's signature + kind as markdown (or `None`).
fn hover(state: &ServerState, params: HoverParams) -> Option<Hover> {
    let pos = params.text_document_position_params;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    // Inline assembly uses Yul/EVM builtins rather than Solidity scope names.
    if let Some(h) = yul_builtin_hover(&root, offset) {
        return Some(h);
    }
    // 0. a named-argument key (`f({ owner_: … })`) → its parameter/field type.
    if let Some(h) = named_arg_hover(state, &uri, &root, offset) {
        return Some(h);
    }
    // 0b. an overloaded call resolved by argument types → the matching overload.
    if let Some((turi, def)) = typed_overload_target(state, &uri, &root, offset) {
        let troot = parse_root(state, &turi)?;
        return Some(markup_hover(
            solsp_ide::navigation::hover_text(&troot, &def),
            None,
        ));
    }
    // 1. same-file hover.
    if let Some(info) = solsp_ide::navigation::hover(&root, offset) {
        return Some(markup_hover(
            info.contents,
            Some(to_proto::range(li, info.range)),
        ));
    }
    // 2. member access `receiver.member` → hover from the member's declaration.
    if let Some((target_uri, def)) = member_resolve(state, &uri, &root, offset) {
        let troot = parse_root(state, &target_uri)?;
        return Some(markup_hover(
            solsp_ide::navigation::hover_text(&troot, &def),
            None,
        ));
    }
    // 2b. a bare name inherited from a cross-file base contract.
    if let Some((target_uri, def)) = inherited_name_at(state, &uri, &root, offset) {
        let troot = parse_root(state, &target_uri)?;
        return Some(markup_hover(
            solsp_ide::navigation::hover_text(&troot, &def),
            None,
        ));
    }
    // 2c. a builtin / synthetic member (`msg.sender`, `tx.gasprice`, `address(x).balance`,
    //     `arr.length`, `MyError.selector`, `type(X).max`) — show its type.
    if let Some(h) = builtin_member_hover(state, &uri, &root, offset) {
        return Some(h);
    }
    // 3. an imported top-level symbol (followed transitively through re-exports) → hover
    //    from the target file. The hovered identifier is in *this* file, so report no
    //    range and let the client highlight it.
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

/// `textDocument/completion` → member completion after a `.`, else scope completion
/// (names visible at the cursor). The client filters by the typed prefix.
fn completion(state: &ServerState, params: CompletionParams) -> CompletionResponse {
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
    // named-argument keys (`f({ <here>: … })`) are the most specific context.
    if let Some(items) = named_arg_completion(state, &uri, &root, offset) {
        return Some(items);
    }
    // member completion whenever the cursor sits after a `.`.
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

/// Completion for `receiver.<here>`: the members of the receiver's type (incl. cross-file
/// inherited members). `None` when the cursor is not after a `.`.
fn member_completion(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<Vec<CompletionItem>> {
    let receiver = dotted_receiver(root, offset)?;
    // library functions attached to the receiver's type via `using L for T`.
    let using_items = using_member_items(state, uri, root, &receiver);
    // `N.` where `N` is an `import * as N` namespace alias → the imported file's exports.
    if let Some(turi) = namespace_target_uri(uri, root, &receiver) {
        if let Some(tfile) = state.file(&turi) {
            let troot = solsp_base_db::parse(state.db(), tfile).syntax();
            let mut defs = solsp_hir::resolve::file_definitions(&troot);
            let mut visited = std::collections::HashSet::new();
            collect_file_exports(state, &turi, &troot, &mut visited, &mut defs);
            return Some(completion_items_from(defs));
        }
    }
    // a receiver with a source type → its members (incl. cross-file inherited).
    if let Some((turi, tdef)) = resolve_receiver_type(state, uri, root, &receiver) {
        let Some(troot) = parse_root(state, &turi) else {
            return Some(completion_items_from(solsp_hir::resolve::type_members(
                &tdef,
            )));
        };
        let contract_like = matches!(tdef.kind(), solsp_syntax::SyntaxKind::CONTRACT_DEF);
        let library = contract_like && is_library_node(&tdef);
        // a contract/interface *instance* (`x.`, `this.`) → only public/external members;
        // a library (`Lib.`) or `super.` → everything except `private`; a struct → fields.
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
        // members carry their declared type (`Definition::ty`) in the completion detail.
        let same_file = if is_super {
            Vec::new()
        } else {
            solsp_hir::resolve::type_members(&tdef)
                .into_iter()
                .filter(|d| keep(&d.full_ptr.to_node(&troot)))
                .collect()
        };
        let mut items = completion_items_from(same_file);
        // contracts inherit across files (libraries do not); add those members.
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
    // a builtin global (`block.`, `tx.`, `msg.`, `abi.`) has no source type.
    if let Some(items) = builtin_member_items(&receiver) {
        return Some(items);
    }
    // `type(X).` — contract/integer/enum type introspection.
    if let Some(items) = type_expr_members(state, uri, root, &receiver) {
        return Some(items);
    }
    // builtins on an `address` / array / `bytes` value (plus any `using` functions).
    if let Some(mut items) = value_type_builtin_members(state, uri, root, &receiver) {
        items.extend(using_items);
        return Some(items);
    }
    // `MyError.`/`MyEvent.`/`myFunc.` → the ABI `.selector`.
    if let Some(items) = selector_member(state, uri, root, &receiver) {
        return Some(items);
    }
    // an elementary value with only `using L for T` functions (e.g. `uint256.toString`).
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

/// Hover for a builtin / synthetic member (`msg.sender`, `address(x).balance`,
/// `arr.length`, `MyError.selector`, `type(X).max`, …): finds the hovered member among the
/// receiver's synthetic members and reports its type.
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

/// `.selector` on an error/function (`bytes4`) or event (`bytes32`) receiver, when the
/// receiver resolves to such a declaration.
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

/// Resolve a receiver expression to the declaration it names — a bare name (`MyError`,
/// `myFunc`) or a qualified one (`Lib.MyError`). For looking up what kind of thing a
/// receiver is, not its type.
fn resolve_receiver_def(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<solsp_hir::resolve::Definition> {
    resolve_receiver_def_target(state, uri, root, receiver).map(|(_, _, def)| def)
}

fn resolve_receiver_def_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<(
    Url,
    solsp_syntax::SyntaxNode,
    solsp_hir::resolve::Definition,
)> {
    use solsp_syntax::SyntaxKind::{MEMBER_EXPR, NAME_REF, PATH_EXPR};
    match receiver.kind() {
        PATH_EXPR | NAME_REF => {
            let nr = receiver_name_ref(receiver)?;
            if let Some(d) = solsp_hir::resolve::resolve(&nr) {
                return Some((uri.clone(), root.clone(), d));
            }
            let name = nameref_text(&nr)?;
            if let Some(c) = enclosing_contract(receiver) {
                if let Some((target_uri, d)) = inherited_member(state, uri, root, &c, &name, None) {
                    let target_root = parse_root(state, &target_uri)?;
                    return Some((target_uri, target_root, d));
                }
            }
            let (target_uri, d) = cross_file_definition(state, uri, root, &name, None)?;
            let target_root = parse_root(state, &target_uri)?;
            Some((target_uri, target_root, d))
        }
        // `A.B` → resolve the member `B` at its own offset.
        MEMBER_EXPR => {
            let member_nr = receiver
                .children()
                .filter(|n| n.kind() == NAME_REF)
                .last()?;
            let off = member_nr.text_range().start();
            let (target_uri, d) = member_resolve(state, uri, root, off)?;
            let target_root = parse_root(state, &target_uri)?;
            Some((target_uri, target_root, d))
        }
        _ => None,
    }
}

/// Members of `type(X)`: integer/enum `min`/`max`, contract bytecode metadata, or an
/// interface `interfaceId`. `None` if the receiver isn't `type(X)`.
fn type_expr_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<Vec<CompletionItem>> {
    use solsp_syntax::SyntaxKind::{CONTRACT_DEF, ENUM_DEF, PATH_TYPE, TYPE_EXPR};
    if receiver.kind() != TYPE_EXPR {
        return None;
    }
    let pt = receiver.children().find(|n| n.kind() == PATH_TYPE)?;
    let name = solsp_hir::resolve::path_type_segments(&pt).pop()?;
    if is_integer_type_name(&name) {
        let minmax = vec![("min", name.as_str(), false), ("max", name.as_str(), false)];
        return Some(synthetic_members(&minmax));
    }
    if let Some((_, tdef)) = resolve_path_type(state, uri, root, &pt) {
        return Some(match tdef.kind() {
            ENUM_DEF => synthetic_members(&[("min", "", false), ("max", "", false)]),
            CONTRACT_DEF if is_interface_node(&tdef) => {
                synthetic_members(&[("name", "string", false), ("interfaceId", "bytes4", false)])
            }
            CONTRACT_DEF => synthetic_members(&[
                ("name", "string", false),
                ("creationCode", "bytes", false),
                ("runtimeCode", "bytes", false),
            ]),
            _ => Vec::new(),
        });
    }
    Some(Vec::new())
}

/// Builtin members of an `address` / array / `bytes` value, by the receiver's declared
/// type. `None` for other types.
fn value_type_builtin_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<Vec<CompletionItem>> {
    let (ty, is_storage) = receiver_value_info(state, uri, root, receiver)?;
    let ty = ty.trim();
    if ty == "address" {
        return Some(synthetic_members(&[
            ("balance", "uint256", false),
            ("code", "bytes", false),
            ("codehash", "bytes32", false),
            ("call", "", true),
            ("delegatecall", "", true),
            ("staticcall", "", true),
        ]));
    }
    if ty == "address payable" {
        return Some(synthetic_members(&[
            ("balance", "uint256", false),
            ("code", "bytes", false),
            ("codehash", "bytes32", false),
            ("call", "", true),
            ("delegatecall", "", true),
            ("staticcall", "", true),
            ("transfer", "", true),
            ("send", "", true),
        ]));
    }
    // a dynamic array or `bytes` — `.length` always; `.push`/`.pop` only in storage.
    if ty.ends_with("[]") || ty == "bytes" {
        let mut m: Vec<(&str, &str, bool)> = vec![("length", "uint256", false)];
        if is_storage {
            m.push(("push", "", true));
            m.push(("pop", "", true));
        }
        return Some(synthetic_members(&m));
    }
    // a fixed-size array `T[N]` or `bytesN` — `.length` only.
    if ty.ends_with(']') || is_fixed_bytes(ty) {
        return Some(synthetic_members(&[("length", "uint256", false)]));
    }
    None
}

/// The `(type text, lives in storage)` of a receiver value: simple/cross-file variables,
/// member accesses, address casts (`address(x)`/`payable(x)`), and the builtin
/// address-returning members (`msg.sender`, `tx.origin`, `block.coinbase`).
fn receiver_value_info(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<(String, bool)> {
    use solsp_syntax::SyntaxKind::{CALL_EXPR, INDEX_EXPR, MEMBER_EXPR};
    if receiver.kind() == CALL_EXPR {
        let callee = receiver.first_child()?;
        match callee_display_name(&callee)?.as_str() {
            "address" => return Some(("address".to_string(), false)),
            "payable" => return Some(("address payable".to_string(), false)),
            name => {
                let parsed = typecheck::parse_ty(name);
                if !matches!(parsed, typecheck::Ty::User(_)) {
                    return Some((ty_label(&parsed), false));
                }
            }
        }
        // a function call → its return type (a memory/value result).
        let (duri, def) = resolve_named_callee(state, uri, root, &callee)?;
        let droot = parse_root(state, &duri)?;
        let ret = function_return_param(&def.full_ptr.to_node(&droot))?;
        return Some((type_text(&ret)?, false));
    }
    if receiver.kind() == INDEX_EXPR {
        // `base[i]` → the array element / mapping value type; storage follows the base.
        let base = receiver.first_child()?;
        // a declared array/mapping → its element/value type (a nested mapping value stays
        // a mapping, which is reportable when a struct is expected).
        if let Some(base_decl) = receiver_decl(state, uri, root, &base) {
            if let Some(t) = indexed_type_text(&base_decl) {
                return Some((t, is_storage_decl(&base_decl)));
            }
        }
        // a nested index / call base → strip one array level from its type text.
        let (base_ty, storage) = receiver_value_info(state, uri, root, &base)?;
        return Some((base_ty.strip_suffix("[]")?.trim().to_string(), storage));
    }
    if receiver.kind() == MEMBER_EXPR {
        // a builtin global member (`msg.sender`, `msg.data`, `tx.origin`, `block.coinbase`)
        // → its declared type, so chains like `msg.data.length` resolve.
        let recv = receiver.first_child()?;
        let member = member_name(receiver)?;
        if let Some(items) = builtin_member_items(&recv) {
            if let Some(d) = items
                .iter()
                .find(|i| i.label == member)
                .and_then(|i| i.detail.as_deref())
                .filter(|d| !d.is_empty())
            {
                // drop a data location so the type model sees `bytes`, not `bytes calldata`.
                let ty = d
                    .trim_end_matches(" calldata")
                    .trim_end_matches(" memory")
                    .trim_end_matches(" storage")
                    .to_string();
                return Some((ty, false));
            }
        }
        if let Some(items) = value_type_builtin_members(state, uri, root, &recv) {
            if let Some(d) = items
                .iter()
                .find(|i| i.label == member)
                .and_then(|i| i.detail.as_deref())
                .filter(|d| !d.is_empty())
            {
                let ty = d
                    .trim_end_matches(" calldata")
                    .trim_end_matches(" memory")
                    .trim_end_matches(" storage")
                    .to_string();
                return Some((ty, false));
            }
        }
    }
    let decl = receiver_decl(state, uri, root, receiver)?;
    Some((type_text(&decl)?, is_storage_decl(&decl)))
}

/// Whether a declaration's value lives in storage: a state variable, or a local with the
/// `storage` data location.
fn is_storage_decl(decl: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{STATE_VAR_DEF, STORAGE_KW};
    decl.kind() == STATE_VAR_DEF
        || decl
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == STORAGE_KW)
}

/// The declaration node a receiver value refers to: a simple/cross-file variable or a
/// member access.
fn receiver_decl(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{MEMBER_EXPR, NAME_REF, PATH_EXPR};
    match receiver.kind() {
        PATH_EXPR | NAME_REF => {
            let nr = receiver_name_ref(receiver)?;
            if let Some(d) = solsp_hir::resolve::resolve(&nr) {
                return Some(d.full_ptr.to_node(root));
            }
            // a cross-file inherited variable.
            let name = solsp_hir::resolve::receiver_name(receiver)?;
            let c = enclosing_contract(receiver)?;
            let (duri, d) = inherited_member(state, uri, root, &c, &name, None)?;
            let droot = parse_root(state, &duri)?;
            Some(d.full_ptr.to_node(&droot))
        }
        MEMBER_EXPR => {
            let recv = receiver.first_child()?;
            let member = member_name(receiver)?;
            let (turi, tdef) = receiver_type(state, uri, root, &recv, false)?;
            let troot = parse_root(state, &turi)?;
            let (muri, mdef) = if is_super_receiver(&recv) {
                inherited_base_member(state, &turi, &troot, &tdef, &member, None)?
            } else {
                (
                    turi.clone(),
                    member_lookup(state, &turi, &tdef, &member, None)?,
                )
            };
            let mroot = parse_root(state, &muri)?;
            Some(mdef.full_ptr.to_node(&mroot))
        }
        _ => None,
    }
}

/// Whether a `CONTRACT_DEF` node is a `library`.
fn is_library_node(c: &solsp_syntax::SyntaxNode) -> bool {
    c.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == solsp_syntax::SyntaxKind::LIBRARY_KW)
}

/// Whether a `CONTRACT_DEF` node is an `interface`.
fn is_interface_node(c: &solsp_syntax::SyntaxNode) -> bool {
    c.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == solsp_syntax::SyntaxKind::INTERFACE_KW)
}

fn is_super_receiver(receiver: &solsp_syntax::SyntaxNode) -> bool {
    solsp_hir::resolve::receiver_name(receiver).as_deref() == Some("super")
}

/// Whether a receiver is a *value* (a contract instance) rather than a bare type name —
/// i.e. `instance.member` (external access) vs `Type.member` (static).
fn is_instance_receiver(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> bool {
    use solsp_syntax::SyntaxKind::{NAME_REF, PATH_EXPR};
    if !matches!(receiver.kind(), PATH_EXPR | NAME_REF) {
        return true; // a member/call/index expression is always a value
    }
    let Some(name) = solsp_hir::resolve::receiver_name(receiver) else {
        return true;
    };
    let resolved = receiver_name_ref(receiver)
        .and_then(|nr| solsp_hir::resolve::resolve(&nr))
        .or_else(|| cross_file_definition(state, uri, root, &name, None).map(|(_, d)| d));
    // a bare name that resolves to a type (library/contract/struct/enum) is a static
    // receiver; anything else (a variable, or unresolved) is treated as an instance.
    !resolved.map(|d| is_type_kind(d.kind)).unwrap_or(false)
}

/// Completion for a bare identifier: every name visible at the cursor — locals, params,
/// the enclosing contract's members (incl. cross-file inherited), and file top-level.
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
        // internal access (the contract's own scope) — keep all but private bases.
        defs.extend(collect_inherited_members(
            state, uri, root, &contract, false,
        ));
    }
    defs.extend(imported_symbols(state, uri, root));
    let mut items = completion_items_from(defs);
    items.extend(namespace_alias_items(root)); // `import * as N` → N
    items.extend(builtin_items()); // keywords, elementary types, globals
                                   // dedup by label, keeping the first (user/imported names shadow builtins).
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(i.label.clone()));
    items
}

/// `textDocument/signatureHelp` → the signature of the call the cursor is inside (a
/// positional `f(…)` call), with the active parameter highlighted.
fn signature_help(state: &ServerState, params: SignatureHelpParams) -> Option<SignatureHelp> {
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

    // the candidate declarations: same-file overloads of a function, else the single
    // struct/constructor.
    let candidates = signature_candidates(&def, &def_node, &name, &droot);

    // active parameter = number of top-level commas before the cursor.
    let active = arg_list
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == COMMA && t.text_range().start() < offset)
        .count() as u32;
    let signatures: Vec<SignatureInformation> = candidates
        .iter()
        .map(|(kind, node)| signature_info(&name, *kind, node, active))
        .collect();
    // pick the overload whose parameter count matches the arguments typed so far.
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

/// The declarations to show as signatures: every same-file overload of a function (sorted
/// by parameter count), or the single struct / constructor.
fn signature_candidates(
    def: &solsp_hir::resolve::Definition,
    def_node: &solsp_syntax::SyntaxNode,
    name: &str,
    droot: &solsp_syntax::SyntaxNode,
) -> Vec<(solsp_hir::resolve::DefKind, solsp_syntax::SyntaxNode)> {
    use solsp_hir::resolve::DefKind::{Function, Modifier};
    if !matches!(def.kind, Function | Modifier) {
        return vec![(def.kind, def_node.clone())];
    }
    // same-named functions in the enclosing contract, or at file top level.
    let pool = match enclosing_contract(def_node) {
        Some(c) => solsp_hir::resolve::type_members(&c),
        None => solsp_hir::resolve::file_definitions(droot),
    };
    let mut nodes: Vec<solsp_syntax::SyntaxNode> = pool
        .into_iter()
        .filter(|d| d.kind == Function && d.name == name)
        .map(|d| d.full_ptr.to_node(droot))
        .collect();
    if nodes.is_empty() {
        nodes.push(def_node.clone());
    }
    nodes.sort_by_key(|n| named_arg_fields(Function, n).len());
    nodes.into_iter().map(|n| (def.kind, n)).collect()
}

/// Build a `SignatureInformation` from a declaration's `(name, type)` parameters.
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

/// The display name of a call's callee: `f` / `S` / `obj.method` / `new T`.
fn callee_display_name(callee: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::{MEMBER_EXPR, NAME_REF, NEW_EXPR, PATH_EXPR};
    match callee.kind() {
        PATH_EXPR | NAME_REF => solsp_hir::resolve::receiver_name(callee),
        MEMBER_EXPR => member_name(callee),
        NEW_EXPR => callee
            .descendants()
            .filter(|n| n.kind() == NAME_REF)
            .last()
            .and_then(|nr| node_ident(&nr)),
        _ => None,
    }
}

/// Resolve a named-call callee to its declaration: `new T(...)` → the type `T`, else a
/// function/struct/contract name or `obj.method`.
fn resolve_named_callee(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    callee: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_syntax::SyntaxKind::{NAME_REF, NEW_EXPR};
    if callee.kind() == NEW_EXPR {
        let nr = callee.descendants().find(|n| n.kind() == NAME_REF)?;
        let name = solsp_hir::resolve::receiver_name(&nr)?;
        return solsp_hir::resolve::resolve(&nr)
            .map(|d| (uri.clone(), d))
            .or_else(|| cross_file_definition(state, uri, root, &name, None));
    }
    resolve_callee(state, uri, root, callee, None)
}

/// The receiver expression of a `receiver.member` access at `offset`, when the cursor is
/// on the member side (after the `.`). `None` otherwise.
fn dotted_receiver(
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{DOT, MEMBER_EXPR};
    let tok = root.token_at_offset(offset).left_biased()?;
    let member_expr = tok
        .parent()?
        .ancestors()
        .find(|n| n.kind() == MEMBER_EXPR)?;
    let dot = member_expr
        .children_with_tokens()
        .find(|e| e.kind() == DOT)?;
    if offset < dot.text_range().end() {
        return None; // cursor is in the receiver, not after the dot
    }
    member_expr.first_child()
}

/// All members inherited by `contract` from its base contracts across files (BFS,
/// diamond-safe). Each contract contributes its own direct members.
fn collect_inherited_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    // external access (`instance.member`) → only `public`/`external` members.
    external_only: bool,
) -> Vec<solsp_hir::resolve::Definition> {
    collect_inherited_members_impl(state, uri, root, contract, external_only, true)
}

/// All members reachable through `super`: direct and transitive base contracts only.
fn collect_base_members(
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
    // (uri, root, contract, is_base) — a base's `private` members are not inherited.
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

/// Flag identifiers used as values that resolve to no declaration anywhere — typo'd or
/// missing variables/functions. Conservative: a name reachable through any path (scope,
/// inheritance, imports, builtins, same-file top level) is left alone, so a resolution gap
/// never becomes a false "undefined".
fn undefined_name_diagnostics(
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
        // only a bare identifier used as a value (`x`, `foo`, `Lib`), not a member or type.
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
    // Yul assignment targets (`x := …`): the left side names a variable (a Yul `let` or a
    // Solidity variable in scope), never a builtin, so an unresolved one is undefined.
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
        // each target path before `:=`; its first segment is the variable.
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

/// Type-check assignments: a simple assignment `lhs = rhs` and a local declaration with an
/// initializer `T x = rhs`, flagging an `rhs` not implicitly convertible to the target
/// type. Conservative — an un-inferrable side is `Unknown` and never flagged.
fn assignment_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{ASSIGN_EXPR, EQ, VAR_DECL, VAR_DECL_STMT};
    let mut out = Vec::new();
    for node in root.descendants() {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let (target, value) = match node.kind() {
            // `lhs = rhs` (simple assignment only — compound `+=` needs numeric operands).
            ASSIGN_EXPR => {
                let op_is_eq = node
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .any(|t| t.kind() == EQ);
                let exprs: Vec<_> = node.children().collect();
                if !op_is_eq || exprs.len() != 2 {
                    continue;
                }
                let lty = infer_arg_ty(state, uri, root, &exprs[0]);
                (lty, exprs[1].clone())
            }
            // `T x = rhs` — a single (non-tuple) local declaration with an initializer.
            VAR_DECL_STMT => {
                // a tuple destructuring `(a, , uint x) = f()` is parenthesized and binds a
                // function's tuple return; only one slot may be named, so skip by the paren.
                let is_tuple = node
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .any(|t| t.kind() == solsp_syntax::SyntaxKind::L_PAREN);
                let decls: Vec<_> = node.children().filter(|c| c.kind() == VAR_DECL).collect();
                let Some(init) = node.children().find(|c| c.kind() != VAR_DECL) else {
                    continue; // no initializer
                };
                if is_tuple || decls.len() != 1 {
                    continue; // tuple destructuring — skip
                }
                let Some(ty) = type_text(&decls[0]) else {
                    continue;
                };
                (typecheck::parse_ty(&ty), init)
            }
            _ => continue,
        };
        // a number literal that overflows the target integer type (`uint8 x = 300`).
        if let Some(msg) = literal_range_error(&value, &target) {
            out.push(type_mismatch(li, &value, &msg));
            continue;
        }
        let value_ty = infer_arg_ty(state, uri, root, &value);
        if matches!(target, typecheck::Ty::Unknown) || matches!(value_ty, typecheck::Ty::Unknown) {
            continue;
        }
        if !types_compatible(state, uri, root, &value_ty, &target) {
            out.push(type_mismatch(
                li,
                &value,
                &format!(
                    "value of type `{}` is not implicitly convertible to `{}`",
                    arg_text(&value),
                    ty_label(&target),
                ),
            ));
        }
    }
    out
}

/// An error message if `value` is a plain integer literal that does not fit the integer
/// `target` type. `None` for non-integer targets, non-literal values, or values we can't
/// evaluate (scientific notation, units, hex beyond `u128`).
fn literal_range_error(value: &solsp_syntax::SyntaxNode, target: &typecheck::Ty) -> Option<String> {
    let (signed, bits) = match target {
        typecheck::Ty::Uint(b) => (false, *b),
        typecheck::Ty::Int(b) => (true, *b),
        _ => return None,
    };
    if bits >= 128 {
        return None; // a `u128` literal value can't overflow uint128+/int128+
    }
    let v = literal_u128(value)?;
    let max = if signed {
        (1u128 << (bits - 1)) - 1
    } else {
        (1u128 << bits) - 1
    };
    (v > max).then(|| format!("literal `{v}` does not fit in `{}`", ty_label(target)))
}

/// A plain (decimal or hex) integer literal's value, or `None` for non-number literals,
/// scientific notation / fractions / units, or values exceeding `u128`.
fn literal_u128(value: &solsp_syntax::SyntaxNode) -> Option<u128> {
    use solsp_syntax::SyntaxKind::{LITERAL_EXPR, NUMBER};
    if value.kind() != LITERAL_EXPR {
        return None;
    }
    let tok = value
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == NUMBER)?;
    let text = tok.text().replace('_', "");
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16).ok()
    } else if text.contains(['.', 'e', 'E']) {
        None // scientific / fractional — skip
    } else {
        text.parse::<u128>().ok()
    }
}

/// Type-check `return expr;` against the enclosing function's single declared return type,
/// flagging an `expr` not implicitly convertible to it. Tuple returns and un-inferrable
/// expressions are left alone.
fn return_type_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{
        COMMA, FUNCTION_DEF, PARAM, PARAM_LIST, RETURN_STMT, TUPLE_EXPR,
    };
    let mut out = Vec::new();
    for ret in root.descendants().filter(|n| n.kind() == RETURN_STMT) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let Some(value) = ret.children().next() else {
            continue; // `return;` with no value
        };
        let Some(func) = ret.ancestors().find(|n| n.kind() == FUNCTION_DEF) else {
            continue;
        };
        // the second parameter list is `returns (...)`.
        let Some(returns) = func.children().filter(|n| n.kind() == PARAM_LIST).nth(1) else {
            continue;
        };
        let ret_params: Vec<_> = returns.children().filter(|n| n.kind() == PARAM).collect();
        // an explicit tuple `return (a, b)` must have as many elements as declared returns.
        if value.kind() == TUPLE_EXPR {
            let elems = value
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == COMMA)
                .count()
                + 1;
            if elems != ret_params.len() {
                out.push(type_mismatch(
                    li,
                    &value,
                    &format!(
                        "returns {} value(s), but the function declares {}",
                        elems,
                        ret_params.len(),
                    ),
                ));
            }
            continue; // tuple element types are not checked individually here
        }
        if ret_params.len() != 1 {
            // a single value can fill multiple returns only if it is a (tuple-returning)
            // call; anything else returns one value where several are declared.
            if value.kind() != solsp_syntax::SyntaxKind::CALL_EXPR {
                out.push(type_mismatch(
                    li,
                    &value,
                    &format!(
                        "returns 1 value, but the function declares {}",
                        ret_params.len(),
                    ),
                ));
            }
            continue;
        }
        let Some(ty) = type_text(&ret_params[0]) else {
            continue;
        };
        let target = typecheck::parse_ty(&ty);
        if let Some(msg) = literal_range_error(&value, &target) {
            out.push(type_mismatch(li, &value, &msg));
            continue;
        }
        let value_ty = infer_arg_ty(state, uri, root, &value);
        if matches!(target, typecheck::Ty::Unknown) || matches!(value_ty, typecheck::Ty::Unknown) {
            continue;
        }
        if !types_compatible(state, uri, root, &value_ty, &target) {
            out.push(type_mismatch(
                li,
                &value,
                &format!(
                    "returned value of type `{}` is not implicitly convertible to `{}`",
                    arg_text(&value),
                    ty_label(&target),
                ),
            ));
        }
    }
    out
}

/// When the cursor is on the callee of an overloaded call, pick the overload by argument
/// types (not just count) — returns the matching overload only when exactly one accepts the
/// arguments, so ambiguous/un-inferrable cases fall back to the default arity resolution.
fn typed_overload_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_hir::resolve::DefKind;
    use solsp_syntax::SyntaxKind::{ARG_LIST, CALL_EXPR, NAME_REF};
    let nr = root
        .token_at_offset(offset)
        .find_map(|t| t.parent_ancestors().find(|n| n.kind() == NAME_REF))?;
    let call = nr.ancestors().find(|n| n.kind() == CALL_EXPR)?;
    let callee = call.first_child()?;
    // the cursor must be on the callee, not inside an argument.
    if !callee.text_range().contains(offset) {
        return None;
    }
    let (def_uri, def) = resolve_named_callee(state, uri, root, &callee)?;
    if def.kind != DefKind::Function {
        return None;
    }
    let droot = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&droot);
    let name = callee_display_name(&callee)?;
    let candidates = signature_candidates(&def, &def_node, &name, &droot);
    if candidates.len() < 2 {
        return None; // not overloaded — nothing to disambiguate
    }
    use solsp_syntax::SyntaxKind::NAMED_ARG_LIST;
    // arguments, positional (`key = None`) or named (`key = Some`).
    let args: Vec<(Option<String>, solsp_syntax::SyntaxNode)> =
        if let Some(al) = call.children().find(|n| n.kind() == ARG_LIST) {
            al.children().map(|v| (None, v)).collect()
        } else if let Some(nal) = call.children().find(|n| n.kind() == NAMED_ARG_LIST) {
            named_arg_pairs(&nal)
        } else {
            return None;
        };
    let arg_tys: Vec<typecheck::Ty> = args
        .iter()
        .map(|(_, v)| infer_arg_ty(state, uri, root, v))
        .collect();
    let is_base = |a: &str, b: &str| is_subtype(state, uri, root, a, b);
    let accepts = |node: &solsp_syntax::SyntaxNode| {
        let params = named_arg_fields(DefKind::Function, node);
        if params.len() != args.len() {
            return false;
        }
        (0..args.len()).all(|i| {
            // a named arg matches its parameter by key; a positional one by position.
            let ptype = match &args[i].0 {
                Some(key) => params.iter().find(|(pn, _)| pn == key).map(|(_, t)| t),
                None => params.get(i).map(|(_, t)| t),
            };
            ptype.is_some_and(|p| {
                typecheck::implicitly_convertible(&arg_tys[i], &typecheck::parse_ty(p), &is_base)
            })
        })
    };
    let mut matches = candidates.iter().filter(|(_, node)| accepts(node));
    let (_, node) = matches.next()?;
    if matches.next().is_some() {
        return None; // ambiguous (e.g. un-inferrable args accept several) — fall back
    }
    let def = solsp_hir::resolve::definition(node)?;
    Some((def_uri, def))
}

/// Flag invalid explicit casts to address: `address(X)` / `payable(X)` where `X` names a
/// non-value (a function, library, type, …) rather than a castable value — e.g.
/// `address(roles)` where `roles` is a function, not an instance.
fn cast_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, CALL_EXPR, NAME_REF, PATH_EXPR};
    let mut out = Vec::new();
    for call in root.descendants().filter(|n| n.kind() == CALL_EXPR) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let Some(callee) = call.first_child() else {
            continue;
        };
        if !matches!(
            callee_display_name(&callee).as_deref(),
            Some("address" | "payable")
        ) {
            continue;
        }
        let Some(arg_list) = call.children().find(|n| n.kind() == ARG_LIST) else {
            continue;
        };
        let args: Vec<_> = arg_list.children().collect();
        // only a single bare-name operand (`address(roles)`, not `address(x.y)`/`address(0)`).
        let [arg] = args.as_slice() else { continue };
        if !matches!(arg.kind(), PATH_EXPR | NAME_REF) {
            continue;
        }
        let Some(def) = resolve_receiver_def(state, uri, root, arg) else {
            continue; // unresolved or a builtin (`this`) — leave alone
        };
        if !is_value_kind(def.kind) {
            out.push(type_mismatch(
                li,
                arg,
                &format!(
                    "cannot convert {} `{}` to an address",
                    def_kind_noun(def.kind),
                    arg_text(arg),
                ),
            ));
        }
    }
    out
}

/// Whether a definition names a value (so it can be cast/used as one), as opposed to a
/// type, function, or other non-value declaration.
fn is_value_kind(kind: solsp_hir::resolve::DefKind) -> bool {
    use solsp_hir::resolve::DefKind::{Field, Local, Parameter, StateVariable, Variant};
    matches!(kind, StateVariable | Parameter | Local | Field | Variant)
}

/// A short noun for a definition kind, for diagnostic messages.
fn def_kind_noun(kind: solsp_hir::resolve::DefKind) -> &'static str {
    use solsp_hir::resolve::DefKind::*;
    match kind {
        Function => "function",
        Modifier => "modifier",
        Event => "event",
        Error => "error",
        Contract => "contract",
        Interface => "interface",
        Library => "library",
        Struct => "struct",
        Enum => "enum",
        UserType => "type",
        _ => "value",
    }
}

/// Flag arithmetic / bitwise / shift operators applied to a non-numeric operand — e.g.
/// Utils.sol `… >> address(1)`. Comparisons (`==`, `<`) and logical operators are not
/// checked (addresses are comparable).
fn binary_op_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{
        AMP, BIN_EXPR, CARET, MINUS, PERCENT, PIPE, PLUS, SHL, SHR, SLASH, STAR, STAR2,
    };
    let mut out = Vec::new();
    for bin in root.descendants().filter(|n| n.kind() == BIN_EXPR) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let is_arith = bin
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| {
                matches!(
                    t.kind(),
                    PLUS | MINUS | STAR | SLASH | PERCENT | STAR2 | AMP | PIPE | CARET | SHL | SHR
                )
            });
        if !is_arith {
            continue; // a comparison / logical operator — not type-restricted here
        }
        for operand in bin.children() {
            let ty = infer_arg_ty(state, uri, root, &operand);
            if is_non_arithmetic_type(&ty) {
                out.push(type_mismatch(
                    li,
                    &operand,
                    &format!(
                        "`{}` of type `{}` cannot be used in an arithmetic or bitwise expression",
                        arg_text(&operand),
                        ty_label(&ty),
                    ),
                ));
            }
        }
    }
    out
}

/// Flag a single local declaration whose name is never referenced again in its function
/// (`uint256 x;` with no later use). Conservative: any identifier of that name anywhere in
/// the function — a read, a write, or a Yul use — keeps it live.
fn unused_local_diagnostics(
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{FUNCTION_DEF, IDENT, L_PAREN, NAME, VAR_DECL, VAR_DECL_STMT};
    let mut out = Vec::new();
    for stmt in root.descendants().filter(|n| n.kind() == VAR_DECL_STMT) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        // single, non-tuple declaration only.
        let is_tuple = stmt
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == L_PAREN);
        let decls: Vec<_> = stmt.children().filter(|c| c.kind() == VAR_DECL).collect();
        if is_tuple || decls.len() != 1 {
            continue;
        }
        let Some(name_node) = decls[0].children().find(|c| c.kind() == NAME) else {
            continue;
        };
        let Some(name) = nameref_text(&name_node) else {
            continue;
        };
        let Some(func) = stmt.ancestors().find(|n| n.kind() == FUNCTION_DEF) else {
            continue;
        };
        // the name's only occurrence in the function is this declaration → unused.
        let uses = func
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == IDENT && t.text() == name)
            .count();
        if uses == 1 {
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, name_node.text_range()),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                source: Some("solsp".to_string()),
                message: format!("unused local variable `{name}`"),
                tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            });
        }
    }
    out
}

/// Flag named import entries whose target file does not export the requested symbol.
fn invalid_import_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_hir::imports::ImportKind;
    use solsp_syntax::SyntaxKind::IMPORT_DIRECTIVE;

    let directives: Vec<_> = root
        .descendants()
        .filter(|node| node.kind() == IMPORT_DIRECTIVE)
        .collect();
    let mut out = Vec::new();
    for (dir, imp) in directives.iter().zip(solsp_hir::imports::imports(root)) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let ImportKind::Named(names) = &imp.kind else {
            continue;
        };
        for name in names {
            if import_export_exists(state, uri, &imp.path, &name.name) {
                continue;
            }
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, import_name_range(dir, &name.name)),
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                source: Some("solsp".to_string()),
                message: format!(
                    "Import target `{}` does not export `{}`",
                    imp.path, name.name
                ),
                ..Default::default()
            });
        }
    }
    out
}

fn import_export_exists(state: &ServerState, uri: &Url, path: &str, name: &str) -> bool {
    let Some(turi) = state::resolve_import_uri(uri, path) else {
        return true;
    };
    let Some(troot) = parse_root(state, &turi) else {
        return true;
    };
    solsp_hir::resolve::top_level_definition(&troot, name, None).is_some()
        || cross_file_definition(state, &turi, &troot, name, None).is_some()
}

fn import_name_range(import_directive: &solsp_syntax::SyntaxNode, name: &str) -> rowan::TextRange {
    use solsp_syntax::SyntaxKind::{AS_KW, IDENT, L_BRACE, R_BRACE};
    let tokens: Vec<_> = import_directive
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| !token.kind().is_trivia())
        .collect();
    let start = tokens
        .iter()
        .position(|token| token.kind() == L_BRACE)
        .map(|index| index + 1)
        .unwrap_or(0);
    let end = tokens
        .iter()
        .position(|token| token.kind() == R_BRACE)
        .unwrap_or(tokens.len());
    let mut index = start;
    while index < end {
        if tokens[index].kind() == IDENT && tokens[index].text() == name {
            return tokens[index].text_range();
        }
        if tokens.get(index + 1).map(|token| token.kind()) == Some(AS_KW) {
            index += 3;
        } else {
            index += 1;
        }
    }
    import_directive.text_range()
}

/// Flag imported names that are never referenced in the file (`import { A } from "x"` where
/// `A` appears nowhere else).
fn unused_import_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_hir::imports::ImportKind;
    use solsp_syntax::SyntaxKind::{IDENT, IMPORT_DIRECTIVE};
    let directives: Vec<_> = root
        .descendants()
        .filter(|n| n.kind() == IMPORT_DIRECTIVE)
        .collect();
    if directives.is_empty() {
        return Vec::new();
    }
    let used = identifiers_outside_imports(root);

    let mut out = Vec::new();
    for (dir, imp) in directives.iter().zip(solsp_hir::imports::imports(root)) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        if has_unused_import_suppression(root, dir) {
            continue;
        }
        let locals: Vec<String> = match imp.kind {
            ImportKind::Named(names) => names
                .iter()
                .filter(|n| import_export_exists(state, uri, &imp.path, &n.name))
                .map(|n| n.local().to_string())
                .collect(),
            ImportKind::Namespace(n) => vec![n],
            ImportKind::Glob => continue, // binds everything — can't tell what's used
        };
        for local in locals.iter().filter(|l| !used.contains(*l)) {
            // flag the identifier inside the directive (the alias if there is one).
            let span = dir
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == IDENT && t.text() == local)
                .last()
                .map(|t| t.text_range())
                .unwrap_or_else(|| dir.text_range());
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, span),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                source: Some("solsp".to_string()),
                message: format!("`{local}` is imported but never used"),
                tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            });
        }
    }
    out
}

fn has_unused_import_suppression(
    root: &solsp_syntax::SyntaxNode,
    import_directive: &solsp_syntax::SyntaxNode,
) -> bool {
    const MARKER: &str = "forge-lint: disable-next-line(unused-import)";
    if import_directive.text().to_string().contains(MARKER) {
        return true;
    }
    let text = root.text().to_string();
    let start: usize = import_directive.text_range().start().into();
    let before = &text[..start];
    let previous_line = before
        .trim_end_matches([' ', '\t', '\r', '\n'])
        .rsplit_once('\n')
        .map_or(before.trim_end(), |(_, line)| line.trim());
    previous_line.contains(MARKER)
}

fn identifiers_outside_imports(
    root: &solsp_syntax::SyntaxNode,
) -> std::collections::HashSet<String> {
    use solsp_syntax::SyntaxKind::{IDENT, IMPORT_DIRECTIVE};
    let directives: Vec<_> = root
        .descendants()
        .filter(|n| n.kind() == IMPORT_DIRECTIVE)
        .collect();
    root.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == IDENT)
        .filter(|t| {
            !directives
                .iter()
                .any(|d| d.text_range().contains_range(t.text_range()))
        })
        .map(|t| t.text().to_string())
        .collect()
}

/// Flag direct state access that conflicts with function mutability, and suggest stricter
/// mutability for functions that only read state or do not touch state.
fn mutability_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_hir::resolve::DefKind;
    use solsp_syntax::SyntaxKind::{
        BLOCK, FUNCTION_DEF, MODIFIER_INVOCATION, NAME_REF, PAYABLE_KW, PURE_KW, VIEW_KW,
    };
    let mut out = Vec::new();
    for func in root.descendants().filter(|n| n.kind() == FUNCTION_DEF) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let is_view = func
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == VIEW_KW);
        let is_pure = func
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == PURE_KW);
        let yul_effects = yul_mutability_effects(&func);

        let mut reads_state = false;
        let mut writes_state = false;
        for nr in func.descendants().filter(|n| n.kind() == NAME_REF) {
            let Some((_, def_root, def)) = resolve_receiver_def_target(state, uri, root, &nr)
            else {
                continue;
            };
            if def.kind != DefKind::StateVariable {
                continue;
            }
            if state_variable_is_constant_or_immutable(&def.full_ptr.to_node(&def_root)) {
                continue;
            }
            if state_write_lhs(&nr).is_some() {
                writes_state = true;
            } else {
                reads_state = true;
                if is_pure {
                    out.push(type_mismatch(
                        li,
                        &nr,
                        "cannot read state in a `pure` function",
                    ));
                }
            }
        }

        if (is_view || is_pure) && writes_state {
            for lhs in func.descendants().filter_map(|n| {
                (n.kind() == NAME_REF
                    && resolve_receiver_def(state, uri, root, &n)
                        .is_some_and(|def| def.kind == DefKind::StateVariable))
                .then(|| state_write_lhs(&n))
                .flatten()
            }) {
                out.push(type_mismatch(
                    li,
                    &lhs,
                    "cannot write to state in a `view`/`pure` function",
                ));
            }
        }
        if is_pure {
            for read in &yul_effects.state_reads {
                out.push(type_mismatch(
                    li,
                    read,
                    "cannot read state in a `pure` function",
                ));
            }
        }
        if is_view || is_pure {
            for write in &yul_effects.state_writes {
                out.push(type_mismatch(
                    li,
                    write,
                    "cannot write to state in a `view`/`pure` function",
                ));
            }
        }

        if is_view
            || is_pure
            || writes_state
            || !yul_effects.state_writes.is_empty()
            || yul_effects.has_unknown_call
            || has_unknown_mutability_calls(state, uri, root, &func)
            || func.children().all(|n| n.kind() != BLOCK)
            || func.children().any(|n| n.kind() == MODIFIER_INVOCATION)
            || func
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .any(|t| t.kind() == PAYABLE_KW)
        {
            continue;
        }

        if reads_state || !yul_effects.state_reads.is_empty() {
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, function_name_range(&func)),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                source: Some("solsp".to_string()),
                message: "function can be marked `view`".to_string(),
                ..Default::default()
            });
        } else {
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, function_name_range(&func)),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                source: Some("solsp".to_string()),
                message: "function can be marked `pure`".to_string(),
                ..Default::default()
            });
        }
    }
    out
}

struct YulMutabilityEffects {
    state_reads: Vec<solsp_syntax::SyntaxNode>,
    state_writes: Vec<solsp_syntax::SyntaxNode>,
    has_unknown_call: bool,
}

fn yul_mutability_effects(function: &solsp_syntax::SyntaxNode) -> YulMutabilityEffects {
    use solsp_syntax::SyntaxKind::{NAME_REF, YUL_FUNCTION_CALL};

    let mut effects = YulMutabilityEffects {
        state_reads: Vec::new(),
        state_writes: Vec::new(),
        has_unknown_call: false,
    };
    for call in function
        .descendants()
        .filter(|node| node.kind() == YUL_FUNCTION_CALL)
    {
        let Some(callee) = call.descendants().find(|node| node.kind() == NAME_REF) else {
            effects.has_unknown_call = true;
            continue;
        };
        let text = callee.text().to_string();
        let name = text.trim();
        if yul_builtin_writes_state(name) {
            effects.state_writes.push(callee);
        } else if yul_builtin_reads_state(name) {
            effects.state_reads.push(callee);
        } else if !yul_builtin_is_pure(name) {
            effects.has_unknown_call = true;
        }
    }
    effects
}

fn yul_builtin_writes_state(name: &str) -> bool {
    matches!(
        name,
        "sstore"
            | "tstore"
            | "log0"
            | "log1"
            | "log2"
            | "log3"
            | "log4"
            | "create"
            | "create2"
            | "call"
            | "callcode"
            | "delegatecall"
            | "selfdestruct"
    )
}

fn yul_builtin_reads_state(name: &str) -> bool {
    matches!(
        name,
        "sload"
            | "tload"
            | "balance"
            | "selfbalance"
            | "extcodesize"
            | "extcodecopy"
            | "extcodehash"
            | "origin"
            | "caller"
            | "callvalue"
            | "gasprice"
            | "coinbase"
            | "timestamp"
            | "number"
            | "difficulty"
            | "prevrandao"
            | "gaslimit"
            | "chainid"
            | "basefee"
            | "blobbasefee"
            | "blobhash"
            | "gas"
    )
}

fn yul_builtin_is_pure(name: &str) -> bool {
    matches!(
        name,
        "stop"
            | "add"
            | "sub"
            | "mul"
            | "div"
            | "sdiv"
            | "mod"
            | "smod"
            | "exp"
            | "not"
            | "lt"
            | "gt"
            | "slt"
            | "sgt"
            | "eq"
            | "iszero"
            | "and"
            | "or"
            | "xor"
            | "byte"
            | "shl"
            | "shr"
            | "sar"
            | "addmod"
            | "mulmod"
            | "signextend"
            | "keccak256"
            | "pc"
            | "pop"
            | "mload"
            | "mstore"
            | "mstore8"
            | "msize"
            | "mcopy"
            | "calldataload"
            | "calldatasize"
            | "calldatacopy"
            | "codesize"
            | "codecopy"
            | "returndatasize"
            | "returndatacopy"
            | "return"
            | "revert"
    )
}

fn state_variable_is_constant_or_immutable(var: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{CONSTANT_KW, IMMUTABLE_KW};
    var.children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| matches!(token.kind(), CONSTANT_KW | IMMUTABLE_KW))
}

fn has_unknown_mutability_calls(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    function: &solsp_syntax::SyntaxNode,
) -> bool {
    use solsp_hir::resolve::DefKind;
    use solsp_syntax::SyntaxKind::CALL_EXPR;

    for call in function
        .descendants()
        .filter(|node| node.kind() == CALL_EXPR)
    {
        let Some(callee) = call.first_child() else {
            return true;
        };
        let Some((target_uri, def)) = resolve_callee(state, uri, root, &callee, arg_count(&call))
        else {
            return true;
        };
        match def.kind {
            DefKind::Function => {
                let Some(target_root) = parse_root(state, &target_uri) else {
                    return true;
                };
                let target_node = def.full_ptr.to_node(&target_root);
                if !function_has_view_or_pure(&target_node) {
                    return true;
                }
            }
            DefKind::Error => {}
            _ => return true,
        }
    }
    false
}

fn function_has_view_or_pure(function: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{PURE_KW, VIEW_KW};
    function
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| matches!(token.kind(), VIEW_KW | PURE_KW))
}

fn state_write_lhs(name_ref: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{ASSIGN_EXPR, NAME_REF};
    let asn = name_ref.ancestors().find(|n| n.kind() == ASSIGN_EXPR)?;
    let lhs = asn.first_child()?;
    if !lhs.text_range().contains_range(name_ref.text_range()) {
        return None;
    }
    let base = if lhs.kind() == NAME_REF {
        lhs.clone()
    } else {
        lhs.descendants().find(|n| n.kind() == NAME_REF)?
    };
    (base.text_range() == name_ref.text_range()).then_some(lhs)
}

/// Flag statements that follow a `return` / `revert` / `break` / `continue` in the same
/// block — they can never run.
fn unreachable_diagnostics(
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{BLOCK, BREAK_STMT, CONTINUE_STMT, RETURN_STMT, REVERT_STMT};
    let mut out = Vec::new();
    for block in root.descendants().filter(|n| n.kind() == BLOCK) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let stmts: Vec<_> = block.children().collect();
        let Some(term) = stmts.iter().position(|s| {
            matches!(
                s.kind(),
                RETURN_STMT | REVERT_STMT | BREAK_STMT | CONTINUE_STMT
            )
        }) else {
            continue;
        };
        if let Some(dead) = stmts.get(term + 1) {
            // unreachable code is a warning, not an error.
            out.push(lsp_types::Diagnostic {
                range: to_proto::range(li, dead.text_range()),
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                source: Some("solsp".to_string()),
                message: "unreachable code".to_string(),
                tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            });
        }
    }
    out
}

/// Flag a comparison (`< > <= >= == !=`) between incompatible operand types — e.g.
/// `address < uint`. Only fires when both operands are concrete, non-literal, and neither
/// is convertible to the other (literals and un-inferrable operands are left alone).
fn comparison_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{BIN_EXPR, EQ2, GT, GT_EQ, LT, LT_EQ, NEQ};
    let mut out = Vec::new();
    for bin in root.descendants().filter(|n| n.kind() == BIN_EXPR) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let ordered = bin
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find_map(|t| match t.kind() {
                LT | GT | LT_EQ | GT_EQ => Some(true),
                EQ2 | NEQ => Some(false),
                _ => None,
            });
        let Some(ordered) = ordered else {
            continue; // not a comparison operator
        };
        let operands: Vec<_> = bin.children().collect();
        let [lhs, rhs] = operands.as_slice() else {
            continue;
        };
        let lt = infer_arg_ty(state, uri, root, lhs);
        let rt = infer_arg_ty(state, uri, root, rhs);
        // skip only un-inferrable operands; literals are kept — `types_compatible` knows a
        // number literal pairs with integers but not with `address`/`bool`.
        if matches!(lt, typecheck::Ty::Unknown) || matches!(rt, typecheck::Ty::Unknown) {
            continue;
        }
        // cross-type: the operands aren't convertible to a common type (`address < uint`).
        if !types_compatible(state, uri, root, &lt, &rt)
            && !types_compatible(state, uri, root, &rt, &lt)
        {
            out.push(type_mismatch(
                li,
                &bin,
                &format!("cannot compare `{}` and `{}`", ty_label(&lt), ty_label(&rt)),
            ));
            continue;
        }
        // an ordered comparison (`< > <= >=`) needs ordered operands (`bytes`, `bool`,
        // structs, … support only `==`/`!=`, if anything).
        if ordered && (!is_ordered_comparable(&lt) || !is_ordered_comparable(&rt)) {
            let bad = if is_ordered_comparable(&lt) { &rt } else { &lt };
            out.push(type_mismatch(
                li,
                &bin,
                &format!("`{}` does not support ordered comparison", ty_label(bad)),
            ));
        }
    }
    out
}

/// Flag control-flow conditions whose concrete type is not `bool`. Unknown expressions
/// are skipped so incomplete type inference does not create false positives.
fn condition_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{DO_WHILE_STMT, FOR_STMT, IF_STMT, WHILE_STMT};
    let mut out = Vec::new();
    for stmt in root
        .descendants()
        .filter(|n| matches!(n.kind(), IF_STMT | WHILE_STMT | DO_WHILE_STMT | FOR_STMT))
    {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        let Some(condition) = condition_expr(&stmt) else {
            continue;
        };
        let ty = infer_arg_ty(state, uri, root, &condition);
        if matches!(
            ty,
            typecheck::Ty::Unknown | typecheck::Ty::Bool | typecheck::Ty::BoolLiteral
        ) {
            continue;
        }
        out.push(type_mismatch(
            li,
            &condition,
            &format!("condition must be `bool`, got `{}`", ty_label(&ty)),
        ));
    }
    out
}

/// The condition expression of an `if`/`while`/`do while`/`for` statement.
fn condition_expr(stmt: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{DO_WHILE_STMT, FOR_STMT, IF_STMT, SEMICOLON, WHILE_STMT};
    match stmt.kind() {
        IF_STMT | WHILE_STMT => stmt.children().next(),
        DO_WHILE_STMT => stmt.children().last(),
        FOR_STMT => {
            let semis: Vec<_> = stmt
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == SEMICOLON)
                .collect();
            let [first, second, ..] = semis.as_slice() else {
                return None;
            };
            stmt.children().find(|n| {
                first.text_range().end() <= n.text_range().start()
                    && n.text_range().end() <= second.text_range().start()
            })
        }
        _ => None,
    }
}

/// Whether a type supports the ordered comparison operators `< > <= >=` (integers,
/// `address`, `bytesN`, number literals). `bytes`/`string`/`bool`/user types do not.
fn is_ordered_comparable(ty: &typecheck::Ty) -> bool {
    use typecheck::Ty::*;
    matches!(
        ty,
        Uint(_) | Int(_) | Address | AddressPayable | BytesN(_) | NumberLiteral | HexLiteral
    )
}

/// A type that supports no arithmetic / bitwise / shift operator (so using it as such an
/// operand is an error). Integers, literals, `bytesN`, and `Unknown` are left alone.
fn is_non_arithmetic_type(ty: &typecheck::Ty) -> bool {
    use typecheck::Ty::*;
    matches!(
        ty,
        Address | AddressPayable | Bool | StringT | User(_) | Array(_) | FixedArray(_) | Mapping
    )
}

/// Whether a value of type `from` is implicitly convertible to `to`, resolving user-type
/// inheritance through the caller's file.
fn types_compatible(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    from: &typecheck::Ty,
    to: &typecheck::Ty,
) -> bool {
    typecheck::implicitly_convertible(from, to, &|a, b| is_subtype(state, uri, root, a, b))
}

/// A readable Solidity name for a type in a diagnostic message.
fn ty_label(ty: &typecheck::Ty) -> String {
    use typecheck::Ty::*;
    match ty {
        Uint(n) => format!("uint{n}"),
        Int(n) => format!("int{n}"),
        Address => "address".into(),
        AddressPayable => "address payable".into(),
        Bool => "bool".into(),
        StringT => "string".into(),
        Bytes => "bytes".into(),
        BytesN(n) => format!("bytes{n}"),
        Array(inner) | FixedArray(inner) => format!("{}[]", ty_label(inner)),
        Mapping => "mapping".into(),
        User(n) => n.clone(),
        NumberLiteral | HexLiteral | StringLiteral | BoolLiteral => "literal".into(),
        Unknown => "?".into(),
    }
}

/// The identifier text of a `NAME_REF`.
fn nameref_text(nr: &solsp_syntax::SyntaxNode) -> Option<String> {
    nr.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == solsp_syntax::SyntaxKind::IDENT)
        .map(|t| t.text().to_string())
}

/// Whether `name` (used as a value at `nr`) resolves to any declaration — a builtin,
/// something in scope, a same-file top-level, a cross-file import, or an inherited member.
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

/// A callable's overloads, each as its parameter `(name, type)` list.
type Overloads = Vec<Vec<(String, String)>>;

/// Type-check the positional call arguments in `root`: an argument whose inferred type is
/// not implicitly convertible to the parameter type yields a diagnostic. Conservative —
/// anything un-inferrable is left alone (see [`crate::typecheck`]).
fn type_check_diagnostics(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    li: &solsp_ide::LineIndex,
    deadline: Option<std::time::Instant>,
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, CALL_EXPR, NAMED_ARG_LIST};
    use std::cell::RefCell;
    use std::collections::HashMap;
    let mut out = Vec::new();
    // per-run caches: the same callee text resolves to the same overloads, and the same
    // (subtype, base) pair has a stable answer. Without this, a big forge-std-heavy test
    // file re-walked huge cheatcode files once per call and took tens of seconds.
    let mut callee_cache: HashMap<String, Option<Overloads>> = HashMap::new();
    let subtype_memo: RefCell<HashMap<(String, String), bool>> = RefCell::new(HashMap::new());

    for call in root.descendants().filter(|n| n.kind() == CALL_EXPR) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break; // background budget spent — the file's open/save pass will finish it
        }
        // the arguments: positional (`key = None`) or named (`key = Some`).
        let args: Vec<(Option<String>, solsp_syntax::SyntaxNode)> =
            if let Some(al) = call.children().find(|n| n.kind() == ARG_LIST) {
                al.children().map(|v| (None, v)).collect()
            } else if let Some(nal) = call.children().find(|n| n.kind() == NAMED_ARG_LIST) {
                named_arg_pairs(&nal)
            } else {
                continue;
            };
        let Some(callee) = call.first_child() else {
            continue;
        };
        // skip cheatcode / logging calls (`vm.*`, `console.*`): resolving them walks huge
        // forge-std files for no benefit, and they dominate test files.
        if is_cheatcode_receiver(&callee) {
            continue;
        }
        // every overload's parameter list, resolved once per distinct callee text.
        let key = callee.text().to_string();
        if !callee_cache.contains_key(&key) {
            let v = resolve_callee_overloads(state, uri, root, &callee);
            callee_cache.insert(key.clone(), v);
        }
        let Some(all_overloads) = callee_cache.get(&key).and_then(|v| v.as_ref()) else {
            continue;
        };
        // those of the matching arity (a small set; cloning keeps the rest simple).
        let overloads: Vec<Vec<(String, String)>> = all_overloads
            .iter()
            .filter(|params| params.len() == args.len())
            .cloned()
            .collect();
        if overloads.is_empty() {
            // no overload takes this many arguments — an arity error.
            let name = callee_display_name(&callee).unwrap_or_default();
            let counts: Vec<String> = all_overloads.iter().map(|p| p.len().to_string()).collect();
            let span = call
                .children()
                .find(|n| matches!(n.kind(), ARG_LIST | NAMED_ARG_LIST));
            out.push(type_mismatch(
                li,
                span.as_ref().unwrap_or(&call),
                &format!(
                    "`{name}` expects {} argument(s), but {} given",
                    counts.join(" or "),
                    args.len(),
                ),
            ));
            continue;
        }
        // infer the argument types once; `Unknown` args never contribute a mismatch.
        let arg_tys: Vec<typecheck::Ty> = args
            .iter()
            .map(|(_, v)| infer_arg_ty(state, uri, root, v))
            .collect();
        let is_base = |a: &str, b: &str| {
            let k = (a.to_string(), b.to_string());
            if let Some(&v) = subtype_memo.borrow().get(&k) {
                return v;
            }
            let v = is_subtype(state, uri, root, a, b);
            subtype_memo.borrow_mut().insert(k, v);
            v
        };
        // the parameter type an argument targets — by name for a named arg, else by
        // position. `None` if a named key doesn't match any parameter.
        let param_for = |params: &[(String, String)], i: usize| -> Option<String> {
            match &args[i].0 {
                Some(key) => params
                    .iter()
                    .find(|(pn, _)| pn == key)
                    .map(|(_, t)| t.clone()),
                None => params.get(i).map(|(_, t)| t.clone()),
            }
        };
        let accepts = |params: &[(String, String)]| {
            (0..args.len()).all(|i| {
                param_for(params, i).is_some_and(|p| {
                    typecheck::implicitly_convertible(
                        &arg_tys[i],
                        &typecheck::parse_ty(&p),
                        &is_base,
                    )
                })
            })
        };
        // a call is valid if SOME overload accepts every argument (Solidity resolves
        // overloads by argument type, which we approximate this way).
        if overloads.iter().any(|p| accepts(p)) {
            continue;
        }
        if overloads.len() == 1 {
            // unambiguous: flag each argument the single signature rejects.
            for (i, (_, value)) in args.iter().enumerate() {
                if matches!(arg_tys[i], typecheck::Ty::Unknown) {
                    continue;
                }
                let Some(ptype) = param_for(&overloads[0], i) else {
                    continue;
                };
                if !typecheck::implicitly_convertible(
                    &arg_tys[i],
                    &typecheck::parse_ty(&ptype),
                    &is_base,
                ) {
                    out.push(type_mismatch(li, value, &format!(
                        "argument of type `{}` is not implicitly convertible to expected type `{ptype}`",
                        arg_text(value),
                    )));
                }
            }
        } else {
            // overloaded and none matched → one diagnostic on the call.
            let name = callee_display_name(&callee).unwrap_or_default();
            let span = call
                .children()
                .find(|n| matches!(n.kind(), ARG_LIST | NAMED_ARG_LIST));
            out.push(type_mismatch(
                li,
                span.as_ref().unwrap_or(&call),
                &format!("no overload of `{name}` accepts these argument types"),
            ));
        }
    }
    out
}

/// Whether a callee is a member call on a forge-std cheatcode / logging handle
/// (`vm.*`, `console.*`, `console2.*`) — cheap to detect and not worth type-checking.
fn is_cheatcode_receiver(callee: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::MEMBER_EXPR;
    if callee.kind() != MEMBER_EXPR {
        return false;
    }
    callee
        .first_child()
        .and_then(|recv| solsp_hir::resolve::receiver_name(&recv))
        .is_some_and(|n| matches!(n.as_str(), "vm" | "console" | "console2"))
}

/// Every overload's parameter list (`(name, type)` pairs) for a call's callee, resolved
/// once per distinct callee. `None` for casts/types/builtins/unresolved/non-callables.
fn resolve_callee_overloads(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    callee: &solsp_syntax::SyntaxNode,
) -> Option<Overloads> {
    use solsp_hir::resolve::DefKind;
    let (def_uri, def) = resolve_named_callee(state, uri, root, callee)?;
    if !matches!(
        def.kind,
        DefKind::Function | DefKind::Event | DefKind::Error
    ) {
        return None;
    }
    let droot = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&droot);
    let name = callee_display_name(callee)?;
    Some(
        signature_candidates(&def, &def_node, &name, &droot)
            .into_iter()
            .map(|(_, n)| named_arg_fields(DefKind::Function, &n))
            .collect(),
    )
}

/// The `(key, value)` pairs of a `NAMED_ARG_LIST` (`{ a: x, b: y }`).
fn named_arg_pairs(
    nal: &solsp_syntax::SyntaxNode,
) -> Vec<(Option<String>, solsp_syntax::SyntaxNode)> {
    use solsp_syntax::SyntaxKind::NAME;
    let mut out = Vec::new();
    let mut key: Option<String> = None;
    for child in nal.children() {
        if child.kind() == NAME {
            key = node_ident(&child);
        } else {
            out.push((key.take(), child));
        }
    }
    out
}

fn arg_text(arg: &solsp_syntax::SyntaxNode) -> String {
    arg.text()
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn type_mismatch(
    li: &solsp_ide::LineIndex,
    node: &solsp_syntax::SyntaxNode,
    message: &str,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: to_proto::range(li, node.text_range()),
        severity: Some(lsp_types::DiagnosticSeverity::ERROR),
        source: Some("solsp".to_string()),
        message: message.to_string(),
        ..Default::default()
    }
}

/// The inferred [`typecheck::Ty`] of a call argument: a literal, a cast, or a value whose
/// declared/return type is read (`receiver_value_info`). `Unknown` when not inferrable.
fn infer_arg_ty(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    arg: &solsp_syntax::SyntaxNode,
) -> typecheck::Ty {
    use solsp_syntax::SyntaxKind::*;
    match arg.kind() {
        LITERAL_EXPR => {
            let tok = arg
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|t| !matches!(t.kind(), WHITESPACE | COMMENT));
            match tok.as_ref().map(|t| t.kind()) {
                // a hex literal (`0x…`) may also be an address / fixed-bytes value.
                Some(NUMBER)
                    if tok.as_ref().is_some_and(|t| {
                        t.text().starts_with("0x") || t.text().starts_with("0X")
                    }) =>
                {
                    typecheck::Ty::HexLiteral
                }
                Some(NUMBER) => typecheck::Ty::NumberLiteral,
                Some(STRING) => typecheck::Ty::StringLiteral,
                Some(TRUE_KW | FALSE_KW) => typecheck::Ty::BoolLiteral,
                _ => typecheck::Ty::Unknown,
            }
        }
        CALL_EXPR => {
            let Some(callee) = arg.first_child() else {
                return typecheck::Ty::Unknown;
            };
            // `new T[](n)` / `new T(...)` → the constructed type (the node after `new`).
            if callee.kind() == NEW_EXPR {
                return callee
                    .children()
                    .next()
                    .map(|t| typecheck::parse_ty(&node_type_text(&t)))
                    .unwrap_or(typecheck::Ty::Unknown);
            }
            let Some(cname) = callee_display_name(&callee) else {
                return typecheck::Ty::Unknown;
            };
            let parsed = typecheck::parse_ty(&cname);
            // an elementary cast: `uint8(x)`, `address(x)`, `bytes32(x)`.
            if !matches!(parsed, typecheck::Ty::User(_)) {
                return parsed;
            }
            // a user name: a contract/struct cast, or a function call (use its return type).
            match resolve_named_callee(state, uri, root, &callee) {
                Some((_, def)) if is_type_kind(def.kind) => typecheck::Ty::User(cname),
                _ => receiver_value_info(state, uri, root, arg)
                    .map(|(t, _)| typecheck::parse_ty(&t))
                    .unwrap_or(typecheck::Ty::Unknown),
            }
        }
        PATH_EXPR | NAME_REF | MEMBER_EXPR | INDEX_EXPR => {
            receiver_value_info(state, uri, root, arg)
                .map(|(t, _)| typecheck::parse_ty(&t))
                .unwrap_or(typecheck::Ty::Unknown)
        }
        _ => typecheck::Ty::Unknown,
    }
}

/// Whether user type `a` is `b` or has `b` somewhere in its inheritance (bases /
/// implemented interfaces). Resolves `a` from the caller's file; `true` when `a` can't be
/// resolved (conservative).
fn is_subtype(
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
        return true; // unknown type — never flag
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

/// The argument count of the call whose callee is the identifier at `offset` (for
/// overload resolution), or `None` if the cursor is not on a callee.
fn arity_at(root: &solsp_syntax::SyntaxNode, offset: rowan::TextSize) -> Option<usize> {
    let token = root
        .token_at_offset(offset)
        .find(|t| t.kind() == solsp_syntax::SyntaxKind::IDENT)?;
    let name_ref = token.parent()?;
    solsp_hir::resolve::call_arity(&name_ref)
}

/// Find an imported top-level symbol `name` referenced in `root` (following re-exports
/// transitively): the target file URI and the byte range of the declaration's name.
fn cross_file_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<(Url, rowan::TextRange)> {
    let (turi, def) = cross_file_definition(state, uri, root, name, arity)?;
    let troot = parse_root(state, &turi)?;
    Some((turi, def_name_range(&troot, &def)))
}

/// The target-file export name a local `name` refers to under an import's binding, or
/// `None` if this import does not bind it. Namespace imports (`* as N`) are skipped —
/// `N.member` access needs member resolution (a later step).
fn exported_name(kind: &solsp_hir::imports::ImportKind, name: &str) -> Option<String> {
    use solsp_hir::imports::ImportKind;
    match kind {
        ImportKind::Glob => Some(name.to_string()),
        ImportKind::Named(list) => list
            .iter()
            .find(|n| n.local() == name)
            .map(|n| n.name.clone()),
        ImportKind::Namespace(_) => None,
    }
}

/// Resolve a member access `receiver.member` at `offset`: returns the target file URI
/// and the member's [`Definition`]. Handles a receiver that is a type name
/// (contract/library/interface/struct/enum) or a variable (following its declared
/// type), same-file or imported.
fn member_resolve(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_syntax::SyntaxKind;
    // the clicked identifier must be the member side of a `receiver.member`.
    let token = root
        .token_at_offset(offset)
        .find(|t| t.kind() == SyntaxKind::IDENT)?;
    let member_ref = token.parent()?;
    let (receiver, member) = solsp_hir::resolve::member_access(&member_ref)?;
    // `obj.method(args)` — pick the overload matching the call's argument count.
    let arity = solsp_hir::resolve::call_arity(&member_ref);

    // `N.member` where `N` is an `import * as N` namespace alias → the imported file's
    // top-level symbol.
    if let Some(found) = namespace_member(state, uri, root, &receiver, &member, arity) {
        return Some(found);
    }

    if let Some((type_uri, type_def)) = resolve_receiver_type(state, uri, root, &receiver) {
        if is_super_receiver(&receiver) {
            let troot = parse_root(state, &type_uri)?;
            return inherited_base_member(state, &type_uri, &troot, &type_def, &member, arity);
        }
        if let Some(def) = member_lookup(state, &type_uri, &type_def, &member, arity) {
            return Some((type_uri, def));
        }
        // the member may be inherited from a cross-file base of the receiver's type.
        if let Some(troot) = parse_root(state, &type_uri) {
            if let Some(found) =
                inherited_member(state, &type_uri, &troot, &type_def, &member, arity)
            {
                return Some(found);
            }
        }
    }
    // `using L for T` — a library function attached to the receiver's type.
    using_member(state, uri, root, &receiver, &member, arity)
}

/// The file a `* as N` namespace import aliases, if `receiver` is that bare alias `N`.
fn namespace_target_uri(
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<Url> {
    use solsp_hir::imports::ImportKind::Namespace;
    use solsp_syntax::SyntaxKind::{NAME_REF, PATH_EXPR};
    if !matches!(receiver.kind(), PATH_EXPR | NAME_REF) {
        return None;
    }
    let alias = solsp_hir::resolve::receiver_name(receiver)?;
    solsp_hir::imports::imports(root)
        .into_iter()
        .find_map(|imp| {
            matches!(&imp.kind, Namespace(a) if *a == alias)
                .then(|| state::resolve_import_uri(uri, &imp.path))
                .flatten()
        })
}

/// Resolve `N.member` where `N` is a `* as N` namespace alias → the imported file's
/// top-level symbol (following re-exports).
fn namespace_member(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
    member: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    let turi = namespace_target_uri(uri, root, receiver)?;
    let tfile = state.file(&turi)?;
    let troot = solsp_base_db::parse(state.db(), tfile).syntax();
    if let Some(def) = solsp_hir::resolve::top_level_definition(&troot, member, arity) {
        return Some((turi, def));
    }
    cross_file_definition(state, &turi, &troot, member, arity)
}

/// Resolve the receiver of a member access to its type definition node and the file
/// that node lives in.
fn resolve_receiver_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    receiver_type(state, uri, root, receiver, false)
}

/// The type definition of an expression (structural, recursive). With `element`, the
/// array element type (for an indexed expression). Handles names, member access, calls
/// (→ the function's return type), indexing, and parentheses — so a chain like
/// `a.b().c[d].e` resolves segment by segment.
fn receiver_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    expr: &solsp_syntax::SyntaxNode,
    element: bool,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    use solsp_syntax::SyntaxKind::*;
    match expr.kind() {
        PAREN_EXPR | TUPLE_EXPR => receiver_type(state, uri, root, &expr.first_child()?, element),
        INDEX_EXPR => receiver_type(state, uri, root, &expr.first_child()?, true),
        CALL_EXPR => call_result_type(state, uri, root, expr, element),
        MEMBER_EXPR => {
            let recv = expr.first_child()?;
            let member = member_name(expr)?;
            // `N.Type` where `N` is an `import * as N` namespace alias → the imported type.
            if let Some((turi, def)) = namespace_member(state, uri, root, &recv, &member, None) {
                if is_type_kind(def.kind) && !element {
                    let troot = parse_root(state, &turi)?;
                    return Some((turi, def.full_ptr.to_node(&troot)));
                }
            }
            let (turi, tdef) = receiver_type(state, uri, root, &recv, false)?;
            let troot = parse_root(state, &turi)?;
            let (muri, mdef) = if is_super_receiver(&recv) {
                inherited_base_member(state, &turi, &troot, &tdef, &member, None)?
            } else {
                (
                    turi.clone(),
                    member_lookup(state, &turi, &tdef, &member, None)?,
                )
            };
            let mroot = parse_root(state, &muri)?;
            member_value_type(state, &muri, &mroot, &mdef, element)
        }
        PATH_EXPR | NAME_REF => {
            // `this` / `super` → the enclosing contract's type.
            if !element {
                if let Some(name) = solsp_hir::resolve::receiver_name(expr) {
                    if (name == "this" || name == "super") && enclosing_contract(expr).is_some() {
                        return Some((uri.clone(), enclosing_contract(expr)?));
                    }
                }
            }
            resolve_value_type(state, uri, root, expr, element)
        }
        _ => None,
    }
}

/// The result type of a call expression: the callee's return type, or — for a cast /
/// constructor `Type(x)` — the type itself.
fn call_result_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    call: &solsp_syntax::SyntaxNode,
    element: bool,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    use solsp_hir::resolve::DefKind;
    let callee = call.first_child()?;
    let arity = arg_count(call);
    let (def_uri, def) = resolve_callee(state, uri, root, &callee, arity)?;
    let def_root = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&def_root);
    match def.kind {
        DefKind::Function => {
            let ret = function_return_param(&def_node)?;
            let path_type = solsp_hir::resolve::decl_type_path(&ret, element)?;
            resolve_path_type(state, &def_uri, &def_root, &path_type)
        }
        _ if is_type_kind(def.kind) && !element => Some((def_uri, def_node)),
        _ => None,
    }
}

/// Resolve a call's callee to its declaration: a plain name (function, or a type for a
/// cast/constructor), or a member `obj.method`.
fn resolve_callee(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    callee: &solsp_syntax::SyntaxNode,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_syntax::SyntaxKind::*;
    match callee.kind() {
        PATH_EXPR | NAME_REF => {
            let nr = receiver_name_ref(callee)?;
            if let Some(def) = solsp_hir::resolve::resolve(&nr) {
                return Some((uri.clone(), def));
            }
            let name = solsp_hir::resolve::receiver_name(callee)?;
            if let Some(found) = cross_file_definition(state, uri, root, &name, arity) {
                return Some(found);
            }
            // a bare call to an internal/private method inherited from a cross-file base.
            let contract = enclosing_contract(callee)?;
            inherited_member(state, uri, root, &contract, &name, arity)
        }
        MEMBER_EXPR => {
            let recv = callee.first_child()?;
            let member = member_name(callee)?;
            let (turi, tdef) = receiver_type(state, uri, root, &recv, false)?;
            if is_super_receiver(&recv) {
                let troot = parse_root(state, &turi)?;
                return inherited_base_member(state, &turi, &troot, &tdef, &member, arity);
            }
            // same-file C3 first, then cross-file inheritance.
            if let Some(mdef) = member_lookup(state, &turi, &tdef, &member, arity) {
                return Some((turi, mdef));
            }
            let troot = parse_root(state, &turi)?;
            inherited_member(state, &turi, &troot, &tdef, &member, arity)
        }
        _ => None,
    }
}

/// The type of a member (`a.b` as a value): a field/variable follows its declared type;
/// a nested type is itself. With `element`, the array element type.
fn member_value_type(
    state: &ServerState,
    member_uri: &Url,
    member_root: &solsp_syntax::SyntaxNode,
    mdef: &solsp_hir::resolve::Definition,
    element: bool,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    use solsp_hir::resolve::DefKind;
    let node = mdef.full_ptr.to_node(member_root);
    match mdef.kind {
        DefKind::Contract
        | DefKind::Interface
        | DefKind::Library
        | DefKind::Struct
        | DefKind::Enum
            if !element =>
        {
            Some((member_uri.clone(), node))
        }
        DefKind::StateVariable | DefKind::Field | DefKind::Local | DefKind::Parameter => {
            let path_type = solsp_hir::resolve::decl_type_path(&node, element)?;
            resolve_path_type(state, member_uri, member_root, &path_type)
        }
        _ => None,
    }
}

/// The member name of a `MEMBER_EXPR` (`b` in `a.b`).
fn member_name(member_expr: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::{IDENT, NAME_REF};
    let nr = member_expr.children().nth(1)?; // [receiver, member]
    if nr.kind() != NAME_REF {
        return None;
    }
    nr.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == IDENT)
        .map(|t| t.text().to_string())
}

/// The argument count of a call's `ARG_LIST` / `NAMED_ARG_LIST`.
fn arg_count(call: &solsp_syntax::SyntaxNode) -> Option<usize> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, NAMED_ARG_LIST};
    let args = call
        .children()
        .find(|n| matches!(n.kind(), ARG_LIST | NAMED_ARG_LIST))?;
    Some(args.children().count())
}

/// The first `PARAM` of a function's return list (its second `PARAM_LIST`).
fn function_return_param(func: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::{PARAM, PARAM_LIST};
    let returns = func.children().filter(|n| n.kind() == PARAM_LIST).nth(1)?;
    returns.children().find(|n| n.kind() == PARAM)
}

/// Resolve a receiver to a type def. With `element`, take the array element type
/// (the receiver was indexed). A bare type name resolves to itself; a variable follows
/// its declared type. Same-file lexical resolution first, then imported symbols.
fn resolve_value_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
    element: bool,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    use solsp_hir::resolve::DefKind;
    let name = solsp_hir::resolve::receiver_name(receiver)?;
    let recv_ref = receiver_name_ref(receiver)?;

    let (def_uri, def) = solsp_hir::resolve::resolve(&recv_ref)
        .map(|d| (uri.clone(), d))
        .or_else(|| cross_file_definition(state, uri, root, &name, None))
        .or_else(|| {
            // an inherited member from a cross-file base (e.g. forge-std's `vm`).
            let contract = enclosing_contract(receiver)?;
            inherited_member(state, uri, root, &contract, &name, None)
        })?;
    let def_root = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&def_root);

    match def.kind {
        // the receiver IS a type (only meaningful without indexing).
        DefKind::Contract
        | DefKind::Interface
        | DefKind::Library
        | DefKind::Struct
        | DefKind::Enum
            if !element =>
        {
            Some((def_uri, def_node))
        }
        // the receiver is a value; follow its declared (or element) type path.
        DefKind::StateVariable | DefKind::Parameter | DefKind::Local => {
            let path_type = solsp_hir::resolve::decl_type_path(&def_node, element)?;
            resolve_path_type(state, &def_uri, &def_root, &path_type)
        }
        _ => None,
    }
}

/// The nearest enclosing contract/interface/library definition of a node.
fn enclosing_contract(node: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    node.ancestors()
        .find(|n| n.kind() == solsp_syntax::SyntaxKind::CONTRACT_DEF)
}

/// Resolve a base contract name to its definition node and file — same-file first, then
/// an imported base.
fn resolve_base(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    base_name: &str,
) -> Option<(Url, solsp_syntax::SyntaxNode, solsp_syntax::SyntaxNode)> {
    if let Some(node) = solsp_hir::resolve::find_contract(root, base_name) {
        return Some((uri.clone(), root.clone(), node));
    }
    let (buri, bdef) = cross_file_definition(state, uri, root, base_name, None)?;
    if !is_type_kind(bdef.kind) {
        return None;
    }
    let broot = parse_root(state, &buri)?;
    let bnode = bdef.full_ptr.to_node(&broot);
    Some((buri, broot, bnode))
}

/// Look up `name` as a member inherited by `contract`, walking its base contracts across
/// files (BFS, diamond-safe). Returns the file + [`Definition`] of the first match.
fn inherited_member(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    inherited_member_impl(state, uri, root, contract, name, arity, true)
}

/// Resolve a `super.name` target: direct and transitive bases only, never the current
/// contract's override.
fn inherited_base_member(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    inherited_member_impl(state, uri, root, contract, name, arity, false)
}

fn inherited_member_impl(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    contract: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
    include_self: bool,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use std::collections::{HashSet, VecDeque};
    let mut visited: HashSet<(Url, String)> = HashSet::new();
    // (uri, root, contract, is_base) — a base's `private` member is not accessible here.
    let mut queue: VecDeque<(
        Url,
        solsp_syntax::SyntaxNode,
        solsp_syntax::SyntaxNode,
        bool,
    )> = VecDeque::new();
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
            continue; // already searched this contract (diamond)
        }
        if let Some(def) = solsp_hir::resolve::contract_member(&c, name, arity) {
            if !is_base || !solsp_hir::resolve::is_private(&def.full_ptr.to_node(&r)) {
                return Some((u, def));
            }
            // a private base member — not accessible from here; keep searching.
        }
        for base in solsp_hir::resolve::base_names(&c) {
            if let Some((bu, br, bn)) = resolve_base(state, &u, &r, &base) {
                queue.push_back((bu, br, bn, true));
            }
        }
    }
    None
}

/// Go-to-def target for a bare name used inside a contract that resolves to an inherited
/// member from a cross-file base (e.g. a forge-std `Test` helper). Skips member-access
/// positions (handled by member resolution).
fn inherited_name_at(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    use solsp_syntax::SyntaxKind::{IDENT, MEMBER_EXPR, NAME_REF};
    let token = root.token_at_offset(offset).find(|t| t.kind() == IDENT)?;
    let nr = token.parent()?;
    if nr.kind() != NAME_REF {
        return None;
    }
    // the `.member` of `recv.member` is member resolution's job, not inheritance.
    if let Some(p) = nr.parent() {
        if p.kind() == MEMBER_EXPR && p.first_child().as_ref() != Some(&nr) {
            return None;
        }
    }
    let contract = enclosing_contract(&nr)?;
    let arity = solsp_hir::resolve::call_arity(&nr);
    inherited_member(state, uri, root, &contract, token.text(), arity)
}

/// Resolve a type path node (`IRoles` or qualified `ICraftV2.TokenInput`) to its type
/// definition and file: the first segment is a top-level/imported type, each further
/// segment a nested type member of the previous.
fn resolve_path_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    path_type: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    let segments = solsp_hir::resolve::path_type_segments(path_type);
    let (first, rest) = segments.split_first()?;
    let (turi, mut type_def) = resolve_type_by_name(state, uri, root, first, Some(path_type))?;
    for seg in rest {
        let member = member_lookup(state, &turi, &type_def, seg, None)?;
        if !is_type_kind(member.kind) {
            return None;
        }
        let troot = parse_root(state, &turi)?; // nested types live in the same file
        type_def = member.full_ptr.to_node(&troot);
    }
    Some((turi, type_def))
}

/// Resolve a *type* name to its definition node and file: same-file top-level first,
/// then an imported type.
fn resolve_type_by_name(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    type_name: &str,
    context: Option<&solsp_syntax::SyntaxNode>,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    // resolving a type name cross-file is hot and repeats across a file's many uses of the
    // same type, so memoize it keyed by (file, name, enclosing contract).
    let key = (
        uri.to_string(),
        type_name.to_string(),
        context.and_then(enclosing_contract).map(|c| c.text_range()),
    );
    let resolved = match state.cached_type(&key) {
        Some(hit) => hit,
        None => {
            let r = resolve_type_def_by_name(state, uri, root, type_name, context);
            state.cache_type(key, r.clone());
            r
        }
    };
    let (turi, def) = resolved?;
    let troot = parse_root(state, &turi)?;
    Some((turi, def.full_ptr.to_node(&troot)))
}

/// The uncached resolution behind [`resolve_type_by_name`], returning the definition (the
/// node is rebuilt by the caller from the cache).
fn resolve_type_def_by_name(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    type_name: &str,
    context: Option<&solsp_syntax::SyntaxNode>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    // 1. a contract-nested type visible where the name is used (its enclosing contract +
    //    cross-file bases) — these shadow file scope.
    if let Some(contract) = context.and_then(enclosing_contract) {
        if let Some(def) = member_lookup(state, uri, &contract, type_name, None) {
            if is_type_kind(def.kind) {
                return Some((uri.clone(), def));
            }
        }
        if let Some((turi, def)) = inherited_member(state, uri, root, &contract, type_name, None) {
            if is_type_kind(def.kind) {
                return Some((turi, def));
            }
        }
    }
    // 2. a top-level type in this file (via the cached file index).
    if let Some(index) = state.file_index(uri) {
        if let Some(def) = solsp_hir::resolve::select_named(&index.defs, type_name, None, root) {
            if is_type_kind(def.kind) {
                return Some((uri.clone(), def));
            }
        }
    }
    // 3. an imported type.
    let (turi, def) = cross_file_definition(state, uri, root, type_name, None)?;
    is_type_kind(def.kind).then_some((turi, def))
}

fn is_type_kind(kind: solsp_hir::resolve::DefKind) -> bool {
    use solsp_hir::resolve::DefKind::*;
    matches!(
        kind,
        Contract | Interface | Library | Struct | Enum | UserType
    )
}

/// Resolve a symbol `name` referenced in `root` to its declaration via the import graph,
/// following re-exports transitively to full depth (cycle-safe). A glob `import "X"`
/// re-exports everything `X` itself imports, so a symbol can sit several files away from
/// where it is used. Returns the file + [`Definition`].
fn cross_file_definition(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    let mut visited = std::collections::HashSet::new();
    cross_file_rec(state, uri, root, name, arity, &mut visited)
}

fn cross_file_rec(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    name: &str,
    arity: Option<usize>,
    // keyed by (file, name): the same file may be searched for different names across a
    // file's imports (e.g. a glob import probes it for `U`, an alias for `Utils`).
    visited: &mut std::collections::HashSet<(Url, String)>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    if !visited.insert((uri.clone(), name.to_string())) {
        return None; // already searched this file for this name (import cycle)
    }
    let _ = root; // imports now come from the cached index, not a fresh tree walk
    let index = state.file_index(uri)?;
    for imp in &index.imports {
        let Some(export) = exported_name(&imp.kind, name) else {
            continue;
        };
        let Some(target_uri) = imp.target.clone() else {
            continue;
        };
        let Some(tindex) = state.file_index(&target_uri) else {
            continue;
        };
        let troot = parse_root(state, &target_uri)?;
        // a top-level declaration in the imported file…
        if let Some(def) = solsp_hir::resolve::select_named(&tindex.defs, &export, arity, &troot) {
            return Some((target_uri, def));
        }
        // …or one the imported file itself re-exports (transitively).
        if let Some(found) = cross_file_rec(state, &target_uri, &troot, &export, arity, visited) {
            return Some(found);
        }
    }
    None
}

/// Look up `member` in a type, caching a contract's member list to avoid re-walking its
/// body and same-file C3 bases on every access (the dominant member-resolution cost on
/// big types). `type_uri` is the file `type_def` lives in. Only the common arity-free
/// contract lookup is cached; struct/enum and overload-by-arity take the direct path,
/// which preserves exact base-precedence semantics.
fn member_lookup(
    state: &ServerState,
    type_uri: &Url,
    type_def: &solsp_syntax::SyntaxNode,
    member: &str,
    arity: Option<usize>,
) -> Option<solsp_hir::resolve::Definition> {
    if arity.is_some() || type_def.kind() != solsp_syntax::SyntaxKind::CONTRACT_DEF {
        return solsp_hir::resolve::member_in_type(type_def, member, arity);
    }
    // arity-free contract lookup = first member of that name in C3 order.
    state
        .member_index(type_uri, type_def)
        .iter()
        .find(|d| d.name == member)
        .cloned()
}

/// The `NAME_REF` node of a receiver expression (`PATH_EXPR` → `NAME_REF`, or a bare
/// `NAME_REF`).
fn receiver_name_ref(receiver: &solsp_syntax::SyntaxNode) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::NAME_REF;
    if receiver.kind() == NAME_REF {
        Some(receiver.clone())
    } else {
        receiver.children().find(|n| n.kind() == NAME_REF)
    }
}

/// Parse the current tree of a tracked file.
fn parse_root(state: &ServerState, uri: &Url) -> Option<solsp_syntax::SyntaxNode> {
    let file = state.file(uri)?;
    Some(solsp_base_db::parse(state.db(), file).syntax())
}

fn reference_target_at(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<RefTarget> {
    if let Some((turi, def)) = typed_overload_target(state, uri, root, offset) {
        return definition_target(state, turi, &def);
    }
    if let Some(def) = solsp_hir::resolve::definition_at(root, offset) {
        return Some(RefTarget {
            uri: uri.clone(),
            range: def_name_range(root, &def),
        });
    }
    if let Some((turi, def)) = member_resolve(state, uri, root, offset) {
        return definition_target(state, turi, &def);
    }
    if let Some((turi, def)) = inherited_name_at(state, uri, root, offset) {
        return definition_target(state, turi, &def);
    }
    let name = solsp_ide::navigation::name_at(root, offset)?;
    let arity = arity_at(root, offset);
    let (turi, def) = cross_file_definition(state, uri, root, &name, arity)?;
    definition_target(state, turi, &def)
}

fn definition_target(
    state: &ServerState,
    uri: Url,
    def: &solsp_hir::resolve::Definition,
) -> Option<RefTarget> {
    let root = parse_root(state, &uri)?;
    Some(RefTarget {
        range: def_name_range(&root, def),
        uri,
    })
}

/// The byte range of a definition's name identifier within `root`.
fn def_name_range(
    root: &solsp_syntax::SyntaxNode,
    def: &solsp_hir::resolve::Definition,
) -> rowan::TextRange {
    use solsp_syntax::SyntaxKind::IDENT;
    let name_node = def.name_ptr.to_node(root);
    name_node
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == IDENT)
        .map(|t| t.text_range())
        .unwrap_or_else(|| name_node.text_range())
}

/// Handle a notification: open/change update the store and republish diagnostics;
/// close drops the doc and clears its diagnostics. Unknown notifications are ignored.
fn handle_notification(
    connection: &Connection,
    state: &mut ServerState,
    not: Notification,
) -> Result<()> {
    match not.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let Some(params) = extract_notification::<DidOpenTextDocument>(not) else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            state.set(&uri, params.text_document.text);
            state.load_import_graph(&uri); // pull imported files into the db
            publish_diagnostics(connection, state, &uri, true, None)?;
        }
        DidChangeTextDocument::METHOD => {
            let Some(params) = extract_notification::<DidChangeTextDocument>(not) else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            // INCREMENTAL sync: apply each content change in order to the current text
            // (each is relative to the document after the previous change), then reset
            // the whole text — full-document changes (range: None) also work.
            let Some(mut text) = state.text(&uri) else {
                return Ok(());
            };
            for change in params.content_changes {
                apply_change(&mut text, change);
            }
            state.set(&uri, text);
            state.load_import_graph(&uri); // imports may have changed
                                           // syntax-only while typing; the slow semantic pass runs on open/save.
            publish_diagnostics(connection, state, &uri, false, None)?;
        }
        DidSaveTextDocument::METHOD => {
            let Some(params) = extract_notification::<DidSaveTextDocument>(not) else {
                return Ok(());
            };
            publish_diagnostics(connection, state, &params.text_document.uri, true, None)?;
        }
        DidCloseTextDocument::METHOD => {
            let Some(params) = extract_notification::<DidCloseTextDocument>(not) else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            // Refresh the file from disk (it may still be imported by open files). Keep its
            // project-wide diagnostics in the tree by re-diagnosing the on-disk version,
            // rather than clearing — unless the file is gone.
            state.reload_or_drop(&uri);
            if state.file(&uri).is_some() {
                publish_diagnostics(
                    connection,
                    state,
                    &uri,
                    true,
                    Some(std::time::Duration::from_millis(150)),
                )?;
            } else {
                send_diagnostics(connection, uri, Vec::new())?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unused_import_respects_forge_lint_disable_next_line() {
        let uri = Url::parse("file:///Base.sol").unwrap();
        let src = "/// forge-lint: disable-next-line(unused-import)\n\
                   import { TransferHelper } from \"./Helper.sol\";\n\
                   import { Other } from \"./Helper.sol\";\n\
                   contract Base {}\n";
        let mut state = ServerState::default();
        state.set(&uri, src.to_string());
        let file = state.file(&uri).unwrap();
        let root = solsp_base_db::parse(state.db(), file).syntax();
        let li = state.line_index(&uri).unwrap();

        let diags = unused_import_diagnostics(&state, &uri, &root, li, None);
        let messages: Vec<_> = diags.iter().map(|diag| diag.message.as_str()).collect();
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("TransferHelper")),
            "{messages:?}"
        );
        assert!(
            messages.iter().any(|message| message.contains("Other")),
            "{messages:?}"
        );
    }
}
