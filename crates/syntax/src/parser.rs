//! The recursive-descent parser API the grammar drives. It does not build a tree:
//! it pushes `Event`s and hands out `Marker`s for node boundaries (design §3.3).

use crate::event::Event;
use crate::input::Input;
use crate::SyntaxKind;

pub(crate) struct Parser<'t> {
    input: &'t Input,
    pos: usize,
    events: Vec<Event>,
}

impl<'t> Parser<'t> {
    pub(crate) fn new(input: &'t Input) -> Parser<'t> {
        Parser { input, pos: 0, events: Vec::new() }
    }

    /// Kind of the token `n` ahead (0 = current); `EOF` past the end.
    pub(crate) fn nth(&self, n: usize) -> SyntaxKind {
        self.input.kind(self.pos + n)
    }

    pub(crate) fn current(&self) -> SyntaxKind {
        self.nth(0)
    }

    pub(crate) fn at(&self, kind: SyntaxKind) -> bool {
        self.current() == kind
    }

    /// Open a node: push a placeholder event, return a marker pointing at it.
    pub(crate) fn start(&mut self) -> Marker {
        let pos = self.events.len() as u32;
        self.events.push(Event::Tombstone);
        Marker { pos }
    }

    /// Consume the current token as a leaf of `kind`. Panics if not at `kind`
    /// (callers must check first) — keeps grammar bugs loud.
    pub(crate) fn bump(&mut self, kind: SyntaxKind) {
        assert!(self.at(kind), "expected to bump {:?}, found {:?}", kind, self.current());
        self.do_bump(kind);
    }

    /// Consume the current token whatever it is (no-op at EOF). Used by recovery.
    pub(crate) fn bump_any(&mut self) {
        let k = self.current();
        if k == SyntaxKind::EOF {
            return;
        }
        self.do_bump(k);
    }

    fn do_bump(&mut self, kind: SyntaxKind) {
        self.pos += 1;
        self.events.push(Event::Token { kind });
    }

    /// If at `kind`, consume it and return true.
    pub(crate) fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.do_bump(kind);
            true
        } else {
            false
        }
    }

    /// Consume `kind` if present, else record an error and return false.
    pub(crate) fn expect(&mut self, kind: SyntaxKind) -> bool {
        if self.eat(kind) {
            true
        } else {
            self.error(format!("expected {:?}", kind));
            false
        }
    }

    pub(crate) fn error(&mut self, msg: impl Into<String>) {
        self.events.push(Event::Error { msg: msg.into() });
    }

    /// Wrap the current token in an ERROR node with a message (recovery step that
    /// always makes progress).
    pub(crate) fn err_and_bump(&mut self, msg: &str) {
        let m = self.start();
        self.error(msg.to_string());
        self.bump_any();
        m.complete(self, SyntaxKind::ERROR);
    }

    pub(crate) fn finish(self) -> Vec<Event> {
        self.events
    }
}

/// A pending open node. Complete it with a kind, or abandon it.
pub(crate) struct Marker {
    pos: u32,
}

impl Marker {
    /// Turn the placeholder into `Start { kind }` and push the matching `Finish`.
    pub(crate) fn complete(self, p: &mut Parser, kind: SyntaxKind) -> CompletedMarker {
        let idx = self.pos as usize;
        match &mut p.events[idx] {
            slot @ Event::Tombstone => {
                *slot = Event::Start { kind, forward_parent: None };
            }
            other => unreachable!("marker slot was not a tombstone: {:?}", other),
        }
        p.events.push(Event::Finish);
        CompletedMarker { pos: self.pos }
    }

    /// Drop the node. If the placeholder is the last event, pop it.
    #[allow(dead_code)] // recovery counterpart to `complete`; first consumer is a later grammar plan
    pub(crate) fn abandon(self, p: &mut Parser) {
        let idx = self.pos as usize;
        if idx == p.events.len() - 1 {
            assert!(matches!(p.events.pop(), Some(Event::Tombstone)));
        }
        // otherwise leave the Tombstone in place; the tree builder skips it.
    }
}

/// A finished node, which can become the child of a node started *after* it
/// (left-associative grouping) via `precede`.
pub(crate) struct CompletedMarker {
    pos: u32,
}

impl CompletedMarker {
    /// Start a new node that will wrap this one. Records the relationship as a
    /// `forward_parent` the tree builder resolves.
    pub(crate) fn precede(self, p: &mut Parser) -> Marker {
        let new_m = p.start();
        if let Event::Start { forward_parent, .. } = &mut p.events[self.pos as usize] {
            *forward_parent = Some(new_m.pos - self.pos);
        }
        new_m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Input;
    use crate::lexer::tokenize;
    use crate::SyntaxKind::*;

    #[test]
    fn parser_emits_start_token_finish() {
        // Hand-drive the parser over "contract" and check the event shape.
        let toks = tokenize("contract");
        let input = Input::new(&toks);
        let mut p = Parser::new(&input);

        let m = p.start();
        assert!(p.at(CONTRACT_KW));
        p.bump(CONTRACT_KW);
        assert!(p.at(EOF));
        m.complete(&mut p, CONTRACT_DEF);

        let events = p.finish();
        // Start(CONTRACT_DEF), Token(CONTRACT_KW), Finish
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], Event::Start { kind: CONTRACT_DEF, .. }));
        assert!(matches!(events[1], Event::Token { kind: CONTRACT_KW }));
        assert!(matches!(events[2], Event::Finish));
    }

    #[test]
    fn expect_records_error_when_missing() {
        let toks = tokenize("contract");
        let input = Input::new(&toks);
        let mut p = Parser::new(&input);
        p.bump(CONTRACT_KW);
        assert!(!p.expect(L_BRACE)); // no '{' -> false + error event
        let events = p.finish();
        assert!(events.iter().any(|e| matches!(e, Event::Error { .. })));
    }
}
