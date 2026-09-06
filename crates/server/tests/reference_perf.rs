//! Local reference-search benchmark with large unrelated files already loaded.

use std::{thread, time::Instant};

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{Location, Url};

fn response(client: &Connection) -> Response {
    loop {
        if let Message::Response(response) = client.receiver.recv().unwrap() {
            return response;
        }
    }
}

#[test]
#[ignore = "local benchmark for repeated reference searches"]
fn repeated_references_with_large_unrelated_files() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || solsp_server::run(&server).unwrap());
    let open = |uri: &Url, text: String| {
        client.sender.send(Message::Notification(Notification::new("textDocument/didOpen".into(), serde_json::json!({
            "textDocument": {"uri": uri, "languageId": "solidity", "version": 0, "text": text}
        })))).unwrap();
    };
    let main = Url::parse("file:///reference-perf/Main.sol").unwrap();
    let source =
        "contract Main { function needle() public {} function run() public { needle(); } }";
    open(&main, source.into());
    let padding = "x".repeat(256 * 1024);
    for i in 0..64 {
        open(
            &Url::parse(&format!("file:///reference-perf/Other{i}.sol")).unwrap(),
            format!("/* {padding} */ contract Other{i} {{}}"),
        );
    }
    // A barrier keeps document loading outside the measured requests.
    client
        .sender
        .send(Message::Request(Request::new(
            1.into(),
            "textDocument/documentSymbol".into(),
            serde_json::json!({"textDocument": {"uri": main}}),
        )))
        .unwrap();
    let _ = response(&client);
    let query = |id: i32| {
        client.sender.send(Message::Request(Request::new(id.into(), "textDocument/references".into(), serde_json::json!({
            "textDocument": {"uri": main}, "position": {"line": 0, "character": source.find("needle").unwrap()}, "context": {"includeDeclaration": true}
        })))).unwrap();
        let locations: Vec<Location> =
            serde_json::from_value(response(&client).result.unwrap()).unwrap();
        assert_eq!(locations.len(), 2);
        assert!(locations.iter().all(|location| location.uri == main));
    };
    let started = Instant::now();
    query(2);
    let cold = started.elapsed();
    let started = Instant::now();
    for id in 3..53 {
        query(id);
    }
    println!(
        "64 unrelated files, 16 MiB of comments: first={cold:?}, 50 repeated requests={:?}",
        started.elapsed()
    );
    client
        .sender
        .send(Message::Request(Request::new(
            100.into(),
            "shutdown".into(),
            serde_json::Value::Null,
        )))
        .unwrap();
    let _ = response(&client);
    client
        .sender
        .send(Message::Notification(Notification::new(
            "exit".into(),
            serde_json::Value::Null,
        )))
        .unwrap();
    server_thread.join().unwrap();
}
