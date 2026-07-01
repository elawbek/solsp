//! Code actions and quick-fix edit builders.

use super::*;

/// `textDocument/codeAction` -> quick fixes for concrete contracts that still owe
/// abstract/interface functions.
pub(super) fn code_action(
    state: &ServerState,
    params: CodeActionParams,
) -> Option<CodeActionResponse> {
    let uri = params.text_document.uri;
    let file = state.file(&uri)?;
    let li = state.line_index(&uri)?;
    let offset = to_proto::offset(li, params.range.start)?;
    let root = solsp_base_db::parse(state.db(), file).syntax();
    let contract = contract_at_offset(&root, offset)?;
    if !is_contract_contract(&contract) {
        return Some(Vec::new());
    }

    let mut actions = Vec::new();
    if let Some(function) = function_at_offset(&root, offset) {
        if !function_has_visibility(&function) {
            actions_extend_visibility(&mut actions, &uri, li, &function);
        }
    }

    let missing = missing_inherited_functions(state, &uri, &root, &contract);
    let needs_abstract = !missing.is_empty() || has_direct_abstract_function(&contract);

    if !missing.is_empty() {
        if let Some(edit) = implement_missing_edit(&uri, li, &contract, &missing) {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Implement missing inherited functions".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(edit),
                is_preferred: Some(true),
                ..Default::default()
            }));
        }
    }

    if needs_abstract && !is_abstract_contract(&contract) {
        if let Some(edit) = mark_abstract_edit(&uri, li, &contract) {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Mark contract abstract".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(edit),
                ..Default::default()
            }));
        }
    }

    Some(actions)
}

fn contract_at_offset(
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::CONTRACT_DEF;
    if let Some(token) = root.token_at_offset(offset).find(|t| {
        let range = t.text_range();
        range.start() <= offset && offset <= range.end()
    }) {
        if let Some(contract) = token.parent().and_then(|n| {
            n.ancestors()
                .find(|ancestor| ancestor.kind() == CONTRACT_DEF)
        }) {
            return Some(contract);
        }
    }
    root.descendants()
        .filter(|node| node.kind() == CONTRACT_DEF)
        .find(|node| {
            let range = node.text_range();
            range.start() <= offset && offset <= range.end()
        })
}

fn function_at_offset(
    root: &solsp_syntax::SyntaxNode,
    offset: rowan::TextSize,
) -> Option<solsp_syntax::SyntaxNode> {
    use solsp_syntax::SyntaxKind::FUNCTION_DEF;
    if let Some(token) = root.token_at_offset(offset).find(|token| {
        let range = token.text_range();
        range.start() <= offset && offset <= range.end()
    }) {
        if let Some(function) = token.parent().and_then(|node| {
            node.ancestors()
                .find(|ancestor| ancestor.kind() == FUNCTION_DEF)
        }) {
            return Some(function);
        }
    }
    root.descendants()
        .filter(|node| node.kind() == FUNCTION_DEF)
        .find(|node| {
            let range = node.text_range();
            range.start() <= offset && offset <= range.end()
        })
}

fn actions_extend_visibility(
    actions: &mut CodeActionResponse,
    uri: &Url,
    li: &solsp_ide::LineIndex,
    function: &solsp_syntax::SyntaxNode,
) {
    for visibility in ["public", "external", "internal", "private"] {
        if let Some(edit) = add_visibility_edit(uri, li, function, visibility) {
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Add {visibility} visibility"),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(edit),
                ..Default::default()
            }));
        }
    }
}

fn add_visibility_edit(
    uri: &Url,
    li: &solsp_ide::LineIndex,
    function: &solsp_syntax::SyntaxNode,
    visibility: &str,
) -> Option<WorkspaceEdit> {
    let offset = visibility_insert_offset(function)?;
    let position = to_proto::range(li, rowan::TextRange::new(offset, offset)).start;
    let mut changes = std::collections::HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit::new(
            lsp_types::Range {
                start: position,
                end: position,
            },
            format!(" {visibility}"),
        )],
    );
    Some(WorkspaceEdit::new(changes))
}

fn visibility_insert_offset(function: &solsp_syntax::SyntaxNode) -> Option<rowan::TextSize> {
    use solsp_syntax::SyntaxKind::PARAM_LIST;
    function
        .children()
        .find(|child| child.kind() == PARAM_LIST)
        .map(|params| params.text_range().end())
}

fn implement_missing_edit(
    uri: &Url,
    li: &solsp_ide::LineIndex,
    contract: &solsp_syntax::SyntaxNode,
    missing: &[MissingFunction],
) -> Option<WorkspaceEdit> {
    let insert_offset = contract_body_end_offset(contract)?;
    let position = to_proto::range(li, rowan::TextRange::new(insert_offset, insert_offset)).start;
    let mut new_text = String::new();
    let separator = if contract_body_has_members(contract) {
        "\n\n    "
    } else {
        "\n    "
    };
    for (index, function) in missing.iter().enumerate() {
        if index == 0 {
            new_text.push_str(separator);
        } else {
            new_text.push_str("\n\n    ");
        }
        new_text.push_str(&function_stub(&function.node)?);
    }
    new_text.push('\n');

    let mut changes = std::collections::HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit::new(
            lsp_types::Range {
                start: position,
                end: position,
            },
            new_text,
        )],
    );
    Some(WorkspaceEdit::new(changes))
}

fn mark_abstract_edit(
    uri: &Url,
    li: &solsp_ide::LineIndex,
    contract: &solsp_syntax::SyntaxNode,
) -> Option<WorkspaceEdit> {
    use solsp_syntax::SyntaxKind::CONTRACT_KW;
    let contract_kw = contract
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| token.kind() == CONTRACT_KW)?;
    let offset = contract_kw.text_range().start();
    let position = to_proto::range(li, rowan::TextRange::new(offset, offset)).start;
    let mut changes = std::collections::HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit::new(
            lsp_types::Range {
                start: position,
                end: position,
            },
            "abstract ".to_string(),
        )],
    );
    Some(WorkspaceEdit::new(changes))
}

fn function_stub(function: &solsp_syntax::SyntaxNode) -> Option<String> {
    use solsp_syntax::SyntaxKind::{
        BLOCK, L_BRACE, OVERRIDE_KW, RETURNS_KW, SEMICOLON, VIRTUAL_KW,
    };
    let mut tokens = Vec::new();
    let mut saw_override = false;
    let mut inserted_override = false;

    for token in function
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
    {
        match token.kind() {
            SEMICOLON | L_BRACE => break,
            BLOCK => break,
            VIRTUAL_KW => continue,
            OVERRIDE_KW => saw_override = true,
            RETURNS_KW if !saw_override && !inserted_override => {
                tokens.push((OVERRIDE_KW, "override".to_string()));
                inserted_override = true;
            }
            _ => {}
        }
        if !token.kind().is_trivia() {
            tokens.push((token.kind(), token.text().to_string()));
        }
    }
    if tokens.is_empty() {
        return None;
    }
    if !saw_override && !inserted_override {
        tokens.push((OVERRIDE_KW, "override".to_string()));
    }

    let mut signature = join_solidity_tokens(tokens);
    signature.push_str(" {\n        revert(\"Not implemented\");\n    }");
    Some(signature)
}

fn join_solidity_tokens(tokens: Vec<(solsp_syntax::SyntaxKind, String)>) -> String {
    use solsp_syntax::SyntaxKind::{COMMA, DOT, L_BRACK, L_PAREN, RETURNS_KW, R_BRACK, R_PAREN};
    let mut out = String::new();
    let mut prev_kind = None;
    let mut prev_text = String::new();

    for (kind, text) in tokens {
        if !out.is_empty() && needs_space_between(prev_kind, &prev_text, kind) {
            out.push(' ');
        }
        out.push_str(&text);
        prev_kind = Some(kind);
        prev_text = text;
    }

    fn needs_space_between(
        prev_kind: Option<solsp_syntax::SyntaxKind>,
        prev_text: &str,
        kind: solsp_syntax::SyntaxKind,
    ) -> bool {
        let Some(prev_kind) = prev_kind else {
            return false;
        };
        if matches!(kind, COMMA | R_PAREN | R_BRACK | DOT) {
            return false;
        }
        if matches!(prev_kind, L_PAREN | L_BRACK | DOT) {
            return false;
        }
        if kind == L_PAREN && prev_text != "returns" {
            return false;
        }
        if prev_kind == RETURNS_KW {
            return true;
        }
        true
    }

    out
}

fn has_direct_abstract_function(contract: &solsp_syntax::SyntaxNode) -> bool {
    use solsp_syntax::SyntaxKind::{CONTRACT_BODY, FUNCTION_DEF};
    contract
        .children()
        .find(|child| child.kind() == CONTRACT_BODY)
        .into_iter()
        .flat_map(|body| body.children())
        .any(|child| child.kind() == FUNCTION_DEF && is_abstract_function(&child))
}
