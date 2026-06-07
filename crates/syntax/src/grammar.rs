//! Minimal Solidity grammar that proves the parser pipeline end-to-end:
//! `source_file → item*`, where an item is a pragma, a contract-like shell, or an
//! error-recovered token. Breadth (members, statements, expressions, assembly)
//! lands in later plans.

use crate::parser::Parser;
use crate::SyntaxKind::*;

pub(crate) fn source_file(p: &mut Parser) {
    let m = p.start();
    while !p.at(EOF) {
        item(p);
    }
    m.complete(p, SOURCE_FILE);
}

fn item(p: &mut Parser) {
    match p.current() {
        PRAGMA_KW => pragma(p),
        CONTRACT_KW | INTERFACE_KW | LIBRARY_KW | ABSTRACT_KW => contract(p),
        _ => p.err_and_bump("expected an item (pragma, contract, …)"),
    }
}

/// `pragma <anything> ;` — the directive body isn't structured in M1; we just span
/// up to and including the semicolon.
fn pragma(p: &mut Parser) {
    let m = p.start();
    p.bump(PRAGMA_KW);
    while !p.at(SEMICOLON) && !p.at(EOF) {
        p.bump_any();
    }
    p.expect(SEMICOLON);
    m.complete(p, PRAGMA_DIRECTIVE);
}

/// `('contract'|'interface'|'library'|'abstract' …) NAME { <skipped body> }` —
/// the body is skipped to the matching brace for now (members come later).
fn contract(p: &mut Parser) {
    let m = p.start();
    p.bump_any(); // contract/interface/library/abstract keyword
    // `abstract` may be followed by `contract`.
    if p.at(CONTRACT_KW) {
        p.bump(CONTRACT_KW);
    }
    name(p);
    if p.at(L_BRACE) {
        p.bump(L_BRACE);
        // Shallow skip of the body until the matching close brace.
        let mut depth = 1;
        while depth > 0 && !p.at(EOF) {
            match p.current() {
                L_BRACE => depth += 1,
                R_BRACE => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                break;
            }
            p.bump_any();
        }
        p.expect(R_BRACE);
    } else {
        p.error("expected '{'");
    }
    m.complete(p, CONTRACT_DEF);
}

fn name(p: &mut Parser) {
    if p.at(IDENT) {
        let m = p.start();
        p.bump(IDENT);
        m.complete(p, NAME);
    } else {
        p.error("expected a name");
    }
}
