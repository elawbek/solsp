//! End-to-end LSP session over an in-memory transport (design §6, level 3). Drives a
//! real `initialize → didOpen → documentSymbol → semanticTokens → didChange →
//! shutdown` exchange against [`solsp_server::run`] and asserts on the wire replies —
//! no editor, no stdio, no name resolution.

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CompletionParams, CompletionResponse, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverContents, HoverParams, InitializeParams, InitializeResult, Position,
    PublishDiagnosticsParams, Range, SemanticTokensParams, SemanticTokensResult, SymbolKind,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Url, VersionedTextDocumentIdentifier,
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

fn incremental_change(
    uri: &Url,
    version: i32,
    start: (u32, u32),
    end: (u32, u32),
    text: &str,
) -> DidChangeTextDocumentParams {
    DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier {
            uri: uri.clone(),
            version,
        },
        content_changes: vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: start.0,
                    character: start.1,
                },
                end: Position {
                    line: end.0,
                    character: end.1,
                },
            }),
            range_length: None,
            text: text.to_string(),
        }],
    }
}

#[test]
fn incremental_edit_updates_diagnostics() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });

    send_request(&client, 1, "initialize", InitializeParams::default());
    let resp = next_response(&client);
    // server advertises INCREMENTAL sync
    let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    let sync = serde_json::to_value(init.capabilities.text_document_sync.unwrap()).unwrap();
    assert_eq!(sync, serde_json::json!(2)); // TextDocumentSyncKind::INCREMENTAL
    send_notification(&client, "initialized", lsp_types::InitializedParams {});

    let uri = Url::parse("file:///C.sol").unwrap();
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&uri, "contract C {}"),
    );
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let d: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    assert!(d.diagnostics.is_empty());

    // splice "@@@ " at the very start → now broken
    send_notification(
        &client,
        "textDocument/didChange",
        incremental_change(&uri, 1, (0, 0), (0, 0), "@@@ "),
    );
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let d: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    assert!(!d.diagnostics.is_empty(), "broken after insert");

    // delete the "@@@ " back out → clean again
    send_notification(
        &client,
        "textDocument/didChange",
        incremental_change(&uri, 2, (0, 0), (0, 4), ""),
    );
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let d: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    assert!(d.diagnostics.is_empty(), "clean after delete");

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
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

#[test]
fn cross_file_goto_definition() {
    use std::fs;

    // two real files on disk: Main imports Token and uses it.
    let dir = std::env::temp_dir().join("solsp_xfile_goto");
    fs::create_dir_all(&dir).unwrap();
    let token = dir.join("Token.sol");
    let main = dir.join("Main.sol");
    fs::write(&token, "contract Token { uint256 supply; }\n").unwrap();
    // named import; line 0 = the directive, line 1 = a use site.
    fs::write(
        &main,
        "import {Token} from \"Token.sol\";\ncontract Main { Token t; }\n",
    )
    .unwrap();

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let token_uri = Url::from_file_path(fs::canonicalize(&token).unwrap()).unwrap();
    let main_src = fs::read_to_string(&main).unwrap();
    let line0 = main_src.lines().next().unwrap();
    let line1 = main_src.lines().nth(1).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });

    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});

    // opening Main pulls Token.sol into the db via the import graph.
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&main_uri, &main_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    let definition = |id: i32, line: u32, character: u32| {
        send_request(
            &client,
            id,
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: doc_id(&main_uri),
                    position: Position { line, character },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        );
        let resp = next_response(&client);
        let def: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
        let GotoDefinitionResponse::Scalar(loc) = def else {
            panic!("expected a single definition location");
        };
        loc
    };

    // 1. a use site of the imported `Token` → its declaration in Token.sol.
    let use_ch = line1.find("Token t").unwrap() as u32;
    let loc = definition(2, 1, use_ch + 1);
    assert_eq!(loc.uri, token_uri);
    assert_eq!(
        loc.range.start,
        Position {
            line: 0,
            character: 9
        }
    ); // `contract Token`

    // 2. the name `Token` inside `{ ... }` of the import → same declaration.
    let brace_ch = line0.find("Token}").unwrap() as u32;
    let loc = definition(3, 0, brace_ch + 1);
    assert_eq!(loc.uri, token_uri);
    assert_eq!(
        loc.range.start,
        Position {
            line: 0,
            character: 9
        }
    );

    // 3. the import path string → opens the target file at its start.
    let path_ch = line0.find("Token.sol").unwrap() as u32;
    let loc = definition(4, 0, path_ch + 1);
    assert_eq!(loc.uri, token_uri);
    assert_eq!(
        loc.range.start,
        Position {
            line: 0,
            character: 0
        }
    );

    // 4. the editor opens Token.sol (from the jump) then the user closes it: a
    // didClose must NOT unload it — cross-file resolution must still work afterwards.
    let token_src = fs::read_to_string(&token).unwrap();
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&token_uri, &token_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");
    send_notification(
        &client,
        "textDocument/didClose",
        lsp_types::DidCloseTextDocumentParams {
            text_document: doc_id(&token_uri),
        },
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");
    let loc = definition(5, 1, use_ch + 1);
    assert_eq!(loc.uri, token_uri, "still resolves after target closed");

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn member_access_resolves_cross_file() {
    use std::fs;

    let dir = std::env::temp_dir().join("solsp_member_xfile");
    fs::create_dir_all(&dir).unwrap();
    let other = dir.join("Other.sol");
    let main = dir.join("Main.sol");
    fs::write(
        &other,
        "interface IThing { struct Data { uint256 x; } function ping() external; }\n\
         library Lib { function doThing() internal pure {} }\n",
    )
    .unwrap();
    fs::write(
        &main,
        "import {Lib, IThing} from \"Other.sol\";\n\
         contract Main {\n\
             IThing thing;\n\
             IThing.Data d;\n\
             function f() public { Lib.doThing(); thing.ping(); }\n\
         }\n",
    )
    .unwrap();

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let other_uri = Url::from_file_path(fs::canonicalize(&other).unwrap()).unwrap();
    let main_src = fs::read_to_string(&main).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });
    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&main_uri, &main_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    let typed_line = main_src.lines().nth(3).unwrap(); // `IThing.Data d;`
    let body_line = main_src.lines().nth(4).unwrap(); // the function body
    let definition = |id: i32, line: u32, character: u32| {
        send_request(
            &client,
            id,
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: doc_id(&main_uri),
                    position: Position { line, character },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        );
        let resp = next_response(&client);
        let def: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
        let GotoDefinitionResponse::Scalar(loc) = def else {
            panic!("expected a definition location");
        };
        loc
    };

    // `Lib.doThing()` → the library method in Other.sol (line 1).
    let ch = body_line.find("doThing").unwrap() as u32;
    let loc = definition(2, 4, ch + 1);
    assert_eq!(loc.uri, other_uri);
    assert_eq!(loc.range.start.line, 1);

    // `thing.ping()` → via the state var `IThing thing` → IThing.ping in Other.sol (line 0).
    let ch = body_line.find("ping").unwrap() as u32;
    let loc = definition(3, 4, ch + 1);
    assert_eq!(loc.uri, other_uri);
    assert_eq!(loc.range.start.line, 0);

    // qualified type `IThing.Data` → the struct nested in the interface (Other.sol line 0).
    let ch = typed_line.find("Data").unwrap() as u32;
    let loc = definition(4, 3, ch + 1);
    assert_eq!(loc.uri, other_uri);
    assert_eq!(loc.range.start.line, 0);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn struct_field_via_index_cross_file() {
    use std::fs;

    let dir = std::env::temp_dir().join("solsp_field_index");
    fs::create_dir_all(&dir).unwrap();
    let other = dir.join("Other.sol");
    let main = dir.join("Main.sol");
    fs::write(&other, "interface IT { struct Item { uint256 amount; } }\n").unwrap();
    fs::write(
        &main,
        "import {IT} from \"Other.sol\";\n\
         contract C {\n\
             function f(IT.Item[] calldata items) public {\n\
                 uint256 x = items[0].amount;\n\
             }\n\
         }\n",
    )
    .unwrap();

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let other_uri = Url::from_file_path(fs::canonicalize(&other).unwrap()).unwrap();
    let main_src = fs::read_to_string(&main).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });
    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&main_uri, &main_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    // `items[0].amount` → the struct field `amount` in Other.sol (the array element
    // type IT.Item, resolved cross-file through the qualified path + array stripping).
    let line3 = main_src.lines().nth(3).unwrap();
    let ch = line3.find("amount").unwrap() as u32;
    send_request(
        &client,
        2,
        "textDocument/definition",
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&main_uri),
                position: Position {
                    line: 3,
                    character: ch + 1,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let resp = next_response(&client);
    let def: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
    let GotoDefinitionResponse::Scalar(loc) = def else {
        panic!("expected a field definition location");
    };
    assert_eq!(loc.uri, other_uri);
    assert_eq!(loc.range.start.line, 0); // `amount` is on Other.sol line 0

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn remapped_and_node_modules_imports_resolve() {
    use std::fs;

    let dir = std::env::temp_dir().join("solsp_remap");
    let _ = fs::remove_dir_all(&dir);
    let mk = |rel: &str, content: &str| {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        p
    };
    // project root marker + a remapping; a remapped package and a node_modules package.
    mk("remappings.txt", "@lib/=packages/mylib/\n");
    let thing = mk("packages/mylib/Thing.sol", "contract Thing {}\n");
    let modd = mk("node_modules/pkg/Mod.sol", "contract Mod {}\n");
    let main = mk(
        "src/Main.sol",
        "import {Thing} from \"@lib/Thing.sol\";\n\
         import {Mod} from \"pkg/Mod.sol\";\n\
         contract Main { Thing t; Mod m; }\n",
    );

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let thing_uri = Url::from_file_path(fs::canonicalize(&thing).unwrap()).unwrap();
    let mod_uri = Url::from_file_path(fs::canonicalize(&modd).unwrap()).unwrap();
    let main_src = fs::read_to_string(&main).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });
    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&main_uri, &main_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    let line2 = main_src.lines().nth(2).unwrap(); // `contract Main { Thing t; Mod m; }`
    let definition = |id: i32, character: u32| {
        send_request(
            &client,
            id,
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: doc_id(&main_uri),
                    position: Position { line: 2, character },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        );
        let resp = next_response(&client);
        let def: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
        let GotoDefinitionResponse::Scalar(loc) = def else {
            panic!("expected a definition location");
        };
        loc
    };

    // `Thing` (remapped @lib/) → packages/mylib/Thing.sol
    let ch = line2.find("Thing t").unwrap() as u32;
    assert_eq!(definition(2, ch + 1).uri, thing_uri);
    // `Mod` (node_modules/pkg) → node_modules/pkg/Mod.sol
    let ch = line2.find("Mod m").unwrap() as u32;
    assert_eq!(definition(3, ch + 1).uri, mod_uri);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn overload_resolution_picks_matching_arity() {
    // a library with two `f` overloads, called via `Lib.f(...)` with 1 vs 2 args.
    let uri = Url::parse("file:///O.sol").unwrap();
    let src =
        "library Lib { function f(uint a) internal {} function f(uint a, uint b) internal {} }\n\
               contract C { function g() public { Lib.f(1); Lib.f(1, 2); } }\n";

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });
    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});
    send_notification(&client, "textDocument/didOpen", open_params(&uri, src));
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    let line1 = src.lines().nth(1).unwrap();
    let def = |id: i32, character: u32| {
        send_request(
            &client,
            id,
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: doc_id(&uri),
                    position: Position { line: 1, character },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        );
        let resp = next_response(&client);
        let d: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
        let GotoDefinitionResponse::Scalar(loc) = d else {
            panic!("expected a location");
        };
        loc.range.start
    };

    // both overloads are declared on line 0; the 2-arg call must land on the *second*
    // `f` (later column) and the 1-arg call on the first.
    let one = def(2, (line1.find("f(1);").unwrap() + 1) as u32);
    let two = def(3, (line1.find("f(1, 2)").unwrap() + 1) as u32);
    assert_eq!(one.line, 0);
    assert_eq!(two.line, 0);
    assert!(
        two.character > one.character,
        "2-arg overload must be the later `f`"
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn chained_member_access_resolves_through_call_return_type() {
    use std::fs;

    let dir = std::env::temp_dir().join("solsp_chain");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let other = dir.join("Other.sol");
    let main = dir.join("Main.sol");
    fs::write(
        &other,
        "interface IT { struct Data { uint256 amount; } }\n\
         library Lib { function get() internal pure returns (IT.Data storage d) {} }\n",
    )
    .unwrap();
    fs::write(
        &main,
        "import {Lib, IT} from \"Other.sol\";\n\
         contract C { function f() public { uint256 x = Lib.get().amount; } }\n",
    )
    .unwrap();

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let other_uri = Url::from_file_path(fs::canonicalize(&other).unwrap()).unwrap();
    let main_src = fs::read_to_string(&main).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });
    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&main_uri, &main_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    // `Lib.get().amount` — receiver is the call `Lib.get()`; its type is the function's
    // return type `IT.Data`; `amount` is that struct's field in Other.sol.
    let line1 = main_src.lines().nth(1).unwrap();
    let ch = line1.find("amount").unwrap() as u32;
    send_request(
        &client,
        2,
        "textDocument/definition",
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&main_uri),
                position: Position {
                    line: 1,
                    character: ch + 1,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let resp = next_response(&client);
    let def: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
    let GotoDefinitionResponse::Scalar(loc) = def else {
        panic!("expected a definition location");
    };
    assert_eq!(loc.uri, other_uri);
    assert_eq!(loc.range.start.line, 0); // `amount` is on Other.sol line 0

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cross_file_inheritance_resolves_inherited_members() {
    use std::fs;

    let dir = std::env::temp_dir().join("solsp_inherit");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let base = dir.join("Base.sol");
    let main = dir.join("Main.sol");
    fs::write(
        &base,
        "interface IThing { function go() external; }\n\
         contract Base { IThing internal thing; function _helper() internal {} }\n",
    )
    .unwrap();
    fs::write(
        &main,
        "import {Base} from \"Base.sol\";\n\
         contract Derived is Base { function f() public { _helper(); thing.go(); } }\n",
    )
    .unwrap();

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let base_uri = Url::from_file_path(fs::canonicalize(&base).unwrap()).unwrap();
    let main_src = fs::read_to_string(&main).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });
    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&main_uri, &main_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    let line1 = main_src.lines().nth(1).unwrap();
    let def_at = |id: i32, character: u32| -> lsp_types::Location {
        send_request(
            &client,
            id,
            "textDocument/definition",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: doc_id(&main_uri),
                    position: Position { line: 1, character },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        );
        let resp = next_response(&client);
        let d: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
        match d {
            GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("expected a single location"),
        }
    };

    // `_helper()` — a bare name inherited from the cross-file base `Base`.
    let helper = def_at(2, line1.find("_helper").unwrap() as u32 + 1);
    assert_eq!(helper.uri, base_uri);
    assert_eq!(helper.range.start.line, 1); // `_helper` is on Base.sol line 1

    // `thing.go()` — `thing` is an inherited field; `go` is a member of its type IThing.
    let go = def_at(3, line1.find("go()").unwrap() as u32 + 1);
    assert_eq!(go.uri, base_uri);
    assert_eq!(go.range.start.line, 0); // `go` is on Base.sol line 0 (the interface)

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn transitive_reexport_and_root_relative_import() {
    use std::fs;

    // a Foundry-ish layout: foundry.toml marks the root; `contracts/` is the src dir.
    let root = std::env::temp_dir().join("solsp_transitive");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("contracts")).unwrap();
    fs::create_dir_all(root.join("test")).unwrap();
    fs::write(
        root.join("foundry.toml"),
        "[profile.default]\nsrc = 'contracts'\n",
    )
    .unwrap();
    fs::write(root.join("contracts/Roles.sol"), "contract Roles { }\n").unwrap();
    // Base uses a project-root-relative import path (`contracts/...`, not `./`).
    fs::write(
        root.join("test/Base.sol"),
        "import { Roles } from \"contracts/Roles.sol\";\ncontract Base { }\n",
    )
    .unwrap();
    // Main glob-imports Base, which re-exports Roles transitively.
    fs::write(
        root.join("test/Main.sol"),
        "import \"./Base.sol\";\ncontract Main is Base { function f() public { x = new Roles(); } }\n",
    )
    .unwrap();

    let main_uri =
        Url::from_file_path(fs::canonicalize(root.join("test/Main.sol")).unwrap()).unwrap();
    let roles_uri =
        Url::from_file_path(fs::canonicalize(root.join("contracts/Roles.sol")).unwrap()).unwrap();
    let main_src = fs::read_to_string(root.join("test/Main.sol")).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });
    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&main_uri, &main_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    // `new Roles()` — Roles is reached via `./Base.sol` (glob) → Base's named import of
    // `contracts/Roles.sol` (root-relative path).
    let line1 = main_src.lines().nth(1).unwrap();
    let ch = line1.find("Roles").unwrap() as u32;
    send_request(
        &client,
        2,
        "textDocument/definition",
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&main_uri),
                position: Position {
                    line: 1,
                    character: ch + 1,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let resp = next_response(&client);
    let def: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
    let GotoDefinitionResponse::Scalar(loc) = def else {
        panic!("expected a definition location");
    };
    assert_eq!(loc.uri, roles_uri);
    assert_eq!(loc.range.start.line, 0); // `Roles` on Roles.sol line 0

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn completion_member_and_scope_with_inheritance() {
    use std::fs;

    let dir = std::env::temp_dir().join("solsp_completion");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let base = dir.join("Base.sol");
    let main = dir.join("Main.sol");
    fs::write(
        &base,
        "contract Base { uint256 baseVar; function baseFn() internal {} }\n",
    )
    .unwrap();
    // lines are flush-left so column math is simple; `c` is a local of contract type C.
    fs::write(
        &main,
        "import {Base} from \"Base.sol\";\n\
         contract C is Base {\n\
         function f(uint256 p) public {\n\
         C c;\n\
         c.baseVar;\n\
         }\n\
         }\n",
    )
    .unwrap();

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let main_src = fs::read_to_string(&main).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });
    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&main_uri, &main_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    let labels = |id: i32, line: u32, character: u32| -> Vec<String> {
        send_request(
            &client,
            id,
            "textDocument/completion",
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: doc_id(&main_uri),
                    position: Position { line, character },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            },
        );
        let resp = next_response(&client);
        let r: CompletionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
        let items = match r {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        };
        items.into_iter().map(|i| i.label).collect()
    };

    // scope completion inside f's body (line 3, `C c;`) — sees the param, the contract's
    // own + cross-file inherited members.
    let scope = labels(2, 3, 0);
    for want in ["p", "f", "baseFn", "baseVar"] {
        assert!(
            scope.contains(&want.to_string()),
            "scope missing {want}: {scope:?}"
        );
    }

    // member completion after `c.` (line 4) — `c` is of contract type C, so its members
    // incl. the inherited `baseFn`/`baseVar`.
    let member = labels(3, 4, 2);
    assert!(
        member.contains(&"baseFn".to_string()),
        "member missing baseFn: {member:?}"
    );
    assert!(member.contains(&"baseVar".to_string()));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn completion_imported_symbols_and_named_args() {
    use std::fs;

    let dir = std::env::temp_dir().join("solsp_comp_imports");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("Lib.sol"),
        "struct Point { uint256 x; uint256 y; }\n\
         contract Maker { constructor(address owner_) {} }\n",
    )
    .unwrap();
    let main = dir.join("Main.sol");
    // flush-left lines for simple column math.
    fs::write(
        &main,
        "import {Point, Maker} from \"Lib.sol\";\n\
         import * as Lib from \"Lib.sol\";\n\
         contract C {\n\
         function f() public {\n\
         Maker m = new Maker({ });\n\
         Point p = Point({ });\n\
         Point q = Point({ x: 1, });\n\
         }\n\
         }\n",
    )
    .unwrap();

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let main_src = fs::read_to_string(&main).unwrap();

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });
    send_request(&client, 1, "initialize", InitializeParams::default());
    let _ = next_response(&client);
    send_notification(&client, "initialized", lsp_types::InitializedParams {});
    send_notification(
        &client,
        "textDocument/didOpen",
        open_params(&main_uri, &main_src),
    );
    let _ = next_notification(&client, "textDocument/publishDiagnostics");

    let labels = |id: i32, line: u32, character: u32| -> Vec<String> {
        send_request(
            &client,
            id,
            "textDocument/completion",
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: doc_id(&main_uri),
                    position: Position { line, character },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            },
        );
        let resp = next_response(&client);
        let r: CompletionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
        match r {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        }
        .into_iter()
        .map(|i| i.label)
        .collect()
    };

    // scope completion offers imported symbols, the namespace alias, and builtins.
    let scope = labels(2, 4, 0);
    for want in ["Point", "Maker", "Lib", "require", "address", "msg"] {
        assert!(
            scope.contains(&want.to_string()),
            "scope missing {want}: {scope:?}"
        );
    }

    // `new Maker({ <here> })` → the constructor's parameter.
    let l4 = main_src.lines().nth(4).unwrap();
    let maker = labels(3, 4, l4.find("({").unwrap() as u32 + 2);
    assert_eq!(maker, ["owner_"]);

    // `Point({ <here> })` → the struct's fields.
    let l5 = main_src.lines().nth(5).unwrap();
    let point = labels(4, 5, l5.find("({").unwrap() as u32 + 2);
    assert!(
        point.contains(&"x".to_string()) && point.contains(&"y".to_string()),
        "{point:?}"
    );

    // `Point({ x: 1, <here> })` — the already-supplied key `x` is filtered out.
    let l6 = main_src.lines().nth(6).unwrap();
    let rest = labels(5, 6, l6.find("})").unwrap() as u32);
    assert_eq!(rest, ["y"], "should offer only the unsupplied field");

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
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
