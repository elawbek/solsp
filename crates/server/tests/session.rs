//! End-to-end LSP session over an in-memory transport (design §6, level 3). Drives a
//! real `initialize → didOpen → documentSymbol → semanticTokens → didChange →
//! shutdown` exchange against [`solsp_server::run`] and asserts on the wire replies —
//! no editor, no stdio, no name resolution.

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CompletionItem, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidOpenTextDocumentParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, InitializeParams, InitializeResult,
    Position, PublishDiagnosticsParams, Range, SemanticTokensParams, SemanticTokensResult,
    SignatureHelp, SignatureHelpParams, SymbolKind, TextDocumentContentChangeEvent,
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
    // server advertises INCREMENTAL sync plus save notifications
    let init: InitializeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    let sync = serde_json::to_value(init.capabilities.text_document_sync.unwrap()).unwrap();
    assert_eq!(sync["change"], serde_json::json!(2)); // TextDocumentSyncKind::INCREMENTAL
    assert_eq!(sync["save"], serde_json::json!(true));
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
        "contract Base { uint256 baseVar; function baseFn() internal {} \
         function basePub() public {} }\n",
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

    // member completion after `c.` (line 4) — `c` is a contract *instance*, so external
    // access shows the inherited public `basePub` but NOT the internal `baseFn`/`baseVar`.
    let member = labels(3, 4, 2);
    assert!(
        member.contains(&"basePub".to_string()),
        "member missing basePub: {member:?}"
    );
    assert!(
        !member.contains(&"baseFn".to_string()),
        "internal leaked: {member:?}"
    );
    assert!(
        !member.contains(&"baseVar".to_string()),
        "internal var leaked: {member:?}"
    );

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

#[test]
fn using_for_library_method() {
    let uri = Url::parse("file:///uf.sol").unwrap();
    let src = "library Str { function toString(uint256 v) internal pure returns (string memory) {} \
               function half(uint256 v) internal pure returns (uint256) {} }\n\
               contract C { using Str for uint256; function f() public { uint256 x; x.toString(); x.M; } }";

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
    // go-to-def on `x.toString()` lands on the library function (line 0).
    let ch = line1.find("x.toString").unwrap() as u32 + 2;
    send_request(
        &client,
        2,
        "textDocument/definition",
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 1,
                    character: ch,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let d: GotoDefinitionResponse =
        serde_json::from_value(next_response(&client).result.unwrap()).unwrap();
    let GotoDefinitionResponse::Scalar(loc) = d else {
        panic!("expected a location")
    };
    assert_eq!(loc.range.start.line, 0);

    // completion on `x.` lists the attached library functions.
    let mch = line1.find("x.M").unwrap() as u32 + 2;
    send_request(
        &client,
        3,
        "textDocument/completion",
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 1,
                    character: mch,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        },
    );
    let r: CompletionResponse =
        serde_json::from_value(next_response(&client).result.unwrap()).unwrap();
    let labels: Vec<String> = match r {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    }
    .into_iter()
    .map(|i| i.label)
    .collect();
    assert!(
        labels.contains(&"toString".to_string()) && labels.contains(&"half".to_string()),
        "{labels:?}"
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn this_and_super_member_completion() {
    let uri = Url::parse("file:///ts.sol").unwrap();
    let src = "contract Base { function pub_() public {} function int_() internal {} }\n\
               contract C is Base { function f() public { this.X1; super.X2; } }";

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
    let labels = |id: i32, marker: &str| -> Vec<String> {
        let ch = line1.find(marker).unwrap() as u32;
        send_request(
            &client,
            id,
            "textDocument/completion",
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: doc_id(&uri),
                    position: Position {
                        line: 1,
                        character: ch,
                    },
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
    // `this.` is external access → only public members.
    let this_m = labels(2, "X1");
    assert!(this_m.contains(&"pub_".to_string()), "{this_m:?}");
    assert!(
        !this_m.contains(&"int_".to_string()),
        "this. leaked internal: {this_m:?}"
    );
    // `super.` is internal access → inherited public + internal.
    let super_m = labels(3, "X2");
    assert!(
        super_m.contains(&"int_".to_string()),
        "super. should see internal: {super_m:?}"
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn address_builtins_for_array_element_and_commented_field() {
    let uri = Url::parse("file:///ae.sol").unwrap();
    // `a[0]` is an address element; `s.who` is an address field with a leading comment
    // (the comment is trivia of the type and must not pollute the type text).
    let src = "struct S { // some note\naddress who; }\n\
               contract C { S s; function f() public { address[] memory a; a[0].M1; s.who.M2; } }";

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

    let line2 = src.lines().nth(2).unwrap();
    let labels = |id: i32, marker: &str| -> Vec<String> {
        let ch = line2.find(marker).unwrap() as u32;
        send_request(
            &client,
            id,
            "textDocument/completion",
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: doc_id(&uri),
                    position: Position {
                        line: 2,
                        character: ch,
                    },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            },
        );
        let r: CompletionResponse =
            serde_json::from_value(next_response(&client).result.unwrap()).unwrap();
        match r {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        }
        .into_iter()
        .map(|i| i.label)
        .collect()
    };
    assert!(
        labels(2, "M1").contains(&"call".to_string()),
        "array element"
    );
    assert!(
        labels(3, "M2").contains(&"call".to_string()),
        "commented struct field"
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn address_and_array_builtins_by_location_and_form() {
    let uri = Url::parse("file:///av.sol").unwrap();
    let src = "contract C { uint256[] sarr; struct S { address who; } S s; \
               function f(uint256[] memory marr) public { \
               sarr.A1; marr.A2; msg.sender.A3; address(this).A4; s.who.A5; } }";

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

    let labels = |id: i32, marker: &str| -> Vec<String> {
        let ch = src.find(marker).unwrap() as u32;
        send_request(
            &client,
            id,
            "textDocument/completion",
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: doc_id(&uri),
                    position: Position {
                        line: 0,
                        character: ch,
                    },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            },
        );
        let r: CompletionResponse =
            serde_json::from_value(next_response(&client).result.unwrap()).unwrap();
        match r {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        }
        .into_iter()
        .map(|i| i.label)
        .collect()
    };
    // storage array -> push/pop; memory array -> length only.
    assert!(labels(2, "A1").contains(&"push".to_string()));
    assert!(!labels(3, "A2").contains(&"push".to_string()));
    // address from a builtin member, a cast, and a struct field.
    for (id, m) in [(4, "A3"), (5, "A4"), (6, "A5")] {
        assert!(
            labels(id, m).contains(&"call".to_string()),
            "{m} missing address members"
        );
    }

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn completion_builtin_type_members() {
    let uri = Url::parse("file:///bt.sol").unwrap();
    let src = "interface IFoo { function x() external; }\n\
               contract C { function f() public { \
               address a; a.X1; uint256[] memory arr; arr.X2; uint256 m = type(uint256).X3; \
               bytes4 id = type(IFoo).X4; } }";

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
    let labels = |id: i32, marker: &str| -> Vec<String> {
        let ch = line1.find(marker).unwrap() as u32; // marker `X#` sits right after the dot
        send_request(
            &client,
            id,
            "textDocument/completion",
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: doc_id(&uri),
                    position: Position {
                        line: 1,
                        character: ch,
                    },
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
    let addr = labels(2, "X1");
    assert!(
        addr.contains(&"balance".to_string()) && addr.contains(&"call".to_string()),
        "{addr:?}"
    );
    // a memory array exposes `length` but not `push`/`pop` (those need storage).
    let arr = labels(3, "X2");
    assert!(arr.contains(&"length".to_string()), "{arr:?}");
    assert!(
        !arr.contains(&"push".to_string()),
        "memory array must not have push: {arr:?}"
    );
    let int_ty = labels(4, "X3");
    assert_eq!(int_ty, ["min", "max"]);
    let iface = labels(5, "X4");
    assert!(
        iface.contains(&"interfaceId".to_string()) && iface.contains(&"creationCode".to_string()),
        "{iface:?}"
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn completion_builtin_global_members() {
    let uri = Url::parse("file:///b.sol").unwrap();
    let src = "contract C { function f() public { uint256 x = block.; address a = tx.; } }";

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

    let labels = |id: i32, character: u32| -> Vec<String> {
        send_request(
            &client,
            id,
            "textDocument/completion",
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: doc_id(&uri),
                    position: Position { line: 0, character },
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

    let block = labels(2, src.find("block.").unwrap() as u32 + 6);
    assert!(
        block.contains(&"number".to_string()) && block.contains(&"timestamp".to_string()),
        "{block:?}"
    );
    let tx = labels(3, src.find("tx.").unwrap() as u32 + 3);
    assert!(
        tx.contains(&"origin".to_string()) && tx.contains(&"gasprice".to_string()),
        "{tx:?}"
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn named_arg_type_in_completion_and_hover() {
    let uri = Url::parse("file:///t.sol").unwrap();
    let src = "struct Point { uint256 x; address owner; }\n\
               contract C { function g() public { Point a = Point({ }); Point b = Point({ x: 1 }); } }";

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

    // completion at the empty `Point({ })` — each field carries its type in `detail`.
    let line1 = src.lines().nth(1).unwrap();
    let ch = line1.find("({ })").unwrap() as u32 + 2;
    send_request(
        &client,
        2,
        "textDocument/completion",
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 1,
                    character: ch,
                },
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
    let detail = |label: &str| {
        items
            .iter()
            .find(|i| i.label == label)
            .and_then(|i| i.detail.clone())
    };
    assert_eq!(detail("x").as_deref(), Some("uint256"));
    assert_eq!(detail("owner").as_deref(), Some("address"));

    // hover on the `x` key of `Point({ x: 1 })` → its field type.
    let hx = line1.rfind("x: 1").unwrap() as u32;
    send_request(
        &client,
        3,
        "textDocument/hover",
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 1,
                    character: hx,
                },
            },
            work_done_progress_params: Default::default(),
        },
    );
    let resp = next_response(&client);
    let hov: Hover = serde_json::from_value(resp.result.unwrap()).unwrap();
    let HoverContents::Markup(m) = hov.contents else {
        panic!("expected markup hover");
    };
    assert!(m.value.contains("uint256 x"), "hover was {:?}", m.value);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn signature_help_lists_overloads() {
    let uri = Url::parse("file:///ov.sol").unwrap();
    let src = "contract C { function f(uint256 a) public {} \
               function f(uint256 a, address b) public {} \
               function g() public { f(1, addr); } }";

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

    let ch = src.find("f(1, addr)").unwrap() as u32 + 5; // after the comma, on the 2nd arg
    send_request(
        &client,
        2,
        "textDocument/signatureHelp",
        SignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 0,
                    character: ch,
                },
            },
            work_done_progress_params: Default::default(),
            context: None,
        },
    );
    let resp = next_response(&client);
    let sh: SignatureHelp = serde_json::from_value(resp.result.unwrap()).unwrap();
    let labels: Vec<&str> = sh.signatures.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, ["f(uint256 a)", "f(uint256 a, address b)"]);
    // the 2-argument call selects the 2-parameter overload.
    assert_eq!(sh.active_signature, Some(1));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn signature_help_for_positional_call() {
    let uri = Url::parse("file:///s.sol").unwrap();
    let src = "contract C { function f(uint256 amount, address to) public {} \
               function g() public { uint256 x; address a; f(x, a); } }";

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

    let call = src.find("f(x, a)").unwrap();

    // signature help while on the second argument → the signature + active parameter 1.
    send_request(
        &client,
        2,
        "textDocument/signatureHelp",
        SignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 0,
                    character: (call + 5) as u32,
                }, // after `f(x, `
            },
            work_done_progress_params: Default::default(),
            context: None,
        },
    );
    let resp = next_response(&client);
    let sh: SignatureHelp = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(sh.signatures[0].label, "f(uint256 amount, address to)");
    assert_eq!(sh.active_parameter, Some(1));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn signature_help_on_inherited_internal_method() {
    use std::fs;

    let dir = std::env::temp_dir().join("solsp_sig_inherit");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let base = dir.join("Base.sol");
    let main = dir.join("Main.sol");
    fs::write(
        &base,
        "contract Base { function _helper(uint256 amount, address to) internal {} }\n",
    )
    .unwrap();
    fs::write(
        &main,
        "import {Base} from \"Base.sol\";\n\
         contract C is Base { function f() public { _helper(1, address(0)); } }\n",
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

    // signature help on the bare call to the inherited (cross-file) internal method.
    let call = main_src.lines().nth(1).unwrap().find("_helper(").unwrap() + 8;
    send_request(
        &client,
        2,
        "textDocument/signatureHelp",
        SignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&main_uri),
                position: Position {
                    line: 1,
                    character: call as u32,
                },
            },
            work_done_progress_params: Default::default(),
            context: None,
        },
    );
    let resp = next_response(&client);
    let sh: SignatureHelp = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        sh.signatures[0].label,
        "_helper(uint256 amount, address to)"
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

/// Resolve a single member-access definition request in `src` at the offset of `needle`,
/// returning the 0-based target line (same file). Panics if it does not resolve.
fn def_line_in_memory(src: &str, needle: &str) -> u32 {
    let uri = Url::parse("file:///probe.sol").unwrap();
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

    // click the LAST character of the needle (so the needle can carry context, e.g.
    // `s.x` lands on the member `x`).
    let off = src.find(needle).expect("needle") + needle.len() - 1;
    let before = &src[..off];
    let line = before.matches('\n').count() as u32;
    let character = (off - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    send_request(
        &client,
        2,
        "textDocument/definition",
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let resp = next_response(&client);
    let d: GotoDefinitionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
    let line = match d {
        GotoDefinitionResponse::Scalar(loc) => loc.range.start.line,
        _ => panic!("expected a single location for {needle:?}"),
    };
    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    line
}

#[test]
fn alias_import_resolves_despite_a_competing_glob_import() {
    // a glob import and an aliased named import target the SAME file. The glob's failed
    // search for `U` must not poison the cross-file walk's visited set and block the
    // alias's search for `Utils` in that file.
    use std::fs;
    let dir = std::env::temp_dir().join("solsp_alias_glob");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("Lib.sol"),
        "library Utils { function foo() internal pure returns (uint256) { return 1; } }\n",
    )
    .unwrap();
    fs::write(
        dir.join("Mid.sol"),
        "import \"Lib.sol\";\ncontract Mid {}\n",
    )
    .unwrap();
    let main = dir.join("Main.sol");
    fs::write(
        &main,
        "import \"Mid.sol\";\n\
         import { Utils as U } from \"Mid.sol\";\n\
         contract C { function f() public { U.foo(); } }\n",
    )
    .unwrap();

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let lib_uri = Url::from_file_path(fs::canonicalize(dir.join("Lib.sol")).unwrap()).unwrap();
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

    let line2 = main_src.lines().nth(2).unwrap();
    let ch = line2.find("U.foo").unwrap() as u32 + "U.".len() as u32;
    send_request(
        &client,
        2,
        "textDocument/definition",
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&main_uri),
                position: Position {
                    line: 2,
                    character: ch,
                },
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
    assert_eq!(loc.uri, lib_uri); // `U.foo` → Utils.foo in Lib.sol

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn namespace_import_member_resolves() {
    use std::fs;
    let dir = std::env::temp_dir().join("solsp_namespace");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("Lib.sol"),
        "function foo() pure returns (uint256) { return 1; }\ncontract Bar {}\n",
    )
    .unwrap();
    let main = dir.join("Main.sol");
    fs::write(
        &main,
        "import * as Utils from \"Lib.sol\";\n\
         contract C { function f() public { Utils.foo(); } }\n",
    )
    .unwrap();

    let main_uri = Url::from_file_path(fs::canonicalize(&main).unwrap()).unwrap();
    let lib_uri = Url::from_file_path(fs::canonicalize(dir.join("Lib.sol")).unwrap()).unwrap();
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

    // `Utils.foo` → the free function in Lib.sol.
    let line1 = main_src.lines().nth(1).unwrap();
    let ch = line1.find("Utils.foo").unwrap() as u32 + "Utils.".len() as u32;
    send_request(
        &client,
        2,
        "textDocument/definition",
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&main_uri),
                position: Position {
                    line: 1,
                    character: ch,
                },
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
    assert_eq!(loc.uri, lib_uri);
    assert_eq!(loc.range.start.line, 0);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tuple_declared_local_member_resolves() {
    // the second variable of a tuple destructuring (`Thing t`) must resolve, and member
    // access through it follows its declared type.
    let src = "struct Thing { uint256 amount; }\n\
               contract C { function g() internal returns (bool, Thing memory) {}\n\
               function f() public { (bool ok, Thing memory t) = g(); t.amount; } }";
    // `amount` in `t.amount` → the field on line 0 (resolved through `t`'s type).
    assert_eq!(def_line_in_memory(src, ".amount"), 0);
}

#[test]
fn library_member_completion_shows_internal_hides_private() {
    let uri = Url::parse("file:///lib.sol").unwrap();
    let src = "library L { function pubFn() public {} function intFn() internal {} \
               function privFn() private {} }\n\
               contract C { function f() public { L. } }";

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
    let ch = line1.find("L. ").unwrap() as u32 + 2;
    send_request(
        &client,
        2,
        "textDocument/completion",
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 1,
                    character: ch,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        },
    );
    let resp = next_response(&client);
    let r: CompletionResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
    let labels: Vec<String> = match r {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    }
    .into_iter()
    .map(|i| i.label)
    .collect();
    // a library exposes internal + public functions for direct `L.fn()` calls, but not
    // its private ones.
    assert!(labels.contains(&"pubFn".to_string()), "{labels:?}");
    assert!(
        labels.contains(&"intFn".to_string()),
        "internal should show: {labels:?}"
    );
    assert!(
        !labels.contains(&"privFn".to_string()),
        "private must be hidden: {labels:?}"
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn completion_callables_insert_call_parens() {
    let uri = Url::parse("file:///sn.sol").unwrap();
    let src = "contract C { function doThing(uint256 a) public {} uint256 val; \
               function f() public { d } }";

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

    let ch = src.find("{ d }").unwrap() as u32 + 3;
    send_request(
        &client,
        2,
        "textDocument/completion",
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 0,
                    character: ch,
                },
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
    let it = |label: &str| items.iter().find(|i| i.label == label).unwrap();
    // a function inserts `name()` with the cursor between the parens; a variable does not.
    assert_eq!(it("doThing").insert_text.as_deref(), Some("doThing($0)"));
    assert_eq!(
        it("doThing").insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET)
    );
    assert_eq!(it("val").insert_text, None);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn member_completion_shows_field_types() {
    let uri = Url::parse("file:///ft.sol").unwrap();
    let src = "struct Recipe { uint128 inflation; address owner; }\n\
               contract C { Recipe r; function f() public { r. } }";

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
    let ch = line1.find("r. ").unwrap() as u32 + 2;
    send_request(
        &client,
        2,
        "textDocument/completion",
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 1,
                    character: ch,
                },
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
    let detail = |label: &str| {
        items
            .iter()
            .find(|i| i.label == label)
            .and_then(|i| i.detail.clone())
    };
    assert_eq!(detail("inflation").as_deref(), Some("uint128"));
    assert_eq!(detail("owner").as_deref(), Some("address"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn mapping_value_member_resolves() {
    // `things[k].amount` — indexing the mapping yields its value type `Thing`, whose
    // `amount` field is declared on line 0.
    let src = "struct Thing { uint256 amount; }\n\
               contract C { mapping(bytes32 => Thing) things;\n\
               function f() public { things[bytes32(0)].amount; } }";
    assert_eq!(def_line_in_memory(src, "].amount"), 0);
}

#[test]
fn nested_struct_type_resolves_within_contract() {
    // `S` is declared inside the contract; `s.x` must resolve to the field on line 1.
    let src = "contract C {\n\
               struct S { uint256 x; }\n\
               S s;\n\
               function f() public { s.x; }\n\
               }";
    assert_eq!(def_line_in_memory(src, "s.x"), 1); // `x` in `s.x` → the field on line 1
}

#[test]
fn argument_type_mismatch_is_diagnosed() {
    let uri = Url::parse("file:///tc.sol").unwrap();
    // `takesAddr(s)` (string) and `takesAddr(a)` (a contract — not implicitly an address)
    // are both errors; only the explicit `address(a)` cast is valid.
    let src = "contract A {}\n\
               contract C { function takesAddr(address x) public {} \
               function f() public { string memory s; A a; \
               takesAddr(s); takesAddr(a); takesAddr(address(a)); } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let type_warnings: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("convertible"))
        .collect();
    // two: the `string` and the uncast contract; the `address(a)` cast is fine.
    assert_eq!(type_warnings.len(), 2, "{:?}", diags.diagnostics);
    // a type mismatch is an error, not a warning.
    assert_eq!(
        type_warnings[0].severity,
        Some(lsp_types::DiagnosticSeverity::ERROR)
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn named_argument_type_mismatch_is_diagnosed() {
    let uri = Url::parse("file:///na.sol").unwrap();
    // `f({ id: x })` passes a `bytes32` where the `id` parameter is `uint256`.
    let src = "contract C { function f(uint256 id) public {} \
               function g() public { bytes32 b; f({ id: b }); } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("convertible"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);
    assert!(errs[0].message.contains("uint256"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn mapping_argument_where_struct_expected_is_diagnosed() {
    let uri = Url::parse("file:///mp.sol").unwrap();
    // `m[k]` of a nested mapping is itself a mapping; passing it where a struct is
    // expected is a type error.
    let src = "struct R { uint256 x; }\n\
               contract C { mapping(uint256 => mapping(uint256 => R)) m; \
               function take(R storage r) internal {} \
               function f() public { take(m[1]); } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("convertible"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);
    assert!(errs[0].message.contains('R'));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn new_array_argument_is_not_a_false_positive() {
    let uri = Url::parse("file:///nw.sol").unwrap();
    // `new address[](0)` is an `address[]`, accepted by the `address[]` parameter — its
    // type must not be inferred as the element type `address`.
    let src = "contract C { function take(address[] memory xs) public {} \
               function f() public { take(new address[](0)); } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("convertible"))
        .collect();
    assert!(errs.is_empty(), "{:?}", diags.diagnostics);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn undefined_name_is_diagnosed() {
    let uri = Url::parse("file:///un.sol").unwrap();
    // `bar` is never declared; `foo`, the parameter `p`, the builtin `require`, and the
    // modifier placeholder `_` must NOT be flagged.
    let src = "contract C { uint256 foo; modifier m() { _; } \
               function f(uint256 p) public m { uint256 x = foo + p; require(x > 0); \
               uint256 y = bar + x; } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let undefined: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("not defined"))
        .collect();
    assert_eq!(undefined.len(), 1, "{:?}", diags.diagnostics);
    assert!(undefined[0].message.contains("bar"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn assignment_type_mismatch_is_diagnosed() {
    let uri = Url::parse("file:///as.sol").unwrap();
    // `n = h` (bytes32 -> uint256) and `uint8 s = n` (uint256 -> uint8) are errors; the
    // widening `uint256 m = small` and the literal `n = 5` are fine.
    let src = "contract C { uint256 n; bytes32 h; \
               function f() public { uint8 small; n = h; uint8 s = n; \
               uint256 m = small; n = 5; } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.starts_with("value of type"))
        .collect();
    assert_eq!(errs.len(), 2, "{:?}", diags.diagnostics);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn return_type_mismatch_is_diagnosed() {
    let uri = Url::parse("file:///rt.sol").unwrap();
    // `return h` (bytes32) from a `uint256` function is an error; the literal return is fine.
    let src = "contract C { bytes32 h; \
               function g() public view returns (uint256) { return h; } \
               function ok() public pure returns (uint256) { return 5; } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.starts_with("returned value"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);
    assert!(errs[0].message.contains("uint256"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn selector_completion_for_errors_and_events() {
    let uri = Url::parse("file:///sel.sol").unwrap();
    let src = "contract C { error MyError(uint256 x); event MyEvent(address a); \
               function f() public { MyError.S1; MyEvent.S2; } }";

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

    let complete = |id: i32, marker: &str| -> Vec<CompletionItem> {
        let ch = src.find(marker).unwrap() as u32;
        send_request(
            &client,
            id,
            "textDocument/completion",
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: doc_id(&uri),
                    position: Position {
                        line: 0,
                        character: ch,
                    },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            },
        );
        match serde_json::from_value(next_response(&client).result.unwrap()).unwrap() {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        }
    };
    let err = complete(2, "S1");
    assert!(err
        .iter()
        .any(|i| i.label == "selector" && i.detail.as_deref() == Some("bytes4")));
    let ev = complete(3, "S2");
    assert!(ev
        .iter()
        .any(|i| i.label == "selector" && i.detail.as_deref() == Some("bytes32")));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn hover_on_builtin_members() {
    let uri = Url::parse("file:///bh.sol").unwrap();
    let src = "contract C { error E(); function f() public { address x = msg.sender; \
               bytes32 s = E.selector; } }";

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

    let hover_at = |id: i32, marker: &str| -> String {
        let ch = (src.find(marker).unwrap() + marker.len()) as u32; // inside the member
        send_request(
            &client,
            id,
            "textDocument/hover",
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: doc_id(&uri),
                    position: Position {
                        line: 0,
                        character: ch,
                    },
                },
                work_done_progress_params: Default::default(),
            },
        );
        let r: lsp_types::Hover =
            serde_json::from_value(next_response(&client).result.unwrap()).unwrap();
        match r.contents {
            lsp_types::HoverContents::Markup(m) => m.value,
            _ => String::new(),
        }
    };
    assert!(hover_at(2, "msg.sen").contains("address"));
    assert!(hover_at(3, "E.sel").contains("bytes4"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn invalid_address_cast_of_function_is_diagnosed() {
    let uri = Url::parse("file:///cast.sol").unwrap();
    // `address(roles)` casts the function `roles`, which is invalid; `address(r)` (an
    // instance) and `address(this)` are fine.
    let src = "contract R {} \
               contract C { R r; \
               function roles() external view returns (address) { return address(roles); } \
               function g() external view returns (address) { return address(r); } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let casts: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("cannot convert"))
        .collect();
    assert_eq!(casts.len(), 1, "{:?}", diags.diagnostics);
    assert!(casts[0].message.contains("function"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn goto_def_picks_overload_by_argument_types() {
    let uri = Url::parse("file:///ov.sol").unwrap();
    // two same-arity overloads; the call's `bytes32` named arguments must select the bytes32
    // one, not the first-declared uint256 one (named-arg matching by key).
    let src = "contract C { \
               function eq(uint256 a, uint256 b) internal pure {} \
               function eq(bytes32 a, bytes32 b) internal pure {} \
               function f(bytes32 x, bytes32 y) internal pure { eq({ a: x, b: y }); } }";

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

    // go-to-def on the `eq` callee inside `f`.
    let call_pos = src.rfind("eq({ a: x").unwrap() as u32;
    send_request(
        &client,
        2,
        "textDocument/definition",
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: doc_id(&uri),
                position: Position {
                    line: 0,
                    character: call_pos,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let resp: GotoDefinitionResponse =
        serde_json::from_value(next_response(&client).result.unwrap()).unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected a scalar location")
    };
    // the resolved name sits on the bytes32 overload, after the uint256 one.
    let target = loc.range.start.character as usize;
    assert!(
        src[target..].starts_with("eq") && src[..target].contains("uint256 a, uint256 b"),
        "resolved to offset {target}, expected the bytes32 overload"
    );

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn undefined_yul_assignment_target_is_diagnosed() {
    let uri = Url::parse("file:///yul.sol").unwrap();
    // `result` is assigned in assembly but the return value is unnamed (no such variable);
    // the Yul local `m` and the named return in `ok` must not be flagged.
    let src = "contract C { \
               function bad(uint256 value) internal pure returns (uint256) { \
               assembly { result := value let m := value m := add(m, 1) } } \
               function ok(uint256 v) internal pure returns (uint256 r) { \
               assembly { r := v } } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let undefined: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("not defined"))
        .collect();
    assert_eq!(undefined.len(), 1, "{:?}", diags.diagnostics);
    assert!(undefined[0].message.contains("result"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn arithmetic_on_address_is_diagnosed() {
    let uri = Url::parse("file:///ar.sol").unwrap();
    // `n >> address(1)` and `n + a` are errors; the comparison and integer shift are fine.
    let src = "contract C { function f(address a, uint256 n) public pure { \
               uint256 x = n >> address(1); uint256 y = n + a; \
               uint256 z = n << 2; bool ok = a == address(0); } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("arithmetic or bitwise"))
        .collect();
    assert_eq!(errs.len(), 2, "{:?}", diags.diagnostics);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn call_arity_mismatch_is_diagnosed() {
    let uri = Url::parse("file:///ar.sol").unwrap();
    // `g(1)` and `g(1, 2, 3)` have the wrong argument count; `g(1, 2)` is fine.
    let src = "contract C { function g(uint256 a, uint256 b) public {} \
               function f() public { g(1); g(1, 2, 3); g(1, 2); } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("argument(s)"))
        .collect();
    assert_eq!(errs.len(), 2, "{:?}", diags.diagnostics);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn return_count_mismatch_is_diagnosed() {
    let uri = Url::parse("file:///rc.sol").unwrap();
    // `return (1, 2)` from a single-value function is wrong; the two-value return is fine.
    let src = "contract C { \
               function bad() public pure returns (uint256) { return (1, 2); } \
               function ok() public pure returns (uint256, uint256) { return (1, 2); } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("value(s)"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn incompatible_comparison_is_diagnosed() {
    let uri = Url::parse("file:///cmp.sol").unwrap();
    // `a < n` (address vs uint) is invalid; literal and same-type comparisons are fine.
    let src = "contract C { function f(address a, uint256 n) public pure { \
               bool x = a < n; bool y = n < 5; bool z = a == address(0); } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("cannot compare"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn literal_out_of_range_is_diagnosed() {
    let uri = Url::parse("file:///lit.sol").unwrap();
    // `uint8 a = 300` overflows; `uint8 b = 255` and `uint256 c = 300` are fine.
    let src = "contract C { function f() public pure { \
               uint8 a = 300; uint8 b = 255; uint256 c = 300; } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("does not fit"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);
    assert!(errs[0].message.contains("uint8"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn unreachable_code_is_diagnosed() {
    let uri = Url::parse("file:///un.sol").unwrap();
    // the statement after `return` is unreachable; the branch-then-return is fine.
    let src = "contract C { \
               function a() public pure returns (uint256) { return 1; uint256 x = 2; } \
               function b() public pure returns (uint256) { if (true) { return 1; } return 2; } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("unreachable"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn state_write_in_view_is_diagnosed() {
    let uri = Url::parse("file:///mut.sol").unwrap();
    // writing the state variable `x` in a `view` function is an error; the non-view write
    // and the local write are fine.
    let src = "contract C { uint256 x; \
               function bad() public view { x = 1; } \
               function ok() public { x = 1; } \
               function ok2() public view returns (uint256) { uint256 y = x; return y; } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("write to state"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn unused_import_is_diagnosed() {
    let uri = Url::parse("file:///ui.sol").unwrap();
    // `Unused` is never referenced; `Used` (inheritance) and `A` (a type) are.
    let src = "import { Used, Unused } from \"./x.sol\";\n\
               import { Aliased as A } from \"./y.sol\";\n\
               contract C is Used { A a; }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("imported but never used"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);
    assert!(errs[0].message.contains("Unused"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");
}

#[test]
fn unused_local_is_diagnosed() {
    let uri = Url::parse("file:///ul.sol").unwrap();
    // `unused` is never referenced; `used`/`y` are.
    let src = "contract C { function f(uint256 p) public pure returns (uint256) { \
               uint256 unused = 5; uint256 used = p + 1; uint256 y = used; return y; } }";

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
    let note = next_notification(&client, "textDocument/publishDiagnostics");
    let diags: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
    let errs: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("unused local"))
        .collect();
    assert_eq!(errs.len(), 1, "{:?}", diags.diagnostics);
    assert!(errs[0].message.contains("unused"));

    send_request(&client, 9, "shutdown", serde_json::Value::Null);
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
