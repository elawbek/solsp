//! End-to-end LSP session over an in-memory transport (design §6, level 3). Drives a
//! real `initialize → didOpen → documentSymbol → semanticTokens → didChange →
//! shutdown` exchange against [`solsp_server::run`] and asserts on the wire replies —
//! no editor, no stdio, no name resolution.

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DocumentSymbolParams,
    DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
    HoverParams, InitializeParams, InitializeResult, Position, PublishDiagnosticsParams,
    SemanticTokensParams, SemanticTokensResult, SymbolKind, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Url,
    VersionedTextDocumentIdentifier,
};
use std::thread;

/// Block until the next `Response`, skipping interleaved notifications.
fn next_response(client: &Connection) -> Response {
    loop {
        match client.receiver.recv().expect("server closed early") {
            Message::Response(r) => return r,
            Message::Notification(_) => continue,
            Message::Request(r) => panic!("unexpected server→client request: {r:?}"),
        }
    }
}

/// Block until the next notification with `method`, skipping anything else.
fn next_notification(client: &Connection, method: &str) -> Notification {
    loop {
        match client.receiver.recv().expect("server closed early") {
            Message::Notification(n) if n.method == method => return n,
            Message::Notification(_) | Message::Response(_) => continue,
            Message::Request(r) => panic!("unexpected server→client request: {r:?}"),
        }
    }
}

fn send_request(client: &Connection, id: i32, method: &str, params: impl serde::Serialize) {
    let req = Request::new(RequestId::from(id), method.to_string(), params);
    client.sender.send(Message::Request(req)).unwrap();
}

fn send_notification(client: &Connection, method: &str, params: impl serde::Serialize) {
    let not = Notification::new(method.to_string(), params);
    client.sender.send(Message::Notification(not)).unwrap();
}

fn open_params(uri: &Url, text: &str) -> DidOpenTextDocumentParams {
    DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "solidity".to_string(),
            version: 0,
            text: text.to_string(),
        },
    }
}

fn change_params(uri: &Url, version: i32, text: &str) -> DidChangeTextDocumentParams {
    DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier {
            uri: uri.clone(),
            version,
        },
        content_changes: vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }],
    }
}

fn doc_id(uri: &Url) -> TextDocumentIdentifier {
    TextDocumentIdentifier { uri: uri.clone() }
}

#[test]
fn full_lsp_session() {
    let (server, client) = Connection::memory();

    // The server: complete the handshake with our real capabilities, then run.
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });

    // 1. initialize ----------------------------------------------------------
    send_request(&client, 1, "initialize", InitializeParams::default());
    let resp = next_response(&client);
    assert_eq!(resp.id, RequestId::from(1));
    let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(init.capabilities.document_symbol_provider.is_some());
    assert!(init.capabilities.semantic_tokens_provider.is_some());
    send_notification(&client, "initialized", lsp_types::InitializedParams {});

    let uri = Url::parse("file:///Vault.sol").unwrap();

    // 2. didOpen a clean contract → diagnostics should be empty ---------------
    let src = "contract C {\n    function f() public {}\n}\n";
    send_notification(&client, "textDocument/didOpen", open_params(&uri, src));
    let diag_note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(diag_note.params).unwrap();
    assert_eq!(diags.uri, uri);
    assert!(
        diags.diagnostics.is_empty(),
        "clean parse should publish no diagnostics, got {:?}",
        diags.diagnostics
    );

    // 3. documentSymbol → outline `C { f }` ----------------------------------
    send_request(
        &client,
        2,
        "textDocument/documentSymbol",
        DocumentSymbolParams {
            text_document: doc_id(&uri),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let resp = next_response(&client);
    assert_eq!(resp.id, RequestId::from(2));
    let symbols: DocumentSymbolResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
    let DocumentSymbolResponse::Nested(syms) = symbols else {
        panic!("expected nested document symbols");
    };
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].name, "C");
    assert_eq!(syms[0].kind, SymbolKind::CLASS);
    let children = syms[0].children.as_ref().expect("contract has members");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].name, "f");
    assert_eq!(children[0].kind, SymbolKind::FUNCTION);
    // The selection range is the bare identifier `f` on line 1.
    assert_eq!(children[0].selection_range.start.line, 1);

    // 4. semanticTokens/full → non-empty delta stream ------------------------
    send_request(
        &client,
        3,
        "textDocument/semanticTokens/full",
        SemanticTokensParams {
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            text_document: doc_id(&uri),
        },
    );
    let resp = next_response(&client);
    assert_eq!(resp.id, RequestId::from(3));
    let tokens: SemanticTokensResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    let SemanticTokensResult::Tokens(tokens) = tokens else {
        panic!("expected full token set");
    };
    assert!(!tokens.data.is_empty(), "expected classified tokens");
    // The very first token is the `contract` keyword at line 0, char 0.
    assert_eq!(tokens.data[0].delta_line, 0);
    assert_eq!(tokens.data[0].delta_start, 0);
    assert_eq!(tokens.data[0].length, "contract".len() as u32);

    // 5. didChange to broken source → diagnostics now non-empty --------------
    send_notification(
        &client,
        "textDocument/didChange",
        change_params(&uri, 1, "@@@ contract C {"),
    );
    let diag_note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(diag_note.params).unwrap();
    assert!(
        !diags.diagnostics.is_empty(),
        "broken source should publish diagnostics"
    );

    // 6. shutdown / exit -----------------------------------------------------
    send_request(&client, 4, "shutdown", serde_json::Value::Null);
    let resp = next_response(&client);
    assert_eq!(resp.id, RequestId::from(4));
    send_notification(&client, "exit", serde_json::Value::Null);

    server_thread.join().expect("server thread panicked");
}

#[test]
fn goto_definition_and_hover() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });

    send_request(&client, 1, "initialize", InitializeParams::default());
    let resp = next_response(&client);
    let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(init.capabilities.definition_provider.is_some());
    assert!(init.capabilities.hover_provider.is_some());
    send_notification(&client, "initialized", lsp_types::InitializedParams {});

    // single line ⇒ UTF-16 character == byte offset, so positions come from `find`.
    let uri = Url::parse("file:///C.sol").unwrap();
    let src = "contract C { uint256 s; function f() public { s = 1; } }";
    send_notification(&client, "textDocument/didOpen", open_params(&uri, src));
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    let use_char = src.find("s = 1").unwrap() as u32;
    let decl_char = src.find("s;").unwrap() as u32;
    let cursor = TextDocumentPositionParams {
        text_document: doc_id(&uri),
        position: Position {
            line: 0,
            character: use_char,
        },
    };

    // definition: jumps to the state-variable declaration name `s`.
    send_request(
        &client,
        2,
        "textDocument/definition",
        GotoDefinitionParams {
            text_document_position_params: cursor.clone(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let resp = next_response(&client);
    let def: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
    let GotoDefinitionResponse::Scalar(loc) = def else {
        panic!("expected a single definition location");
    };
    assert_eq!(loc.uri, uri);
    assert_eq!(
        loc.range.start,
        Position {
            line: 0,
            character: decl_char
        }
    );
    assert_eq!(
        loc.range.end,
        Position {
            line: 0,
            character: decl_char + 1
        }
    );

    // hover: kind + signature for the same identifier.
    send_request(
        &client,
        3,
        "textDocument/hover",
        HoverParams {
            text_document_position_params: cursor,
            work_done_progress_params: Default::default(),
        },
    );
    let resp = next_response(&client);
    let hover: Hover = serde_json::from_value(resp.result.unwrap()).unwrap();
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup hover");
    };
    assert!(markup.value.contains("(state variable)"));
    assert!(markup.value.contains("`s`"));

    send_request(&client, 4, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

/// A notification with malformed params must be ignored, not crash the main loop:
/// the server has no id to answer, so propagating the error would silently kill it.
#[test]
fn malformed_notification_does_not_kill_server() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });

    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});

    // garbage didOpen params — the server should log-and-skip, staying alive.
    send_notification(
        &client,
        "textDocument/didOpen",
        serde_json::json!({ "bogus": true }),
    );

    // proof of life: a follow-up request still gets a (successful) reply.
    let uri = Url::parse("file:///nope.sol").unwrap();
    send_request(
        &client,
        2,
        "textDocument/documentSymbol",
        DocumentSymbolParams {
            text_document: doc_id(&uri),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let resp = next_response(&client);
    assert_eq!(resp.id, RequestId::from(2));
    assert!(resp.error.is_none(), "server should still answer: {resp:?}");

    send_request(&client, 3, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}
