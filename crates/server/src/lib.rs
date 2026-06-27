//! `solsp-server` library: the LSP protocol layer (capabilities, dispatch loop,
//! handlers) over the pure `solsp-ide` features. The `solsp-server` binary is a thin
//! shim around [`run`]; integration tests drive the same code over an in-memory
//! transport (design §5, §6).

use anyhow::Result;
use lsp_server::{
    Connection, ErrorCode, ExtractError, Message, Notification, Request, RequestId, Response,
};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{
    DocumentSymbolRequest, GotoDefinition, HoverRequest, Request as _, SemanticTokensFullRequest,
};
use lsp_types::{
    DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverContents, HoverParams, HoverProviderCapability, Location, MarkupContent,
    MarkupKind, OneOf, PublishDiagnosticsParams, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensParams, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, TextDocumentContentChangeEvent, TextDocumentSyncCapability,
    TextDocumentSyncKind, Url, WorkDoneProgressOptions,
};
use solsp_ide::LineIndex;

pub mod state;
pub mod to_proto;

use state::ServerState;

/// What the server advertises at `initialize`: full-text sync, an outline provider,
/// and semantic tokens (full-document) with our legend.
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        document_symbol_provider: Some(OneOf::Left(true)),
        definition_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                work_done_progress_options: WorkDoneProgressOptions::default(),
                legend: to_proto::legend(),
                range: None,
                full: Some(SemanticTokensFullOptions::Bool(true)),
            },
        )),
        ..Default::default()
    }
}

/// Run the main loop until the client shuts the connection down. Assumes the
/// `initialize`/`initialized` handshake has already completed.
pub fn run(connection: &Connection) -> Result<()> {
    let mut state = ServerState::default();
    for msg in &connection.receiver {
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
    Ok(())
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
        HoverRequest::METHOD => match req.extract::<HoverParams>(HoverRequest::METHOD) {
            Ok((id, params)) => Response::new_ok(id, hover(state, params)),
            Err(e) => extract_err_response(id, e),
        },
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
    // 3. an imported top-level symbol (a use site, or a name inside `{ ... }`) → jump
    //    into the target file.
    if let Some(name) = solsp_ide::navigation::name_at(&root, offset) {
        if let Some((target_uri, range)) = cross_file_target(state, &uri, &root, &name) {
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

/// `textDocument/hover` → the definition's signature + kind as markdown (or `None`).
fn hover(state: &ServerState, params: HoverParams) -> Option<Hover> {
    let pos = params.text_document_position_params;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
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
    // 3. an imported top-level symbol → hover from the target file.
    let name = solsp_ide::navigation::name_at(&root, offset)?;
    for imp in solsp_hir::imports::imports(&root) {
        let Some(export) = exported_name(&imp.kind, &name) else {
            continue;
        };
        let Some(target_uri) = state::resolve_import_uri(&uri, &imp.path) else {
            continue;
        };
        let Some(tfile) = state.file(&target_uri) else {
            continue;
        };
        let troot = solsp_base_db::parse(state.db(), tfile).syntax();
        if let Some(info) = solsp_ide::navigation::hover_top_level(&troot, &export) {
            // The hovered identifier is in *this* file; report no range (the target
            // range would be in the wrong document) and let the client highlight it.
            return Some(markup_hover(info.contents, None));
        }
    }
    None
}

/// Find an imported top-level symbol `name` referenced in `root`: returns the target
/// file URI and the byte range (in that file) of the declaration's name.
fn cross_file_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    name: &str,
) -> Option<(Url, rowan::TextRange)> {
    for imp in solsp_hir::imports::imports(root) {
        let Some(export) = exported_name(&imp.kind, name) else {
            continue;
        };
        let Some(target_uri) = state::resolve_import_uri(uri, &imp.path) else {
            continue;
        };
        let Some(tfile) = state.file(&target_uri) else {
            continue;
        };
        let troot = solsp_base_db::parse(state.db(), tfile).syntax();
        if let Some(range) = solsp_ide::navigation::goto_top_level(&troot, &export) {
            return Some((target_uri, range));
        }
    }
    None
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

    let (type_uri, type_def) = resolve_receiver_type(state, uri, root, &receiver)?;
    let member_def = solsp_hir::resolve::member_in_type(&type_def, &member)?;
    Some((type_uri, member_def))
}

/// Resolve the receiver of a member access to its type definition node and the file
/// that node lives in. A type name resolves to itself; a variable follows its declared
/// type. Same-file lexical resolution first, then imported symbols.
fn resolve_receiver_type(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    use solsp_hir::resolve::DefKind;
    let name = solsp_hir::resolve::receiver_name(receiver)?;
    let recv_ref = receiver_name_ref(receiver)?;

    // resolve the receiver: same-file lexical, else an imported top-level symbol.
    let (def_uri, def) = match solsp_hir::resolve::resolve(&recv_ref) {
        Some(def) => (uri.clone(), def),
        None => cross_file_definition(state, uri, root, &name)?,
    };
    let def_root = parse_root(state, &def_uri)?;
    let def_node = def.full_ptr.to_node(&def_root);

    match def.kind {
        // the receiver IS a type.
        DefKind::Contract
        | DefKind::Interface
        | DefKind::Library
        | DefKind::Struct
        | DefKind::Enum => Some((def_uri, def_node)),
        // the receiver is a value; follow its declared type.
        DefKind::StateVariable | DefKind::Parameter | DefKind::Local => {
            let type_name = solsp_hir::resolve::declared_type_name(&def_node)?;
            resolve_type_by_name(state, &def_uri, &def_root, &type_name)
        }
        _ => None,
    }
}

/// Resolve a *type* name to its definition node and file: same-file top-level first,
/// then an imported type.
fn resolve_type_by_name(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    type_name: &str,
) -> Option<(Url, solsp_syntax::SyntaxNode)> {
    if let Some(def) = solsp_hir::resolve::top_level_definition(root, type_name) {
        if is_type_kind(def.kind) {
            return Some((uri.clone(), def.full_ptr.to_node(root)));
        }
    }
    let (turi, def) = cross_file_definition(state, uri, root, type_name)?;
    if is_type_kind(def.kind) {
        let troot = parse_root(state, &turi)?;
        return Some((turi, def.full_ptr.to_node(&troot)));
    }
    None
}

fn is_type_kind(kind: solsp_hir::resolve::DefKind) -> bool {
    use solsp_hir::resolve::DefKind::*;
    matches!(
        kind,
        Contract | Interface | Library | Struct | Enum | UserType
    )
}

/// Like [`cross_file_target`], but returns the resolved [`Definition`] (and the file
/// it lives in) rather than a range — for further member/type resolution.
fn cross_file_definition(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    name: &str,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    for imp in solsp_hir::imports::imports(root) {
        let Some(export) = exported_name(&imp.kind, name) else {
            continue;
        };
        let Some(target_uri) = state::resolve_import_uri(uri, &imp.path) else {
            continue;
        };
        let Some(tfile) = state.file(&target_uri) else {
            continue;
        };
        let troot = solsp_base_db::parse(state.db(), tfile).syntax();
        if let Some(def) = solsp_hir::resolve::top_level_definition(&troot, &export) {
            return Some((target_uri, def));
        }
    }
    None
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

/// Wrap markdown text (and an optional range) into an LSP `Hover`.
fn markup_hover(value: String, range: Option<lsp_types::Range>) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range,
    }
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
            publish_diagnostics(connection, state, &uri)?;
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
            publish_diagnostics(connection, state, &uri)?;
        }
        DidCloseTextDocument::METHOD => {
            let Some(params) = extract_notification::<DidCloseTextDocument>(not) else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            // Keep the file loaded (it may be imported by open files) but refresh it
            // from disk; just stop showing its diagnostics.
            state.reload_or_drop(&uri);
            send_diagnostics(connection, uri, Vec::new())?;
        }
        _ => {}
    }
    Ok(())
}

/// Apply one LSP content change to `text`. A change with a `range` splices the
/// replacement over those bytes (range is in UTF-16 line/col, mapped via a fresh
/// `LineIndex` over the current text); a change without a range replaces the whole
/// document. Out-of-range edits are ignored rather than panicking.
fn apply_change(text: &mut String, change: TextDocumentContentChangeEvent) {
    let Some(range) = change.range else {
        *text = change.text;
        return;
    };
    let li = LineIndex::new(text);
    let (Some(start), Some(end)) = (
        to_proto::offset(&li, range.start),
        to_proto::offset(&li, range.end),
    ) else {
        return;
    };
    let (start, end) = (u32::from(start) as usize, u32::from(end) as usize);
    if start <= end && end <= text.len() {
        text.replace_range(start..end, &change.text);
    }
}

/// Extract a notification's params, or `None` (logging) on malformed params. Crucial:
/// a bad notification must NOT abort the main loop — unlike a request, it has no id to
/// answer, so we skip it rather than propagate the error out of `run`.
fn extract_notification<N>(not: Notification) -> Option<N::Params>
where
    N: lsp_types::notification::Notification,
{
    match not.extract::<N::Params>(N::METHOD) {
        Ok(params) => Some(params),
        Err(e) => {
            eprintln!(
                "solsp: ignoring malformed {} notification: {e:?}",
                N::METHOD
            );
            None
        }
    }
}

/// Compute and publish diagnostics for an open document (empty list if missing).
fn publish_diagnostics(connection: &Connection, state: &ServerState, uri: &Url) -> Result<()> {
    let diagnostics = match (state.file(uri), state.line_index(uri)) {
        (Some(file), Some(li)) => {
            let parse = solsp_base_db::parse(state.db(), file);
            let bare = solsp_ide::diagnostics::diagnostics(parse.errors());
            to_proto::diagnostics(&bare, li)
        }
        _ => Vec::new(),
    };
    send_diagnostics(connection, uri.clone(), diagnostics)
}

/// Send a `textDocument/publishDiagnostics` notification.
fn send_diagnostics(
    connection: &Connection,
    uri: Url,
    diagnostics: Vec<lsp_types::Diagnostic>,
) -> Result<()> {
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: None,
    };
    let not = Notification::new(PublishDiagnostics::METHOD.to_string(), params);
    connection.sender.send(Message::Notification(not))?;
    Ok(())
}

/// Turn an `extract` failure into a JSON-RPC error response under the request's own
/// id (captured by the caller, since `JsonError` does not carry it).
fn extract_err_response(id: RequestId, err: ExtractError<Request>) -> Response {
    let (code, message) = match err {
        // Unreachable here — the caller already matched the method — but mapped for
        // completeness.
        ExtractError::MethodMismatch(req) => (
            ErrorCode::MethodNotFound,
            format!("method mismatch: {}", req.method),
        ),
        ExtractError::JsonError { method, error } => (
            ErrorCode::InvalidParams,
            format!("invalid params for {method}: {error}"),
        ),
    };
    Response::new_err(id, code as i32, message)
}
