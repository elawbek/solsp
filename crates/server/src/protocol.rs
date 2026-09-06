//! Small LSP protocol helpers shared by request/notification handlers.

use lsp_server::{ErrorCode, ExtractError, Notification, Request, RequestId, Response};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, TextDocumentContentChangeEvent};
use solsp_ide::LineIndex;
use std::borrow::Cow;

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

/// Apply ordered LSP changes with at most one copy of the original document/index.
/// A full replacement owns its payload and makes all preceding edits irrelevant.
/// Invalid and unchanged ranges are ignored; returns `None` if no edit is applied.
pub(super) fn apply_changes(
    original: &str,
    original_index: &LineIndex,
    changes: Vec<TextDocumentContentChangeEvent>,
) -> Option<(String, LineIndex)> {
    let first = changes
        .iter()
        .rposition(|change| change.range.is_none())
        .unwrap_or(0);
    let mut text = Cow::Borrowed(original);
    let mut index = Cow::Borrowed(original_index);
    for change in changes.into_iter().skip(first) {
        let Some(range) = change.range else {
            if change.text != original {
                index = Cow::Owned(LineIndex::new(&change.text));
                text = Cow::Owned(change.text);
            }
            continue;
        };
        let (Some(start), Some(end)) = (
            to_proto::offset(&index, range.start),
            to_proto::offset(&index, range.end),
        ) else {
            continue;
        };
        let (start, end) = (u32::from(start) as usize, u32::from(end) as usize);
        if start > end || end > text.len() || text[start..end] == change.text {
            continue;
        }
        if start == 0 && end == text.len() {
            index = Cow::Owned(LineIndex::new(&change.text));
            text = Cow::Owned(change.text);
            continue;
        }
        let text = text.to_mut();
        text.replace_range(start..end, &change.text);
        index.to_mut().apply_edit(
            rowan::TextRange::new((start as u32).into(), (end as u32).into()),
            &change.text,
            text,
        );
    }
    let Cow::Owned(text) = text else {
        return None;
    };
    Some((text, index.into_owned()))
}

/// Test oracle: the original one-edit-at-a-time pipeline.
/// Apply one LSP content change to `text`. A change with a `range` splices the
/// replacement over those bytes (range is in UTF-16 line/col, mapped via a fresh
/// `LineIndex` over the current text); a change without a range replaces the whole
/// document. Out-of-range edits are ignored rather than panicking.
#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};
    use std::{hint::black_box, time::Instant};

    fn edit(start: (u32, u32), end: (u32, u32), text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range::new(
                Position::new(start.0, start.1),
                Position::new(end.0, end.1),
            )),
            range_length: None,
            text: text.into(),
        }
    }

    fn full(text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.into(),
        }
    }

    #[test]
    fn batched_changes_match_sequential_rebuilds() {
        let original = "// é🌍\r\ncontract Old {}\r\n";
        for changes in [
            vec![],
            vec![
                edit((0, 0), (0, 999), "// short"),
                edit((1, 9), (1, 12), "New"),
            ],
            vec![
                full("é🌍\n"),
                edit((0, 1), (0, 2), "😀"),
                edit((0, 0), (0, 0), "\r\n"),
            ],
            vec![
                edit((1, 9), (1, 12), "Discarded"),
                full("discarded"),
                full(original),
                edit((1, 9), (1, 12), "Final"),
            ],
            vec![
                edit((99, 0), (99, 1), "invalid"),
                edit((1, 12), (1, 9), "reversed"),
                edit((1, 9), (1, 12), "Old"),
            ],
            vec![full(original)],
            vec![
                edit((0, 0), (2, 999), "full replacement\n"),
                edit((0, 5), (0, 16), "text"),
            ],
        ] {
            let mut expected = original.to_owned();
            for change in changes.clone() {
                apply_change(&mut expected, change);
            }
            let original_index = LineIndex::new(original);
            let result = apply_changes(original, &original_index, changes);
            let (actual, index) = result
                .as_ref()
                .map(|(text, index)| (text.as_str(), index))
                .unwrap_or((original, &original_index));
            assert_eq!(actual, expected);
            let rebuilt = LineIndex::new(&expected);
            for byte in actual
                .char_indices()
                .map(|(byte, _)| byte)
                .chain([actual.len()])
            {
                let byte = rowan::TextSize::from(byte as u32);
                let position = rebuilt.line_col(byte);
                assert_eq!(index.line_col(byte), position);
                assert_eq!(index.offset(position), rebuilt.offset(position));
            }
        }
    }

    #[test]
    fn full_replacements_reuse_payload_and_noops_leave_state_untouched() {
        let original = "abc";
        let index = LineIndex::new(original);
        for change in [full("replacement"), edit((0, 0), (0, 999), "replacement")] {
            let payload = change.text.as_ptr();
            let (text, _) = apply_changes(original, &index, vec![change]).unwrap();
            assert_eq!(
                text.as_ptr(),
                payload,
                "move the replacement buffer instead of copying it"
            );
        }
        for changes in [
            vec![],
            vec![full(original)],
            vec![edit((0, 0), (0, 3), original)],
            vec![edit((99, 0), (99, 0), "x")],
        ] {
            assert!(apply_changes(original, &index, changes).is_none());
        }
    }

    #[test]
    #[ignore = "local benchmark for batched document changes"]
    fn batched_edit_performance() {
        let mut source = String::from("contract Large {\n");
        for i in 0..10_000 {
            source.push_str(&format!("    uint public value{i:05} = 0;\n"));
        }
        source.push_str("}\n");
        let changes: Vec<_> = (0..100)
            .map(|i| TextDocumentContentChangeEvent {
                range: Some(Range::new(
                    Position::new(1 + i * 97, 29),
                    Position::new(1 + i * 97, 30),
                )),
                range_length: None,
                text: "12345".into(),
            })
            .collect();
        let initial_index = LineIndex::new(&source);
        let optimized_changes = changes.clone();
        let started = Instant::now();
        let mut text = source.clone();
        for change in changes {
            apply_change(&mut text, change);
        }
        black_box(LineIndex::new(&text));
        black_box(&text);
        let before = started.elapsed();
        let started = Instant::now();
        let (optimized, final_index) =
            apply_changes(&source, &initial_index, optimized_changes).unwrap();
        black_box(&optimized);
        black_box(final_index);
        let after = started.elapsed();
        assert_eq!(optimized, text);
        println!(
            "{} bytes, 100 sequential edits + final index: before={before:?}, after={after:?}",
            source.len(),
        );
    }
}
