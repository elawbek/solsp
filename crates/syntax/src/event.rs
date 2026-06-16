//! Parser output: a flat event stream the tree builder replays into a rowan green
//! tree. Producing events (not nodes) decouples parsing from tree construction,
//! which is what makes error recovery tractable (design §3.3).

use crate::{SolidityLanguage, SyntaxError, SyntaxKind};
use rowan::{GreenNode, GreenNodeBuilder, Language, TextRange, TextSize};

#[derive(Debug)]
pub(crate) enum Event {
    /// Open a node. `forward_parent` (if set) is the *relative* index of a later
    /// `Start` that should become this node's parent — the mechanism behind
    /// `CompletedMarker::precede` (used for left-associative expressions later).
    Start {
        kind: SyntaxKind,
        forward_parent: Option<u32>,
    },
    /// Close the innermost open node.
    Finish,
    /// Attach the next non-trivia input token as a leaf of `kind`.
    Token { kind: SyntaxKind },
    /// Record a syntax error at the current position.
    Error { msg: String },
    /// An abandoned/placeholder slot. Skipped by the tree builder.
    Tombstone,
}

/// Replay events into a rowan green tree, re-attaching trivia from the full token
/// list so the result round-trips the source byte-for-byte. Returns the tree and
/// any collected syntax errors.
///
/// Trivia attachment is intentionally simple for M1: leading trivia is flushed
/// right before the next real token (so it lands in whatever node is open), and
/// trailing trivia is flushed into the root before it closes. This is lossless;
/// rust-analyzer's finer `n_attached_trivias` rules are a future refinement.
pub(crate) fn build_tree(
    text: &str,
    tokens: &[crate::lexer::Token],
    mut events: Vec<Event>,
) -> (GreenNode, Vec<SyntaxError>) {
    let mut builder = GreenNodeBuilder::new();
    let mut errors: Vec<SyntaxError> = Vec::new();

    // Byte span of every raw token (trivia included).
    let mut spans: Vec<(SyntaxKind, u32, u32)> = Vec::with_capacity(tokens.len());
    let mut off: u32 = 0;
    for t in tokens {
        spans.push((t.kind, off, off + t.len));
        off += t.len;
    }
    let text_len = text.len() as u32;

    let mut raw = 0usize; // index into `spans` (all tokens, incl trivia)
    let mut depth = 0usize;
    let mut forward_parents: Vec<SyntaxKind> = Vec::new();

    // Emit any trivia tokens at `raw` into the currently open node.
    let eat_trivia = |builder: &mut GreenNodeBuilder, raw: &mut usize| {
        while *raw < spans.len() && spans[*raw].0.is_trivia() {
            let (k, s, e) = spans[*raw];
            builder.token(
                SolidityLanguage::kind_to_raw(k),
                &text[s as usize..e as usize],
            );
            *raw += 1;
        }
    };

    for i in 0..events.len() {
        match std::mem::replace(&mut events[i], Event::Tombstone) {
            Event::Start {
                kind,
                forward_parent,
            } => {
                // Collect this node + any forward-parent chain, outermost last.
                forward_parents.push(kind);
                let mut fp = forward_parent;
                let mut idx = i;
                while let Some(fwd) = fp {
                    idx += fwd as usize;
                    fp = match std::mem::replace(&mut events[idx], Event::Tombstone) {
                        Event::Start {
                            kind,
                            forward_parent,
                        } => {
                            forward_parents.push(kind);
                            forward_parent
                        }
                        _ => unreachable!("forward_parent must point at a Start"),
                    };
                }
                for kind in forward_parents.drain(..).rev() {
                    builder.start_node(SolidityLanguage::kind_to_raw(kind));
                    depth += 1;
                }
            }
            Event::Finish => {
                if depth == 1 {
                    eat_trivia(&mut builder, &mut raw); // trailing trivia inside root
                }
                builder.finish_node();
                depth -= 1;
            }
            Event::Token { kind } => {
                eat_trivia(&mut builder, &mut raw);
                debug_assert!(
                    raw < spans.len(),
                    "Token event without a matching non-trivia token (parser/lexer desync)"
                );
                let (_k, s, e) = spans[raw];
                builder.token(
                    SolidityLanguage::kind_to_raw(kind),
                    &text[s as usize..e as usize],
                );
                raw += 1;
            }
            Event::Error { msg } => {
                let at = if raw < spans.len() {
                    spans[raw].1
                } else {
                    text_len
                };
                errors.push(SyntaxError {
                    message: msg,
                    range: TextRange::empty(TextSize::from(at)),
                });
            }
            Event::Tombstone => {}
        }
    }

    (builder.finish(), errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;
    use crate::SyntaxKind::*;

    #[test]
    fn build_tree_reattaches_trivia_losslessly() {
        // Hand-built events for "contract C" wrapped in CONTRACT_DEF.
        let src = "contract C";
        let tokens = tokenize(src);
        let events = vec![
            Event::Start {
                kind: CONTRACT_DEF,
                forward_parent: None,
            },
            Event::Token { kind: CONTRACT_KW },
            Event::Token { kind: IDENT },
            Event::Finish,
        ];
        let (green, errors) = build_tree(src, &tokens, events);
        let node = crate::SyntaxNode::new_root(green);
        // Lossless: the tree text equals the source (whitespace included).
        assert_eq!(node.text().to_string(), src);
        assert_eq!(node.kind(), CONTRACT_DEF);
        assert!(errors.is_empty());
    }

    #[test]
    fn build_tree_collects_errors() {
        let src = "contract";
        let tokens = tokenize(src);
        let events = vec![
            Event::Start {
                kind: SOURCE_FILE,
                forward_parent: None,
            },
            Event::Token { kind: CONTRACT_KW },
            Event::Error { msg: "boom".into() },
            Event::Finish,
        ];
        let (_green, errors) = build_tree(src, &tokens, events);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "boom");
    }
}
