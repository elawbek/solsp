//! LSP diagnostics publishing.

use anyhow::Result;
use lsp_server::{Connection, Message, Notification};
use lsp_types::notification::{Notification as _, PublishDiagnostics};
use lsp_types::{PublishDiagnosticsParams, Url};

use crate::state::ServerState;
use crate::to_proto;

/// Compute and publish diagnostics for a document (empty list if missing). The semantic
/// type-check (slow, cross-file) runs only when `semantic` is set — on open/save and the
/// background sweep, not on every keystroke. `budget` bounds the type-check for the
/// background sweep; an open/save pass passes `None` and runs to completion.
pub(super) fn publish_diagnostics(
    connection: &Connection,
    state: &ServerState,
    uri: &Url,
    semantic: bool,
    budget: Option<std::time::Duration>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let diagnostics = match (state.file(uri), state.line_index(uri)) {
        (Some(file), Some(li)) => {
            let parse = solsp_base_db::parse(state.db(), file);
            let mut diags =
                to_proto::diagnostics(&solsp_ide::diagnostics::diagnostics(parse.errors()), li);
            // Semantic checks only on a syntactically clean file (a broken tree mid-edit is
            // noise). A shared deadline bounds the whole semantic pass on the background
            // sweep; an open/save pass passes `None` and runs to completion.
            if semantic && parse.errors().is_empty() {
                let deadline = budget.map(|b| std::time::Instant::now() + b);
                let root = parse.syntax();
                extend_timed(&mut diags, "undefined_name", uri, deadline, || {
                    super::name_diagnostics::undefined_name_diagnostics(
                        state, uri, &root, li, deadline,
                    )
                });
                extend_timed(&mut diags, "type_check", uri, deadline, || {
                    super::call_resolution::type_check_diagnostics(state, uri, &root, li, deadline)
                });
                extend_timed(&mut diags, "assignment", uri, deadline, || {
                    super::type_diagnostics::assignment_diagnostics(state, uri, &root, li, deadline)
                });
                extend_timed(&mut diags, "return_type", uri, deadline, || {
                    super::type_diagnostics::return_type_diagnostics(
                        state, uri, &root, li, deadline,
                    )
                });
                extend_timed(&mut diags, "cast", uri, deadline, || {
                    super::type_diagnostics::cast_diagnostics(state, uri, &root, li, deadline)
                });
                extend_timed(&mut diags, "binary_op", uri, deadline, || {
                    super::type_diagnostics::binary_op_diagnostics(state, uri, &root, li, deadline)
                });
                extend_timed(&mut diags, "comparison", uri, deadline, || {
                    super::type_diagnostics::comparison_diagnostics(state, uri, &root, li, deadline)
                });
                extend_timed(&mut diags, "condition", uri, deadline, || {
                    super::type_diagnostics::condition_diagnostics(state, uri, &root, li, deadline)
                });
                extend_timed(&mut diags, "unreachable", uri, deadline, || {
                    super::flow_diagnostics::unreachable_diagnostics(&root, li, deadline)
                });
                extend_timed(&mut diags, "mutability", uri, deadline, || {
                    super::mutability::mutability_diagnostics(state, uri, &root, li, deadline)
                });
                extend_timed(&mut diags, "missing_visibility", uri, deadline, || {
                    super::contract_diagnostics::missing_visibility_diagnostics(&root, li, deadline)
                });
                extend_timed(&mut diags, "unused_function", uri, deadline, || {
                    super::usage_diagnostics::unused_function_diagnostics(
                        state, uri, &root, li, deadline,
                    )
                });
                extend_timed(&mut diags, "unused_state_variable", uri, deadline, || {
                    super::usage_diagnostics::unused_state_variable_diagnostics(
                        state, uri, &root, li, deadline,
                    )
                });
                extend_timed(&mut diags, "unused_event", uri, deadline, || {
                    super::usage_diagnostics::unused_event_diagnostics(
                        state, uri, &root, li, deadline,
                    )
                });
                extend_timed(&mut diags, "unused_error", uri, deadline, || {
                    super::usage_diagnostics::unused_error_diagnostics(
                        state, uri, &root, li, deadline,
                    )
                });
                extend_timed(&mut diags, "abstract_contract", uri, deadline, || {
                    super::contract_diagnostics::abstract_contract_diagnostics(
                        state, uri, &root, li, deadline,
                    )
                });
                extend_timed(&mut diags, "invalid_import", uri, deadline, || {
                    super::import_diagnostics::invalid_import_diagnostics(
                        state, uri, &root, li, deadline,
                    )
                });
                extend_timed(&mut diags, "unused_import", uri, deadline, || {
                    super::import_diagnostics::unused_import_diagnostics(
                        state, uri, &root, li, deadline,
                    )
                });
                extend_timed(&mut diags, "unused_local", uri, deadline, || {
                    super::usage_diagnostics::unused_local_diagnostics(&root, li, deadline)
                });
            }
            diags
        }
        _ => Vec::new(),
    };
    let count = diagnostics.len();
    crate::perf::log_elapsed(
        || {
            format!(
                "diagnostics semantic={semantic} budget={:?} count={count} {}",
                budget,
                uri.as_str()
            )
        },
        started,
    );
    send_diagnostics(connection, uri.clone(), diagnostics)
}

pub(super) fn publish_syntax_diagnostics_if_errors(
    connection: &Connection,
    state: &ServerState,
    uri: &Url,
) -> Result<bool> {
    let (Some(file), Some(li)) = (state.file(uri), state.line_index(uri)) else {
        return Ok(false);
    };
    let parse = solsp_base_db::parse(state.db(), file);
    if parse.errors().is_empty() {
        return Ok(false);
    }
    let diagnostics =
        to_proto::diagnostics(&solsp_ide::diagnostics::diagnostics(parse.errors()), li);
    send_diagnostics(connection, uri.clone(), diagnostics)?;
    Ok(true)
}

fn extend_timed(
    out: &mut Vec<lsp_types::Diagnostic>,
    name: &'static str,
    uri: &Url,
    deadline: Option<std::time::Instant>,
    f: impl FnOnce() -> Vec<lsp_types::Diagnostic>,
) {
    if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
        return;
    }
    let started = std::time::Instant::now();
    let mut diagnostics = f();
    let count = diagnostics.len();
    crate::perf::log_elapsed(
        || format!("diagnostic phase {name} count={count} {}", uri.as_str()),
        started,
    );
    out.append(&mut diagnostics);
}

/// Send a `textDocument/publishDiagnostics` notification.
pub(super) fn send_diagnostics(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_budget_does_not_start_another_phase() {
        let uri = Url::parse("file:///budget.sol").unwrap();
        let mut diagnostics = Vec::new();
        extend_timed(
            &mut diagnostics,
            "test",
            &uri,
            Some(std::time::Instant::now()),
            || {
                panic!("an expired phase must not run its setup or tree walks");
            },
        );
        assert!(diagnostics.is_empty());
        let mut ran = false;
        extend_timed(&mut diagnostics, "test", &uri, None, || {
            ran = true;
            Vec::new()
        });
        assert!(ran, "unbudgeted save checks still run");
    }
}
