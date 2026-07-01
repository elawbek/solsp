use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CodeLens, CodeLensParams, DidOpenTextDocumentParams, InitializeParams, InitializeResult,
    TextDocumentIdentifier, TextDocumentItem, Url,
};

const DEFAULT_SOLARRAY: &str = "/home/epc/projects/smart-contracts/test/helpers/Solarray.sol";
const DEFAULT_WORKSPACE: &str = "/home/epc/projects/smart-contracts";
const LARGE_FILE_BUDGET: Duration = Duration::from_millis(500);
const CODE_LENS_BUDGET: Duration = Duration::from_secs(2);

fn solarray_path() -> String {
    std::env::var("SOLSP_SOLARRAY_PATH").unwrap_or_else(|_| DEFAULT_SOLARRAY.to_string())
}

fn read_solarray() -> Option<(String, String)> {
    let path = solarray_path();
    fs::read_to_string(&path).ok().map(|text| (path, text))
}

fn workspace_path() -> String {
    std::env::var("SOLSP_PERF_WORKSPACE").unwrap_or_else(|_| DEFAULT_WORKSPACE.to_string())
}

#[test]
#[ignore = "local perf test for large Solidity files"]
fn solarray_parse_stays_under_large_file_budget() {
    let Some((path, text)) = read_solarray() else {
        eprintln!("skip: Solarray.sol not found at {}", solarray_path());
        return;
    };

    let line_count = text.lines().count();
    let started = Instant::now();
    let (parsed, timings) = solsp_syntax::parse_with_timings(&text);
    let syntax = parsed.syntax();
    let elapsed = started.elapsed();

    eprintln!(
        "solarray parse: path={path} lines={} nodes={} errors={} elapsed={elapsed:?} lexer={:?} input={:?} parser={:?} tree={:?} tokens={} events={}",
        line_count,
        syntax.descendants().count(),
        parsed.errors().len(),
        timings.lexer,
        timings.input,
        timings.parser,
        timings.tree,
        timings.token_count,
        timings.event_count,
    );
    assert!(
        elapsed <= LARGE_FILE_BUDGET,
        "Solarray.sol parse took {elapsed:?}, budget {LARGE_FILE_BUDGET:?}"
    );
}

#[test]
#[ignore = "local perf test for large Solidity workspaces"]
fn workspace_parse_profile_reports_slowest_files() {
    let root = PathBuf::from(workspace_path());
    let files = collect_sol_files(&root);
    if files.is_empty() {
        eprintln!("skip: no .sol files under {}", root.display());
        return;
    }

    let mut total = PhaseTotals::default();
    let mut profiles = Vec::new();
    for path in files {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let line_count = text.lines().count();
        let started = Instant::now();
        let (parsed, timings) = solsp_syntax::parse_with_timings(&text);
        let elapsed = started.elapsed();
        total.add(timings);
        profiles.push(FileProfile {
            path,
            lines: line_count,
            errors: parsed.errors().len(),
            elapsed,
            timings,
        });
    }

    profiles.sort_by_key(|profile| std::cmp::Reverse(profile.elapsed));
    eprintln!(
        "workspace parse: root={} files={} total={:?} lexer={:?} input={:?} parser={:?} tree={:?}",
        root.display(),
        profiles.len(),
        total.total,
        total.lexer,
        total.input,
        total.parser,
        total.tree,
    );
    for profile in profiles.iter().take(10) {
        eprintln!(
            "workspace parse top: elapsed={:?} lines={} errors={} lexer={:?} parser={:?} tree={:?} path={}",
            profile.elapsed,
            profile.lines,
            profile.errors,
            profile.timings.lexer,
            profile.timings.parser,
            profile.timings.tree,
            profile.path.display(),
        );
    }
}

#[test]
#[ignore = "local perf test for large Solidity files"]
fn solarray_code_lens_stays_under_large_file_budget() {
    let Some((path, text)) = read_solarray() else {
        eprintln!("skip: Solarray.sol not found at {}", solarray_path());
        return;
    };
    let uri = Url::from_file_path(&path).expect("file uri");
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || {
        let caps = serde_json::to_value(solsp_server::server_capabilities()).unwrap();
        server.initialize(caps).expect("handshake");
        solsp_server::run(&server).expect("run");
    });

    send_request(&client, 1, "initialize", InitializeParams::default());
    let init: InitializeResult =
        serde_json::from_value(next_response(&client).result.unwrap()).expect("initialize result");
    assert!(init.capabilities.code_lens_provider.is_some());
    send_notification(&client, "initialized", lsp_types::InitializedParams {});

    send_notification(
        &client,
        "textDocument/didOpen",
        DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "solidity".to_string(),
                version: 0,
                text,
            },
        },
    );

    let started = Instant::now();
    send_request(
        &client,
        2,
        "textDocument/codeLens",
        CodeLensParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let lenses: Option<Vec<CodeLens>> =
        serde_json::from_value(next_response(&client).result.unwrap()).expect("code lens result");
    let elapsed = started.elapsed();
    let lens_count = lenses.as_ref().map_or(0, Vec::len);
    let unresolved_count = lenses
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|lens| lens.command.is_none() && lens.data.is_some())
        .count();
    eprintln!(
        "solarray codeLens: path={path} lenses={lens_count} unresolved={unresolved_count} elapsed={elapsed:?}",
    );

    send_request(&client, 3, "shutdown", serde_json::Value::Null);
    let _ = next_response(&client);
    send_notification(&client, "exit", serde_json::Value::Null);
    server_thread.join().expect("server thread panicked");

    assert!(
        elapsed <= CODE_LENS_BUDGET,
        "Solarray.sol codeLens took {elapsed:?}, budget {CODE_LENS_BUDGET:?}"
    );
    assert_eq!(
        unresolved_count, 0,
        "large files must not return reference-count lenses that trigger thousands of resolves"
    );
}

fn send_request(client: &Connection, id: i32, method: &str, params: impl serde::Serialize) {
    let req = Request::new(RequestId::from(id), method.to_string(), params);
    client.sender.send(Message::Request(req)).unwrap();
}

fn send_notification(client: &Connection, method: &str, params: impl serde::Serialize) {
    let not = Notification::new(method.to_string(), params);
    client.sender.send(Message::Notification(not)).unwrap();
}

fn next_response(client: &Connection) -> Response {
    loop {
        match client.receiver.recv().expect("server closed early") {
            Message::Response(response) => return response,
            Message::Notification(_) => continue,
            Message::Request(request) => panic!("unexpected server request: {request:?}"),
        }
    }
}

#[derive(Default)]
struct PhaseTotals {
    lexer: Duration,
    input: Duration,
    parser: Duration,
    tree: Duration,
    total: Duration,
}

impl PhaseTotals {
    fn add(&mut self, timings: solsp_syntax::ParseTimings) {
        self.lexer += timings.lexer;
        self.input += timings.input;
        self.parser += timings.parser;
        self.tree += timings.tree;
        self.total += timings.total;
    }
}

struct FileProfile {
    path: PathBuf,
    lines: usize,
    errors: usize,
    elapsed: Duration,
    timings: solsp_syntax::ParseTimings,
}

fn collect_sol_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_sol_files_into(root, &mut files);
    files
}

fn collect_sol_files_into(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.is_file() {
        if path.extension().is_some_and(|ext| ext == "sol") {
            out.push(path.to_path_buf());
        }
        return;
    }
    if !meta.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.') || name == "node_modules" || name == "out")
        {
            continue;
        }
        collect_sol_files_into(&child, out);
    }
}
