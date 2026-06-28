//! Small LSP protocol helpers shared by request/notification handlers.

use lsp_server::{ErrorCode, ExtractError, Notification, Request, RequestId, Response};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, TextDocumentContentChangeEvent};
use solsp_ide::LineIndex;

use crate::to_proto;

/// A client command that re-opens signature help after a callable snippet is inserted. The
/// snippet writes the `(` itself, so the `(` signature-help trigger character never fires;
/// this nudges the client to request signature help with the cursor sitting inside the parens.
pub(crate) fn trigger_signature_help() -> lsp_types::Command {
    lsp_types::Command {
        title: "Signature help".to_string(),
        command: "editor.action.triggerParameterHints".to_string(),
        arguments: None,
    }
}

/// Wrap markdown text (and an optional range) into an LSP `Hover`.
pub(super) fn markup_hover(value: String, range: Option<lsp_types::Range>) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range,
    }
}

/// Apply one LSP content change to `text`. A change with a `range` splices the
/// replacement over those bytes (range is in UTF-16 line/col, mapped via a fresh
/// `LineIndex` over the current text); a change without a range replaces the whole
/// document. Out-of-range edits are ignored rather than panicking.
pub(super) fn apply_change(text: &mut String, change: TextDocumentContentChangeEvent) {
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
pub(super) fn extract_notification<N>(not: Notification) -> Option<N::Params>
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

/// Turn an `extract` failure into a JSON-RPC error response under the request's own
/// id (captured by the caller, since `JsonError` does not carry it).
pub(super) fn extract_err_response(id: RequestId, err: ExtractError<Request>) -> Response {
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
