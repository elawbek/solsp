//! LSP transport loop, request dispatch, and document lifecycle notifications.

use anyhow::Result;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as _,
};
use lsp_types::request::{
    CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare,
    CodeActionRequest, CodeLensRequest, CodeLensResolve, Completion, DocumentSymbolRequest,
    GotoDefinition, HoverRequest, References, Rename, Request as _, SemanticTokensFullRequest,
    SignatureHelpRequest,
};
use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CodeActionParams, CodeLens, CodeLensParams, CompletionParams, DocumentSymbolParams,
    GotoDefinitionParams, HoverParams, ReferenceParams, RenameParams, SemanticTokensParams,
    SignatureHelpParams, Url,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::diagnostics::{
    publish_diagnostics, publish_syntax_diagnostics_if_errors, send_diagnostics,
};
use crate::protocol::{apply_change, extract_err_response, extract_notification};
use crate::state::{self, ServerState};

const CHANGE_SYNTAX_DEBOUNCE: Duration = Duration::from_millis(120);
const CHANGE_SEMANTIC_IDLE: Duration = Duration::from_millis(700);
const CHANGE_SEMANTIC_BUDGET: Duration = Duration::from_millis(150);

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
    let mut pending_diagnostics = PendingDiagnostics::default();

    loop {
        if publish_due_pending_diagnostics(connection, &state, &mut pending_diagnostics)? {
            continue;
        }

        let msg = if scan_pos < scan_queue.len() {
            match connection.receiver.try_recv() {
                Ok(msg) => msg,
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    if let Some(timeout) = pending_diagnostics.time_until_next_due(Instant::now()) {
                        match connection.receiver.recv_timeout(timeout) {
                            Ok(msg) => msg,
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
                        }
                    } else {
                        // idle: warm + diagnose the next file, then re-check for messages.
                        let uri = scan_queue[scan_pos].clone();
                        let started = std::time::Instant::now();
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
                        crate::perf::log_elapsed(
                            || format!("background scan {}", uri.as_str()),
                            started,
                        );
                        continue;
                    }
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => return Ok(()),
            }
        } else if let Some(timeout) = pending_diagnostics.time_until_next_due(Instant::now()) {
            match connection.receiver.recv_timeout(timeout) {
                Ok(msg) => msg,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
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
                let method = req.method.clone();
                let started = std::time::Instant::now();
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
                crate::perf::log_elapsed(|| format!("request {method}"), started);
                connection.sender.send(Message::Response(resp))?;
            }
            Message::Notification(not) => {
                let method = not.method.clone();
                let started = std::time::Instant::now();
                handle_notification(connection, &mut state, &mut pending_diagnostics, not)?;
                crate::perf::log_elapsed(|| format!("notification {method}"), started);
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
                Ok((id, params)) => {
                    Response::new_ok(id, super::navigation::document_symbols(state, params))
                }
                Err(e) => extract_err_response(id, e),
            }
        }
        SemanticTokensFullRequest::METHOD => {
            match req.extract::<SemanticTokensParams>(SemanticTokensFullRequest::METHOD) {
                Ok((id, params)) => {
                    Response::new_ok(id, super::navigation::semantic_tokens(state, params))
                }
                Err(e) => extract_err_response(id, e),
            }
        }
        GotoDefinition::METHOD => match req.extract::<GotoDefinitionParams>(GotoDefinition::METHOD)
        {
            Ok((id, params)) => {
                Response::new_ok(id, super::navigation::goto_definition(state, params))
            }
            Err(e) => extract_err_response(id, e),
        },
        References::METHOD => match req.extract::<ReferenceParams>(References::METHOD) {
            Ok((id, params)) => Response::new_ok(id, super::references::references(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        CallHierarchyPrepare::METHOD => {
            match req.extract::<CallHierarchyPrepareParams>(CallHierarchyPrepare::METHOD) {
                Ok((id, params)) => {
                    Response::new_ok(id, super::graphs::call_hierarchy_prepare(state, params))
                }
                Err(e) => extract_err_response(id, e),
            }
        }
        CallHierarchyIncomingCalls::METHOD => {
            match req
                .extract::<CallHierarchyIncomingCallsParams>(CallHierarchyIncomingCalls::METHOD)
            {
                Ok((id, params)) => {
                    Response::new_ok(id, super::graphs::call_hierarchy_incoming(state, params))
                }
                Err(e) => extract_err_response(id, e),
            }
        }
        CallHierarchyOutgoingCalls::METHOD => {
            match req
                .extract::<CallHierarchyOutgoingCallsParams>(CallHierarchyOutgoingCalls::METHOD)
            {
                Ok((id, params)) => {
                    Response::new_ok(id, super::graphs::call_hierarchy_outgoing(state, params))
                }
                Err(e) => extract_err_response(id, e),
            }
        }
        Rename::METHOD => match req.extract::<RenameParams>(Rename::METHOD) {
            Ok((id, params)) => Response::new_ok(id, super::references::rename(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        CodeLensRequest::METHOD => match req.extract::<CodeLensParams>(CodeLensRequest::METHOD) {
            Ok((id, params)) => Response::new_ok(id, super::references::code_lens(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        CodeLensResolve::METHOD => match req.extract::<CodeLens>(CodeLensResolve::METHOD) {
            Ok((id, lens)) => {
                Response::new_ok(id, super::references::code_lens_resolve(state, lens))
            }
            Err(e) => extract_err_response(id, e),
        },
        CodeActionRequest::METHOD => {
            match req.extract::<CodeActionParams>(CodeActionRequest::METHOD) {
                Ok((id, params)) => {
                    Response::new_ok(id, super::code_actions::code_action(state, params))
                }
                Err(e) => extract_err_response(id, e),
            }
        }
        HoverRequest::METHOD => match req.extract::<HoverParams>(HoverRequest::METHOD) {
            Ok((id, params)) => Response::new_ok(id, super::hover(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        Completion::METHOD => match req.extract::<CompletionParams>(Completion::METHOD) {
            Ok((id, params)) => Response::new_ok(id, super::completion(state, params)),
            Err(e) => extract_err_response(id, e),
        },
        SignatureHelpRequest::METHOD => {
            match req.extract::<SignatureHelpParams>(SignatureHelpRequest::METHOD) {
                Ok((id, params)) => Response::new_ok(id, super::signature_help(state, params)),
                Err(e) => extract_err_response(id, e),
            }
        }
        "solsp/inheritanceGraph" => {
            match req.extract::<serde_json::Value>("solsp/inheritanceGraph") {
                Ok((id, params)) => {
                    Response::new_ok(id, super::graphs::inheritance_graph(state, params))
                }
                Err(e) => extract_err_response(id, e),
            }
        }
        "solsp/functionCallGraph" => {
            match req.extract::<serde_json::Value>("solsp/functionCallGraph") {
                Ok((id, params)) => {
                    Response::new_ok(id, super::graphs::function_call_graph(state, params))
                }
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

/// Handle a notification: open/change update the store and republish diagnostics;
/// close drops the doc and clears its diagnostics. Unknown notifications are ignored.
fn handle_notification(
    connection: &Connection,
    state: &mut ServerState,
    pending_diagnostics: &mut PendingDiagnostics,
    not: Notification,
) -> Result<()> {
    match not.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let Some(params) = extract_notification::<DidOpenTextDocument>(not) else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            pending_diagnostics.remove(&uri);
            state.set(&uri, params.text_document.text);
            state.load_import_graph(&uri); // pull imported files into the db
            publish_syntax_diagnostics_if_errors(connection, state, &uri)?;
            pending_diagnostics.schedule_semantic(uri);
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
            let old_imports = import_directive_fingerprint(&text);
            for change in params.content_changes {
                apply_change(&mut text, change);
            }
            let imports_changed = old_imports != import_directive_fingerprint(&text);
            state.set(&uri, text);
            if imports_changed {
                state.load_import_graph(&uri);
            }
            pending_diagnostics.schedule(uri);
        }
        DidSaveTextDocument::METHOD => {
            let Some(params) = extract_notification::<DidSaveTextDocument>(not) else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            pending_diagnostics.remove(&uri);
            publish_diagnostics(connection, state, &uri, true, None)?;
        }
        DidCloseTextDocument::METHOD => {
            let Some(params) = extract_notification::<DidCloseTextDocument>(not) else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            pending_diagnostics.remove(&uri);
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

#[derive(Default)]
struct PendingDiagnostics {
    entries: HashMap<Url, PendingDiagnostic>,
}

struct PendingDiagnostic {
    syntax_due: Option<Instant>,
    semantic_due: Option<Instant>,
}

impl PendingDiagnostics {
    fn schedule(&mut self, uri: Url) {
        let now = Instant::now();
        self.entries.insert(
            uri,
            PendingDiagnostic {
                syntax_due: Some(now + CHANGE_SYNTAX_DEBOUNCE),
                semantic_due: Some(now + CHANGE_SEMANTIC_IDLE),
            },
        );
    }

    fn schedule_semantic(&mut self, uri: Url) {
        self.entries.insert(
            uri,
            PendingDiagnostic {
                syntax_due: None,
                semantic_due: Some(Instant::now() + CHANGE_SEMANTIC_IDLE),
            },
        );
    }

    fn remove(&mut self, uri: &Url) {
        self.entries.remove(uri);
    }

    fn time_until_next_due(&self, now: Instant) -> Option<Duration> {
        self.next_due()
            .map(|due| due.checked_duration_since(now).unwrap_or_default())
    }

    fn next_due(&self) -> Option<Instant> {
        self.entries
            .values()
            .flat_map(|entry| [entry.syntax_due, entry.semantic_due])
            .flatten()
            .min()
    }

    fn take_due(&mut self, now: Instant) -> Option<(Url, DiagnosticPhase)> {
        let uri = self
            .entries
            .iter()
            .filter_map(|(uri, entry)| entry.due_phase(now).map(|phase| (uri.clone(), phase)))
            .min_by_key(|(_, phase)| phase.sort_key())?
            .0;
        let entry = self.entries.get_mut(&uri)?;
        let phase = entry.due_phase(now)?;
        match phase {
            DiagnosticPhase::Syntax => entry.syntax_due = None,
            DiagnosticPhase::Semantic => entry.semantic_due = None,
        }
        if entry.syntax_due.is_none() && entry.semantic_due.is_none() {
            self.entries.remove(&uri);
        }
        Some((uri, phase))
    }
}

impl PendingDiagnostic {
    fn due_phase(&self, now: Instant) -> Option<DiagnosticPhase> {
        if self.syntax_due.is_some_and(|due| due <= now) {
            return Some(DiagnosticPhase::Syntax);
        }
        if self.semantic_due.is_some_and(|due| due <= now) {
            return Some(DiagnosticPhase::Semantic);
        }
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiagnosticPhase {
    Syntax,
    Semantic,
}

impl DiagnosticPhase {
    fn sort_key(self) -> u8 {
        match self {
            DiagnosticPhase::Syntax => 0,
            DiagnosticPhase::Semantic => 1,
        }
    }
}

fn publish_due_pending_diagnostics(
    connection: &Connection,
    state: &ServerState,
    pending: &mut PendingDiagnostics,
) -> Result<bool> {
    let Some((uri, phase)) = pending.take_due(Instant::now()) else {
        return Ok(false);
    };
    match phase {
        DiagnosticPhase::Syntax => {
            publish_diagnostics(connection, state, &uri, false, None)?;
        }
        DiagnosticPhase::Semantic => {
            publish_diagnostics(connection, state, &uri, true, Some(CHANGE_SEMANTIC_BUDGET))?;
        }
    }
    Ok(true)
}

fn import_directive_fingerprint(text: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let mut current = String::new();
    let mut in_import = false;

    for line in text.lines().map(str::trim) {
        if !in_import {
            if !is_import_start(line) {
                continue;
            }
            current.clear();
            in_import = true;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(line);
        if line.contains(';') {
            imports.push(current.clone());
            current.clear();
            in_import = false;
        }
    }
    if in_import && !current.is_empty() {
        imports.push(current);
    }
    imports
}

fn is_import_start(line: &str) -> bool {
    line == "import"
        || line
            .strip_prefix("import")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '"' | '\'' | '{' | '*'))
}

#[cfg(test)]
mod tests {
    use super::import_directive_fingerprint;

    #[test]
    fn import_fingerprint_ignores_body_edits() {
        let before = "import { A } from \"./A.sol\";\ncontract C { function f() public {} }";
        let after =
            "import { A } from \"./A.sol\";\ncontract C { function f() public { uint x = 1; } }";
        assert_eq!(
            import_directive_fingerprint(before),
            import_directive_fingerprint(after)
        );
    }

    #[test]
    fn import_fingerprint_tracks_multiline_import_edits() {
        let before = "import {\n  A\n} from \"./A.sol\";\ncontract C {}";
        let after = "import {\n  A,\n  B\n} from \"./A.sol\";\ncontract C {}";
        assert_ne!(
            import_directive_fingerprint(before),
            import_directive_fingerprint(after)
        );
    }

    #[test]
    fn import_fingerprint_ignores_import_prefix_identifiers() {
        let text = "contract C { function f() public { important = 1; } }";
        assert!(import_directive_fingerprint(text).is_empty());
    }
}
