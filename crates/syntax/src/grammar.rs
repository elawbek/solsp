//! Solidity declaration & type grammar (Plan 3). The parser emits events; this
//! module is the recursive-descent grammar that drives it. Statements, Pratt
//! expressions and inline assembly are later plans — here, function/modifier
//! bodies are a single `BLOCK` whose interior is span-skipped, and state-variable
//! initializers are span-skipped to `;`.

use crate::parser::{CompletedMarker, Parser};
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

/// `pragma <anything> ;` — the directive body isn't structured in M1; we span up
/// to and including the semicolon.
fn pragma(p: &mut Parser) {
    let m = p.start();
    p.bump(PRAGMA_KW);
    while !p.at(SEMICOLON) && !p.at(EOF) {
        p.bump_any();
    }
    p.expect(SEMICOLON);
    m.complete(p, PRAGMA_DIRECTIVE);
}

/// `('contract'|'interface'|'library'|'abstract' 'contract'?) NAME contract_body`
fn contract(p: &mut Parser) {
    let m = p.start();
    p.bump_any(); // contract/interface/library/abstract keyword
    if p.at(CONTRACT_KW) {
        p.bump(CONTRACT_KW); // `abstract contract`
    }
    if p.at(IDENT) {
        name(p);
    } else {
        p.error("expected a contract name");
    }
    if p.at(L_BRACE) {
        contract_body(p);
    } else {
        p.error("expected '{'");
    }
    m.complete(p, CONTRACT_DEF);
}

fn contract_body(p: &mut Parser) {
    let m = p.start();
    p.bump(L_BRACE);
    while !p.at(R_BRACE) && !p.at(EOF) {
        member(p);
    }
    p.expect(R_BRACE);
    m.complete(p, CONTRACT_BODY);
}

/// Dispatch one contract member. Every arm consumes at least one token (or
/// `err_and_bump` does), so the `contract_body` loop always makes progress.
fn member(p: &mut Parser) {
    match p.current() {
        IDENT | MAPPING_KW => state_var_def(p),
        _ => p.err_and_bump("expected a contract member"),
    }
}

/// `type_name (visibility|'constant'|'immutable'|override_spec)* NAME? ('=' init)? ';'`
/// The initializer is an expression — span-skipped to `;`/`}` until a later plan.
fn state_var_def(p: &mut Parser) {
    let m = p.start();
    type_name(p);
    loop {
        match p.current() {
            PUBLIC_KW | PRIVATE_KW | INTERNAL_KW | CONSTANT_KW | IMMUTABLE_KW => p.bump_any(),
            OVERRIDE_KW => override_spec(p),
            _ => break,
        }
    }
    if p.at(IDENT) {
        name(p);
    }
    if p.eat(EQ) {
        // initializer expression: skipped here (expressions land in a later plan).
        while !p.at(SEMICOLON) && !p.at(R_BRACE) && !p.at(EOF) {
            p.bump_any();
        }
    }
    p.expect(SEMICOLON);
    m.complete(p, STATE_VAR_DEF);
}

/// `'override' ('(' <names> ')')?` — the override list is skipped for now.
fn override_spec(p: &mut Parser) {
    p.bump(OVERRIDE_KW);
    if p.at(L_PAREN) {
        skip_parens(p);
    }
}

// ---- types -------------------------------------------------------------------

/// A type: a path (`uint256`, `A.B`); array/mapping/function forms land in Task 3.
fn type_name(p: &mut Parser) {
    path_type(p);
}

/// `name_ref ('.' name_ref)*` — covers elementary (`uint256`) and user (`A.B`)
/// type names alike; semantics tells them apart later. Returns the node so type
/// suffixes can wrap it (Task 3).
fn path_type(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    name_ref(p);
    while p.at(DOT) && p.nth(1) == IDENT {
        p.bump(DOT);
        name_ref(p);
    }
    m.complete(p, PATH_TYPE)
}

// ---- names -------------------------------------------------------------------

/// A defining name (binding occurrence).
fn name(p: &mut Parser) {
    if p.at(IDENT) {
        let m = p.start();
        p.bump(IDENT);
        m.complete(p, NAME);
    } else {
        p.error("expected a name");
    }
}

/// A referencing name (use occurrence) — used in type paths, base lists, etc.
fn name_ref(p: &mut Parser) {
    if p.at(IDENT) {
        let m = p.start();
        p.bump(IDENT);
        m.complete(p, NAME_REF);
    } else {
        p.error("expected a name");
    }
}

// ---- shared skips ------------------------------------------------------------

/// Balanced `'(' … ')'` whose interior is span-skipped (it holds expressions,
/// parsed in a later plan).
fn skip_parens(p: &mut Parser) {
    p.bump(L_PAREN);
    let mut depth = 1usize;
    while depth > 0 && !p.at(EOF) {
        match p.current() {
            L_PAREN => depth += 1,
            R_PAREN => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            break;
        }
        p.bump_any();
    }
    p.expect(R_PAREN);
}
