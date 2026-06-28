//! `solsp-server` library: the LSP protocol layer (capabilities, dispatch loop,
//! handlers) over the pure `solsp-ide` features. The `solsp-server` binary is a thin
//! shim around [`run`]; integration tests drive the same code over an in-memory
//! transport (design §5, §6).

use anyhow::Result;
use lsp_server::{
    Connection, ErrorCode, ExtractError, Message, Notification, Request, RequestId, Response,
};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    Completion, DocumentSymbolRequest, GotoDefinition, HoverRequest, Request as _,
    SemanticTokensFullRequest, SignatureHelpRequest,
};
use lsp_types::{
    Command, CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams,
    CompletionResponse, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability,
    InsertTextFormat, Location, MarkupContent, MarkupKind, OneOf, ParameterInformation,
    ParameterLabel, PublishDiagnosticsParams, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensParams, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, SignatureHelp, SignatureHelpOptions, SignatureHelpParams,
    SignatureInformation, TextDocumentContentChangeEvent, TextDocumentSyncCapability,
    TextDocumentSyncKind, Url, WorkDoneProgressOptions,
};
use solsp_ide::LineIndex;

pub mod state;
pub mod to_proto;
pub mod typecheck;

use state::ServerState;

/// What the server advertises at `initialize`: full-text sync, an outline provider,
/// and semantic tokens (full-document) with our legend.
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        // incremental sync, plus save notifications so the semantic type-check can run on
        // save (it is too slow to run on every keystroke).
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            lsp_types::TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(lsp_types::TextDocumentSyncSaveOptions::Supported(true)),
                ..Default::default()
            },
        )),
        document_symbol_provider: Some(OneOf::Left(true)),
        definition_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            // `.` triggers member completion; bare-identifier completion is implicit.
            trigger_characters: Some(vec![".".to_string()]),
            ..Default::default()
        }),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
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

/// `textDocument/hover` → the definition's signature + kind as markdown (or `None`).
fn hover(state: &ServerState, params: HoverParams) -> Option<Hover> {
    let pos = params.text_document_position_params;
    let uri = pos.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, pos.position)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    // 0. a named-argument key (`f({ owner_: … })`) → its parameter/field type.
    if let Some(h) = named_arg_hover(state, &uri, &root, offset) {
        return Some(h);
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
        let is_super = solsp_hir::resolve::receiver_name(&receiver).as_deref() == Some("super");
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
        let same_file = solsp_hir::resolve::type_members(&tdef)
            .into_iter()
            .filter(|d| keep(&d.full_ptr.to_node(&troot)))
            .collect();
        let mut items = completion_items_from(same_file);
        // contracts inherit across files (libraries do not); add those members.
        if contract_like && !library {
            items.extend(completion_items_from(collect_inherited_members(
                state, &turi, &troot, &tdef, external,
            )));
        }
        items.extend(using_items);
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
    // an elementary value with only `using L for T` functions (e.g. `uint256.toString`).
    if !using_items.is_empty() {
        return Some(using_items);
    }
    Some(Vec::new())
}

/// Build completion items from `(name, detail, is_method)` triples — synthetic builtin
/// members. Methods insert call parens.
fn synthetic_members(items: &[(&str, &str, bool)]) -> Vec<CompletionItem> {
    items
        .iter()
        .map(|&(name, detail, method)| {
            let (insert_text, insert_text_format) = if method {
                (Some(format!("{name}($0)")), Some(InsertTextFormat::SNIPPET))
            } else {
                (None, None)
            };
            CompletionItem {
                kind: Some(if method {
                    CompletionItemKind::METHOD
                } else {
                    CompletionItemKind::FIELD
                }),
                detail: Some(if detail.is_empty() {
                    "builtin".to_string()
                } else {
                    detail.to_string()
                }),
                insert_text,
                insert_text_format,
                label: name.to_string(),
                ..Default::default()
            }
        })
        .collect()
}

/// Members of `type(X)`: integer `min`/`max`, enum `min`/`max`, or a contract/interface's
/// `name`/`creationCode`/`runtimeCode`/`interfaceId`. `None` if the receiver isn't `type(X)`.
fn type_expr_members(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<Vec<CompletionItem>> {
    use solsp_syntax::SyntaxKind::{ENUM_DEF, PATH_TYPE, TYPE_EXPR};
    if receiver.kind() != TYPE_EXPR {
        return None;
    }
    let pt = receiver.children().find(|n| n.kind() == PATH_TYPE)?;
    let name = solsp_hir::resolve::path_type_segments(&pt).pop()?;
    if is_integer_type_name(&name) {
        return Some(synthetic_members(&[("min", "", false), ("max", "", false)]));
    }
    if let Some((_, tdef)) = resolve_path_type(state, uri, root, &pt) {
        return Some(match tdef.kind() {
            ENUM_DEF => synthetic_members(&[("min", "", false), ("max", "", false)]),
            _ => synthetic_members(&[
                ("name", "string", false),
                ("creationCode", "bytes", false),
                ("runtimeCode", "bytes", false),
                ("interfaceId", "bytes4", false),
            ]),
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
    if ty == "address" || ty == "address payable" {
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
            _ => {}
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
        let recv_name = solsp_hir::resolve::receiver_name(&receiver.first_child()?);
        let member = member_name(receiver)?;
        if matches!(
            (recv_name.as_deref(), member.as_str()),
            (Some("msg"), "sender") | (Some("tx"), "origin") | (Some("block"), "coinbase")
        ) {
            return Some(("address".to_string(), false));
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
            let mdef = solsp_hir::resolve::member_in_type(&tdef, &member, None)?;
            Some(mdef.full_ptr.to_node(&troot))
        }
        _ => None,
    }
}

fn is_integer_type_name(n: &str) -> bool {
    let rest = n.strip_prefix("uint").or_else(|| n.strip_prefix("int"));
    matches!(rest, Some(d) if d.is_empty() || d.parse::<u16>().is_ok())
}

fn is_fixed_bytes(n: &str) -> bool {
    matches!(n.strip_prefix("bytes").map(str::parse::<u8>), Some(Ok(w)) if (1..=32).contains(&w))
}

/// Parse a `USING_DIRECTIVE` into `(library, target)` — `target` is `None` for `for *`.
/// The `using { f, g } for T` form (no single library) is skipped.
fn parse_using(node: &solsp_syntax::SyntaxNode) -> Option<(String, Option<String>)> {
    use solsp_syntax::SyntaxKind::{FOR_KW, IDENT, STAR, USING_KW};
    let toks: Vec<_> = node
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !matches!(t.kind(), solsp_syntax::SyntaxKind::WHITESPACE))
        .collect();
    let using_pos = toks.iter().position(|t| t.kind() == USING_KW)?;
    let lib_tok = toks.get(using_pos + 1)?;
    if lib_tok.kind() != IDENT {
        return None; // `using { … } for T`
    }
    let for_pos = toks.iter().position(|t| t.kind() == FOR_KW)?;
    let target_tok = toks.get(for_pos + 1)?;
    let target = match target_tok.kind() {
        STAR => None,
        IDENT => Some(target_tok.text().to_string()),
        _ => return None,
    };
    Some((lib_tok.text().to_string(), target))
}

/// The `using L for T` directives in scope at `node`: the enclosing contract's and the
/// file's.
fn using_directives(node: &solsp_syntax::SyntaxNode) -> Vec<(String, Option<String>)> {
    use solsp_syntax::SyntaxKind::{CONTRACT_BODY, SOURCE_FILE, USING_DIRECTIVE};
    node.ancestors()
        .filter(|n| matches!(n.kind(), CONTRACT_BODY | SOURCE_FILE))
        .flat_map(|n| n.children())
        .filter(|c| c.kind() == USING_DIRECTIVE)
        .filter_map(|c| parse_using(&c))
        .collect()
}

/// The type name of a receiver value: a user type's name, or an elementary type's text.
fn receiver_type_name(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<String> {
    if let Some((_, tdef)) = resolve_receiver_type(state, uri, root, receiver) {
        return solsp_hir::resolve::contract_def_name(&tdef);
    }
    receiver_value_info(state, uri, root, receiver).map(|(t, _)| t)
}

/// Resolve `value.member` through a `using L for T` directive: the library function
/// (the receiver is its implicit first argument). `None` if no directive attaches it.
fn using_member(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
    member: &str,
    arity: Option<usize>,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    let type_name = receiver_type_name(state, uri, root, receiver)?;
    for (lib, target) in using_directives(receiver) {
        if target.as_deref().is_none_or(|t| t == type_name) {
            if let Some((luri, lnode)) = resolve_type_by_name(state, uri, root, &lib, None) {
                // the call's args plus the implicit receiver argument.
                let def = solsp_hir::resolve::member_in_type(&lnode, member, arity.map(|a| a + 1))
                    .or_else(|| solsp_hir::resolve::member_in_type(&lnode, member, None));
                if let Some(def) = def {
                    return Some((luri, def));
                }
            }
        }
    }
    None
}

/// Completion items for the library functions a `using L for T` directive attaches to the
/// receiver's type.
fn using_member_items(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    receiver: &solsp_syntax::SyntaxNode,
) -> Vec<CompletionItem> {
    let Some(type_name) = receiver_type_name(state, uri, root, receiver) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (lib, target) in using_directives(receiver) {
        if target.as_deref().is_none_or(|t| t == type_name) {
            if let Some((luri, lnode)) = resolve_type_by_name(state, uri, root, &lib, None) {
                let Some(lroot) = parse_root(state, &luri) else {
                    continue;
                };
                let funcs: Vec<_> = solsp_hir::resolve::type_members(&lnode)
                    .into_iter()
                    .filter(|d| {
                        d.kind == solsp_hir::resolve::DefKind::Function
                            && !solsp_hir::resolve::is_private(&d.full_ptr.to_node(&lroot))
                    })
                    .collect();
                out.extend(completion_items_from(funcs));
            }
        }
    }
    out
}

/// Whether a `CONTRACT_DEF` node is a `library`.
fn is_library_node(c: &solsp_syntax::SyntaxNode) -> bool {
    c.children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == solsp_syntax::SyntaxKind::LIBRARY_KW)
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

/// Members of a builtin global object when the receiver is `block`/`tx`/`msg`/`abi`.
fn builtin_member_items(receiver: &solsp_syntax::SyntaxNode) -> Option<Vec<CompletionItem>> {
    use solsp_syntax::SyntaxKind::{NAME_REF, PATH_EXPR};
    if !matches!(receiver.kind(), PATH_EXPR | NAME_REF) {
        return None; // only a bare global, not a chain/call
    }
    let name = solsp_hir::resolve::receiver_name(receiver)?;
    let (members, kind): (&[&str], CompletionItemKind) = match name.as_str() {
        "block" => (
            &[
                "basefee",
                "blobbasefee",
                "chainid",
                "coinbase",
                "difficulty",
                "gaslimit",
                "number",
                "prevrandao",
                "timestamp",
            ],
            CompletionItemKind::FIELD,
        ),
        "tx" => (&["gasprice", "origin"], CompletionItemKind::FIELD),
        "msg" => (
            &["data", "sender", "sig", "value"],
            CompletionItemKind::FIELD,
        ),
        "abi" => (
            &[
                "decode",
                "encode",
                "encodeCall",
                "encodePacked",
                "encodeWithSelector",
                "encodeWithSignature",
            ],
            CompletionItemKind::FUNCTION,
        ),
        _ => return None,
    };
    Some(
        members
            .iter()
            .map(|&m| CompletionItem {
                kind: Some(kind),
                detail: Some("builtin".to_string()),
                label: m.to_string(),
                ..Default::default()
            })
            .collect(),
    )
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

/// A completion item for each `import * as N` namespace alias.
fn namespace_alias_items(root: &solsp_syntax::SyntaxNode) -> Vec<CompletionItem> {
    use solsp_hir::imports::ImportKind;
    solsp_hir::imports::imports(root)
        .into_iter()
        .filter_map(|imp| match imp.kind {
            ImportKind::Namespace(alias) => Some(CompletionItem {
                kind: Some(CompletionItemKind::MODULE),
                detail: Some("import namespace".to_string()),
                label: alias,
                ..Default::default()
            }),
            _ => None,
        })
        .collect()
}

/// Solidity keywords, elementary types, and global builtins — always available as
/// bare-identifier completions.
fn builtin_items() -> Vec<CompletionItem> {
    use CompletionItemKind as K;
    const KEYWORDS: &[&str] = &[
        "if",
        "else",
        "for",
        "while",
        "do",
        "return",
        "break",
        "continue",
        "emit",
        "try",
        "catch",
        "new",
        "delete",
        "using",
        "unchecked",
        "assembly",
        "is",
        "virtual",
        "override",
        "public",
        "private",
        "internal",
        "external",
        "view",
        "pure",
        "payable",
        "memory",
        "storage",
        "calldata",
        "constant",
        "immutable",
        "returns",
        "function",
        "modifier",
        "struct",
        "enum",
        "event",
        "error",
        "mapping",
        "contract",
        "interface",
        "library",
        "import",
        "pragma",
        "abstract",
        "indexed",
        "anonymous",
    ];
    const TYPES: &[&str] = &[
        "address", "bool", "string", "bytes", "uint", "uint8", "uint16", "uint32", "uint64",
        "uint128", "uint256", "int", "int128", "int256", "bytes1", "bytes4", "bytes20", "bytes32",
    ];
    const GLOBALS: &[&str] = &["msg", "block", "tx", "abi", "this", "super", "type", "now"];
    const FUNCS: &[&str] = &[
        "require",
        "assert",
        "revert",
        "keccak256",
        "sha256",
        "ripemd160",
        "ecrecover",
        "addmod",
        "mulmod",
        "selfdestruct",
        "blockhash",
        "gasleft",
    ];
    let item = |label: &str, kind: CompletionItemKind, detail: &str| CompletionItem {
        kind: Some(kind),
        detail: Some(detail.to_string()),
        label: label.to_string(),
        ..Default::default()
    };
    // a builtin function inserts `name()` with the cursor between the parens, and asks the
    // client to pop signature help there (the `(` is inserted, not typed, so it triggers nothing).
    let func = |label: &str| CompletionItem {
        insert_text: Some(format!("{label}($0)")),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        command: Some(trigger_signature_help()),
        ..item(label, K::FUNCTION, "builtin")
    };
    let mut out = Vec::with_capacity(KEYWORDS.len() + TYPES.len() + GLOBALS.len() + FUNCS.len());
    out.extend(KEYWORDS.iter().map(|&k| item(k, K::KEYWORD, "keyword")));
    out.extend(TYPES.iter().map(|&t| item(t, K::TYPE_PARAMETER, "type")));
    out.extend(GLOBALS.iter().map(|&g| item(g, K::VARIABLE, "builtin")));
    out.extend(FUNCS.iter().map(|&f| func(f)));
    out
}

/// Every symbol the file's imports bring into scope (so `new Roles(` offers `Roles`):
/// named imports under their local name, and glob imports' transitively re-exported
/// top-level declarations.
fn imported_symbols(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
) -> Vec<solsp_hir::resolve::Definition> {
    use solsp_hir::imports::ImportKind;
    let mut out = Vec::new();
    let mut visited = std::collections::HashSet::new();
    for imp in solsp_hir::imports::imports(root) {
        let Some(turi) = state::resolve_import_uri(uri, &imp.path) else {
            continue;
        };
        let Some(tfile) = state.file(&turi) else {
            continue;
        };
        let troot = solsp_base_db::parse(state.db(), tfile).syntax();
        match &imp.kind {
            ImportKind::Named(list) => {
                for n in list {
                    if let Some((_, mut def)) =
                        solsp_hir::resolve::top_level_definition(&troot, &n.name, None)
                            .map(|d| (turi.clone(), d))
                            .or_else(|| cross_file_definition(state, &turi, &troot, &n.name, None))
                    {
                        def.name = n.local().to_string(); // the label is the local alias
                        out.push(def);
                    }
                }
            }
            ImportKind::Glob => collect_file_exports(state, &turi, &troot, &mut visited, &mut out),
            // `* as N` — `N.member` is member completion, not a bare name.
            ImportKind::Namespace(_) => {}
        }
    }
    out
}

/// Collect a file's top-level declarations plus everything it re-exports transitively
/// (a glob import re-exports its own imports). Cycle-safe via `visited`.
fn collect_file_exports(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    visited: &mut std::collections::HashSet<Url>,
    out: &mut Vec<solsp_hir::resolve::Definition>,
) {
    use solsp_hir::imports::ImportKind;
    if !visited.insert(uri.clone()) {
        return;
    }
    out.extend(solsp_hir::resolve::file_definitions(root));
    for imp in solsp_hir::imports::imports(root) {
        let Some(turi) = state::resolve_import_uri(uri, &imp.path) else {
            continue;
        };
        let Some(tfile) = state.file(&turi) else {
            continue;
        };
        let troot = solsp_base_db::parse(state.db(), tfile).syntax();
        match &imp.kind {
            ImportKind::Glob => collect_file_exports(state, &turi, &troot, visited, out),
            ImportKind::Named(list) => {
                for n in list {
                    if let Some((_, mut def)) =
                        solsp_hir::resolve::top_level_definition(&troot, &n.name, None)
                            .map(|d| (turi.clone(), d))
                            .or_else(|| cross_file_definition(state, &turi, &troot, &n.name, None))
                    {
                        def.name = n.local().to_string();
                        out.push(def);
                    }
                }
            }
            ImportKind::Namespace(_) => {}
        }
    }
}

/// Completion for the key side of a named-argument list (`f({ <here>: … })`): the
/// parameter names of the callee function, the field names of a struct, or a contract's
/// constructor parameters. `None` when not at a named-argument key.
fn named_arg_completion(
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
    let droot = parse_root(state, &def_uri)?;
    let fields = named_arg_fields(def.kind, &def.full_ptr.to_node(&droot));
    // drop keys already supplied in this argument list (the direct NAME children).
    let present: std::collections::HashSet<String> = nal
        .children()
        .filter(|n| n.kind() == NAME)
        .filter_map(|n| node_ident(&n))
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
fn named_arg_hover(
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
    let key = node_ident(&name_node)?;
    let (def_uri, def) = named_arg_target(state, uri, root, &nal)?;
    let droot = parse_root(state, &def_uri)?;
    let (_, ty) = named_arg_fields(def.kind, &def.full_ptr.to_node(&droot))
        .into_iter()
        .find(|(n, _)| n == &key)?;
    Some(markup_hover(format!("```solidity\n{ty} {key}\n```"), None))
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

/// Resolve the callee of the call whose named-argument list is `nal` to its declaration.
fn named_arg_target(
    state: &ServerState,
    uri: &Url,
    root: &solsp_syntax::SyntaxNode,
    nal: &solsp_syntax::SyntaxNode,
) -> Option<(Url, solsp_hir::resolve::Definition)> {
    let callee = nal.parent()?.first_child()?;
    resolve_named_callee(state, uri, root, &callee)
}

/// The `(name, type)` of each named argument a callee accepts: a function/constructor's
/// parameters, or a struct's fields.
fn named_arg_fields(
    kind: solsp_hir::resolve::DefKind,
    node: &solsp_syntax::SyntaxNode,
) -> Vec<(String, String)> {
    use solsp_hir::resolve::DefKind::*;
    use solsp_syntax::SyntaxKind::{CONSTRUCTOR_DEF, STRUCT_FIELD};
    match kind {
        Function | Modifier | Event | Error => param_name_types(node),
        Struct => node
            .descendants()
            .filter(|n| n.kind() == STRUCT_FIELD)
            .filter_map(|f| named_type(&f))
            .collect(),
        Contract => node
            .descendants()
            .find(|n| n.kind() == CONSTRUCTOR_DEF)
            .map(|c| param_name_types(&c))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The identifier text of a `NAME`/`NAME_REF` node.
fn node_ident(n: &solsp_syntax::SyntaxNode) -> Option<String> {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == solsp_syntax::SyntaxKind::IDENT)
        .map(|t| t.text().to_string())
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

/// The `(name, type)` of each parameter of a function/constructor (its first
/// `PARAM_LIST`).
fn param_name_types(decl: &solsp_syntax::SyntaxNode) -> Vec<(String, String)> {
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
fn named_type(decl: &solsp_syntax::SyntaxNode) -> Option<(String, String)> {
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
fn type_text(decl: &solsp_syntax::SyntaxNode) -> Option<String> {
    let ty = decl
        .children()
        .find(|n| n.kind() != solsp_syntax::SyntaxKind::NAME)?;
    Some(node_type_text(&ty))
}

/// The element/value type text of an array or mapping declaration (`T[]` → `T`,
/// `mapping(K => V)` → `V`, including when `V` is itself a mapping). `None` for other types.
fn indexed_type_text(decl: &solsp_syntax::SyntaxNode) -> Option<String> {
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
fn node_type_text(ty: &solsp_syntax::SyntaxNode) -> String {
    let text: String = ty
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() != solsp_syntax::SyntaxKind::COMMENT)
        .map(|t| t.text().to_string())
        .collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
    queue.push_back((uri.clone(), root.clone(), contract.clone(), false));
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

/// Build completion items from definitions, keeping the first of each name (inner scopes
/// come first, so a local shadows an inherited member of the same name).
fn completion_items_from(defs: Vec<solsp_hir::resolve::Definition>) -> Vec<CompletionItem> {
    let mut seen = std::collections::HashSet::new();
    defs.into_iter()
        .filter(|d| seen.insert(d.name.clone()))
        .map(|d| {
            let (insert_text, insert_text_format) = callable_snippet(&d.name, d.kind);
            // a callable inserts `name()`; ask the client to pop signature help inside the parens.
            let command = insert_text_format.map(|_| trigger_signature_help());
            CompletionItem {
                kind: Some(completion_kind(d.kind)),
                // a value member shows its declared type; everything else its kind label.
                detail: Some(
                    d.ty.clone()
                        .unwrap_or_else(|| def_detail(d.kind).to_string()),
                ),
                insert_text,
                insert_text_format,
                command,
                label: d.name,
                ..Default::default()
            }
        })
        .collect()
}

/// For a callable (function/modifier/event/error), a snippet inserting `name()` with the
/// cursor between the parentheses; `(None, None)` otherwise.
fn callable_snippet(
    name: &str,
    kind: solsp_hir::resolve::DefKind,
) -> (Option<String>, Option<InsertTextFormat>) {
    use solsp_hir::resolve::DefKind::*;
    if matches!(kind, Function | Modifier | Event | Error) {
        (Some(format!("{name}($0)")), Some(InsertTextFormat::SNIPPET))
    } else {
        (None, None)
    }
}

/// A client command that re-opens signature help after a callable snippet is inserted. The
/// snippet writes the `(` itself, so the `(` signature-help trigger character never fires;
/// this nudges the client to request signature help with the cursor sitting inside the parens.
fn trigger_signature_help() -> Command {
    Command {
        title: "Signature help".to_string(),
        command: "editor.action.triggerParameterHints".to_string(),
        arguments: None,
    }
}

fn completion_kind(k: solsp_hir::resolve::DefKind) -> CompletionItemKind {
    use solsp_hir::resolve::DefKind::*;
    match k {
        Function => CompletionItemKind::FUNCTION,
        Modifier => CompletionItemKind::FUNCTION,
        StateVariable | Local | Parameter => CompletionItemKind::VARIABLE,
        Field => CompletionItemKind::FIELD,
        Variant => CompletionItemKind::ENUM_MEMBER,
        Contract => CompletionItemKind::CLASS,
        Interface => CompletionItemKind::INTERFACE,
        Library => CompletionItemKind::MODULE,
        Struct => CompletionItemKind::STRUCT,
        Enum => CompletionItemKind::ENUM,
        Event => CompletionItemKind::EVENT,
        Error => CompletionItemKind::CONSTRUCTOR,
        UserType => CompletionItemKind::TYPE_PARAMETER,
    }
}

fn def_detail(k: solsp_hir::resolve::DefKind) -> &'static str {
    use solsp_hir::resolve::DefKind::*;
    match k {
        Function => "function",
        Modifier => "modifier",
        StateVariable => "state variable",
        Local => "local",
        Parameter => "parameter",
        Field => "field",
        Variant => "enum variant",
        Contract => "contract",
        Interface => "interface",
        Library => "library",
        Struct => "struct",
        Enum => "enum",
        Event => "event",
        Error => "error",
        UserType => "type",
    }
}

/// Latency cap for one type-check pass; calls past it are left unchecked rather than
/// blocking the main loop on slow cross-file resolution.
const TYPE_CHECK_BUDGET_MS: u64 = 750;

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
) -> Vec<lsp_types::Diagnostic> {
    use solsp_syntax::SyntaxKind::{ARG_LIST, CALL_EXPR, NAMED_ARG_LIST};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::Instant;
    let mut out = Vec::new();
    // per-run caches: the same callee text resolves to the same overloads, and the same
    // (subtype, base) pair has a stable answer. Without this, a big forge-std-heavy test
    // file re-walked huge cheatcode files once per call and took tens of seconds.
    let mut callee_cache: HashMap<String, Option<Overloads>> = HashMap::new();
    let subtype_memo: RefCell<HashMap<(String, String), bool>> = RefCell::new(HashMap::new());
    // a hard latency cap: cross-file resolution of dependency-heavy files (forge-std) can
    // be slow, and this runs on the main loop — better to check fewer calls than to block.
    let deadline = Instant::now() + std::time::Duration::from_millis(TYPE_CHECK_BUDGET_MS);

    for call in root.descendants().filter(|n| n.kind() == CALL_EXPR) {
        if Instant::now() >= deadline {
            break;
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
            continue; // no overload of this arity — an arity issue, not a type one
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
            match tok.map(|t| t.kind()) {
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
        if let Some(def) = solsp_hir::resolve::member_in_type(&type_def, &member, arity) {
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
            let mdef = solsp_hir::resolve::member_in_type(&tdef, &member, None)?;
            member_value_type(state, &turi, &troot, &mdef, element)
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
            // same-file C3 first, then cross-file inheritance.
            if let Some(mdef) = solsp_hir::resolve::member_in_type(&tdef, &member, arity) {
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
    use std::collections::{HashSet, VecDeque};
    let mut visited: HashSet<(Url, String)> = HashSet::new();
    // (uri, root, contract, is_base) — a base's `private` member is not accessible here.
    let mut queue: VecDeque<(
        Url,
        solsp_syntax::SyntaxNode,
        solsp_syntax::SyntaxNode,
        bool,
    )> = VecDeque::new();
    queue.push_back((uri.clone(), root.clone(), contract.clone(), false));
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
        let member = solsp_hir::resolve::member_in_type(&type_def, seg, None)?;
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
    // 1. a contract-nested type visible where the name is used (its enclosing contract +
    //    cross-file bases) — these shadow file scope.
    if let Some(contract) = context.and_then(enclosing_contract) {
        if let Some(def) = solsp_hir::resolve::member_in_type(&contract, type_name, None) {
            if is_type_kind(def.kind) {
                return Some((uri.clone(), def.full_ptr.to_node(root)));
            }
        }
        if let Some((turi, def)) = inherited_member(state, uri, root, &contract, type_name, None) {
            if is_type_kind(def.kind) {
                let troot = parse_root(state, &turi)?;
                return Some((turi, def.full_ptr.to_node(&troot)));
            }
        }
    }
    // 2. a top-level type in this file.
    if let Some(def) = solsp_hir::resolve::top_level_definition(root, type_name, None) {
        if is_type_kind(def.kind) {
            return Some((uri.clone(), def.full_ptr.to_node(root)));
        }
    }
    // 3. an imported type.
    let (turi, def) = cross_file_definition(state, uri, root, type_name, None)?;
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
        // a top-level declaration in the imported file…
        if let Some(def) = solsp_hir::resolve::top_level_definition(&troot, &export, arity) {
            return Some((target_uri, def));
        }
        // …or one the imported file itself re-exports (transitively).
        if let Some(found) = cross_file_rec(state, &target_uri, &troot, &export, arity, visited) {
            return Some(found);
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
            publish_diagnostics(connection, state, &uri, true)?;
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
            publish_diagnostics(connection, state, &uri, false)?;
        }
        DidSaveTextDocument::METHOD => {
            let Some(params) = extract_notification::<DidSaveTextDocument>(not) else {
                return Ok(());
            };
            publish_diagnostics(connection, state, &params.text_document.uri, true)?;
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

/// Compute and publish diagnostics for an open document (empty list if missing). The
/// semantic type-check (slow, cross-file) runs only when `semantic` is set — on open/save,
/// not on every keystroke — so typing stays responsive.
fn publish_diagnostics(
    connection: &Connection,
    state: &ServerState,
    uri: &Url,
    semantic: bool,
) -> Result<()> {
    let diagnostics = match (state.file(uri), state.line_index(uri)) {
        (Some(file), Some(li)) => {
            let parse = solsp_base_db::parse(state.db(), file);
            let mut diags =
                to_proto::diagnostics(&solsp_ide::diagnostics::diagnostics(parse.errors()), li);
            // type-check only a syntactically clean file (a broken tree mid-edit is noise).
            if semantic && parse.errors().is_empty() {
                diags.extend(type_check_diagnostics(state, uri, &parse.syntax(), li));
            }
            diags
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
