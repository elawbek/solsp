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
        FUNCTION_KW | FALLBACK_KW | RECEIVE_KW => function_def(p),
        MODIFIER_KW => modifier_def(p),
        CONSTRUCTOR_KW => constructor_def(p),
        STRUCT_KW => struct_def(p),
        ENUM_KW => enum_def(p),
        EVENT_KW => event_def(p),
        ERROR_KW => error_def(p),
        TYPE_KW => user_defined_value_type(p),
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

// ---- functions ---------------------------------------------------------------

/// `('function' NAME? | 'fallback' | 'receive') param_list function_attribute*
///  ('returns' param_list)? (block | ';')`
fn function_def(p: &mut Parser) {
    let m = p.start();
    let named = p.at(FUNCTION_KW);
    p.bump_any(); // function / fallback / receive
    if named && p.at(IDENT) {
        name(p);
    }
    if p.at(L_PAREN) {
        param_list(p);
    } else {
        p.error("expected '(' after function");
    }
    function_attributes(p);
    if p.eat(RETURNS_KW) {
        if p.at(L_PAREN) {
            param_list(p);
        } else {
            p.error("expected '(' after returns");
        }
    }
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.expect(SEMICOLON);
    }
    m.complete(p, FUNCTION_DEF);
}

/// Visibility / mutability / `virtual` / `override(...)` / modifier invocations,
/// in any order, until the body or `returns`.
fn function_attributes(p: &mut Parser) {
    loop {
        match p.current() {
            PUBLIC_KW | PRIVATE_KW | INTERNAL_KW | EXTERNAL_KW | PURE_KW | VIEW_KW | PAYABLE_KW
            | VIRTUAL_KW => p.bump_any(),
            OVERRIDE_KW => override_spec(p),
            IDENT => modifier_invocation(p),
            _ => break,
        }
    }
}

/// `name_ref ('(' <args> ')')?` — a base-modifier/constructor call in a header.
fn modifier_invocation(p: &mut Parser) {
    let m = p.start();
    name_ref(p);
    if p.at(L_PAREN) {
        skip_parens(p);
    }
    m.complete(p, MODIFIER_INVOCATION);
}

/// A `{ … }` body. Statements land in a later plan, so the interior is span-
/// skipped to the matching brace; the node is still a real `BLOCK`.
fn block(p: &mut Parser) {
    let m = p.start();
    p.bump(L_BRACE);
    let mut depth = 1usize;
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
    m.complete(p, BLOCK);
}

// ---- modifiers & constructors ------------------------------------------------

/// `'modifier' NAME? param_list? ('virtual'|override_spec)* (block | ';')`
fn modifier_def(p: &mut Parser) {
    let m = p.start();
    p.bump(MODIFIER_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.at(L_PAREN) {
        param_list(p);
    }
    loop {
        match p.current() {
            VIRTUAL_KW => p.bump_any(),
            OVERRIDE_KW => override_spec(p),
            _ => break,
        }
    }
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.expect(SEMICOLON);
    }
    m.complete(p, MODIFIER_DEF);
}

/// `'constructor' param_list? function_attribute* (block | ';')`
fn constructor_def(p: &mut Parser) {
    let m = p.start();
    p.bump(CONSTRUCTOR_KW);
    if p.at(L_PAREN) {
        param_list(p);
    }
    function_attributes(p); // visibility / payable / base-constructor invocations
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.expect(SEMICOLON);
    }
    m.complete(p, CONSTRUCTOR_DEF);
}

// ---- structs & enums ---------------------------------------------------------

/// `'struct' NAME? '{' struct_field* '}'`
fn struct_def(p: &mut Parser) {
    let m = p.start();
    p.bump(STRUCT_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.eat(L_BRACE) {
        while !p.at(R_BRACE) && !p.at(EOF) {
            if at_type_start(p) {
                struct_field(p);
            } else {
                p.err_and_bump("expected a struct field");
            }
        }
        p.expect(R_BRACE);
    } else {
        p.error("expected '{'");
    }
    m.complete(p, STRUCT_DEF);
}

/// `type_name NAME? ';'`
fn struct_field(p: &mut Parser) {
    let m = p.start();
    type_name(p);
    if p.at(IDENT) {
        name(p);
    }
    p.expect(SEMICOLON);
    m.complete(p, STRUCT_FIELD);
}

/// `'enum' NAME? '{' (enum_variant (',' enum_variant)*)? ','? '}'`
fn enum_def(p: &mut Parser) {
    let m = p.start();
    p.bump(ENUM_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.eat(L_BRACE) {
        while !p.at(R_BRACE) && !p.at(EOF) {
            if p.at(IDENT) {
                let v = p.start();
                name(p);
                v.complete(p, ENUM_VARIANT);
                if !p.eat(COMMA) {
                    break;
                }
            } else {
                p.err_and_bump("expected an enum variant");
            }
        }
        p.expect(R_BRACE);
    } else {
        p.error("expected '{'");
    }
    m.complete(p, ENUM_DEF);
}

// ---- events, errors & user-defined value types -------------------------------

/// `'event' NAME? param_list? IDENT? /*anonymous*/ ';'`
fn event_def(p: &mut Parser) {
    let m = p.start();
    p.bump(EVENT_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.at(L_PAREN) {
        param_list(p);
    }
    if p.at(IDENT) {
        p.bump_any(); // soft `anonymous`
    }
    p.expect(SEMICOLON);
    m.complete(p, EVENT_DEF);
}

/// `'error' NAME? param_list? ';'`
fn error_def(p: &mut Parser) {
    let m = p.start();
    p.bump(ERROR_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.at(L_PAREN) {
        param_list(p);
    }
    p.expect(SEMICOLON);
    m.complete(p, ERROR_DEF);
}

/// `'type' NAME? 'is' type_name ';'`
fn user_defined_value_type(p: &mut Parser) {
    let m = p.start();
    p.bump(TYPE_KW);
    if p.at(IDENT) {
        name(p);
    }
    p.expect(IS_KW);
    type_name(p);
    p.expect(SEMICOLON);
    m.complete(p, USER_DEFINED_VALUE_TYPE);
}

// ---- types -------------------------------------------------------------------

/// A type: a path/mapping/function base, then zero or more `[ size? ]` suffixes.
/// Array suffixes wrap left-associatively via `precede` (`uint[2][]` ⇒
/// ARRAY_TYPE(ARRAY_TYPE(uint, 2))).
fn type_name(p: &mut Parser) {
    let mut cm = match p.current() {
        MAPPING_KW => mapping_type(p),
        FUNCTION_KW => function_type(p),
        _ => path_type(p),
    };
    while p.at(L_BRACK) {
        let wrap = cm.precede(p);
        p.bump(L_BRACK);
        // array size is a constant expression — skipped until a later plan.
        while !p.at(R_BRACK) && !p.at(EOF) {
            p.bump_any();
        }
        p.expect(R_BRACK);
        cm = wrap.complete(p, ARRAY_TYPE);
    }
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

/// `'mapping' '(' type_name NAME? '=>' type_name NAME? ')'` — the optional names
/// are the 0.8.18+ named key/value syntax.
fn mapping_type(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    p.bump(MAPPING_KW);
    p.expect(L_PAREN);
    type_name(p);
    if p.at(IDENT) {
        name(p); // optional key name
    }
    p.expect(FAT_ARROW);
    type_name(p);
    if p.at(IDENT) {
        name(p); // optional value name
    }
    p.expect(R_PAREN);
    m.complete(p, MAPPING_TYPE)
}

/// `'function' param_list (visibility|mutability)* ('returns' param_list)?`
fn function_type(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    p.bump(FUNCTION_KW);
    if p.at(L_PAREN) {
        param_list(p);
    } else {
        p.error("expected '(' in function type");
    }
    while let INTERNAL_KW | EXTERNAL_KW | PUBLIC_KW | PRIVATE_KW | PURE_KW | VIEW_KW | PAYABLE_KW =
        p.current()
    {
        p.bump_any();
    }
    if p.eat(RETURNS_KW) {
        if p.at(L_PAREN) {
            param_list(p);
        } else {
            p.error("expected '(' after returns");
        }
    }
    m.complete(p, FUNCTION_TYPE)
}

/// `'(' (param (',' param)*)? ')'`
fn param_list(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    while !p.at(R_PAREN) && !p.at(EOF) {
        if !at_type_start(p) {
            // not the start of a parameter — recover one token and retry.
            p.err_and_bump("expected a parameter");
            continue;
        }
        param(p);
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    m.complete(p, PARAM_LIST);
}

/// `type_name (data_location | soft_modifier)* NAME?` — `soft_modifier` covers
/// `indexed`/`anonymous`-style words that lex as IDENT (a real name, if present,
/// is the final IDENT).
fn param(p: &mut Parser) {
    let m = p.start();
    type_name(p);
    loop {
        match p.current() {
            MEMORY_KW | STORAGE_KW | CALLDATA_KW => p.bump_any(),
            IDENT if p.nth(1) == IDENT => p.bump_any(),
            _ => break,
        }
    }
    if p.at(IDENT) {
        name(p);
    }
    m.complete(p, PARAM);
}

/// True when the current token can begin a `type_name`.
fn at_type_start(p: &Parser) -> bool {
    matches!(p.current(), IDENT | MAPPING_KW | FUNCTION_KW)
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
