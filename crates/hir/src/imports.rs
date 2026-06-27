//! Import directive extraction (M2 P7). The grammar keeps `import` directives
//! lossless-but-unstructured (a token span up to `;`); here we read the structured
//! info cross-file resolution needs — the path string and which names it binds —
//! straight from those tokens (`from`/`as`/`{}`/`*` are all present in the tree).

use solsp_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

/// One import directive: its path and what it binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The import path with surrounding quotes stripped (e.g. `./Other.sol`).
    pub path: String,
    pub kind: ImportKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportKind {
    /// `import "path";` — every top-level symbol of the target file.
    Glob,
    /// `import {A, B as C} from "path";` — selected symbols (optional local alias).
    Named(Vec<ImportName>),
    /// `import * as N from "path";` / `import "path" as N;` — a namespace binding.
    Namespace(String),
}

/// A symbol named in a `{ ... }` import, with an optional local alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportName {
    /// The name as exported by the target file.
    pub name: String,
    /// The local alias (`as C`); when absent the symbol keeps `name`.
    pub alias: Option<String>,
}

impl ImportName {
    /// The name this import binds locally (`alias` if present, else `name`).
    pub fn local(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// All import directives in a file, in source order.
pub fn imports(root: &SyntaxNode) -> Vec<Import> {
    root.children()
        .filter(|n| n.kind() == SyntaxKind::IMPORT_DIRECTIVE)
        .filter_map(|d| extract(&d))
        .collect()
}

/// The import directive at `offset`, if the cursor is inside one (e.g. on its path
/// string or `from`). Lets go-to-def on an import open the target file.
pub fn import_at(root: &SyntaxNode, offset: rowan::TextSize) -> Option<Import> {
    let token = root.token_at_offset(offset).next()?;
    let directive = token
        .parent()?
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::IMPORT_DIRECTIVE)?;
    extract(&directive)
}

/// Read one `IMPORT_DIRECTIVE` node's structured form, or `None` if it has no path.
fn extract(directive: &SyntaxNode) -> Option<Import> {
    use SyntaxKind::*;
    let toks: Vec<SyntaxToken> = directive
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
        .collect();

    // The path is the single string literal in the directive.
    let path = toks
        .iter()
        .find(|t| t.kind() == STRING)
        .map(|t| unquote(t.text()))?;

    // `{ ... }` ⇒ a named import.
    if toks.iter().any(|t| t.kind() == L_BRACE) {
        return Some(Import {
            path,
            kind: ImportKind::Named(extract_named(&toks)),
        });
    }

    // `* as N` or `"path" as N` ⇒ a namespace binding (the IDENT right after `as`).
    if let Some(pos) = toks.iter().position(|t| t.kind() == AS_KW) {
        if let Some(alias) = toks.get(pos + 1).filter(|t| t.kind() == IDENT) {
            return Some(Import {
                path,
                kind: ImportKind::Namespace(alias.text().to_string()),
            });
        }
    }

    Some(Import {
        path,
        kind: ImportKind::Glob,
    })
}

/// Names between `{` and `}`: `A` or `A as B`, comma-separated.
fn extract_named(toks: &[SyntaxToken]) -> Vec<ImportName> {
    use SyntaxKind::*;
    let start = toks
        .iter()
        .position(|t| t.kind() == L_BRACE)
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = toks
        .iter()
        .position(|t| t.kind() == R_BRACE)
        .unwrap_or(toks.len());
    let inner = &toks[start..end.max(start)];

    let mut names = Vec::new();
    let mut i = 0;
    while i < inner.len() {
        if inner[i].kind() == IDENT {
            let name = inner[i].text().to_string();
            let mut alias = None;
            if inner.get(i + 1).map(|t| t.kind()) == Some(AS_KW) {
                if let Some(b) = inner.get(i + 2).filter(|t| t.kind() == IDENT) {
                    alias = Some(b.text().to_string());
                    i += 2; // consume `as B`
                }
            }
            names.push(ImportName { name, alias });
        }
        i += 1; // step past commas / stray tokens
    }
    names
}

/// Strip a single layer of matching `"`/`'` quotes from a string-literal token.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        if (first == b'"' || first == b'\'') && *bytes.last().unwrap() == first {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solsp_syntax::parse;

    fn imps(src: &str) -> Vec<Import> {
        imports(&parse(src).syntax())
    }

    #[test]
    fn glob_named_and_namespace() {
        let src = "import \"./A.sol\";\n\
            import {X, Y as Z} from \"./B.sol\";\n\
            import * as N from \"./C.sol\";\n\
            import \"./D.sol\" as M;\n\
            contract C {}";
        let v = imps(src);
        assert_eq!(v.len(), 4);

        assert_eq!(v[0].path, "./A.sol");
        assert_eq!(v[0].kind, ImportKind::Glob);

        assert_eq!(v[1].path, "./B.sol");
        assert_eq!(
            v[1].kind,
            ImportKind::Named(vec![
                ImportName {
                    name: "X".into(),
                    alias: None
                },
                ImportName {
                    name: "Y".into(),
                    alias: Some("Z".into())
                },
            ])
        );
        // Y is bound locally as Z
        let ImportKind::Named(names) = &v[1].kind else {
            unreachable!()
        };
        assert_eq!(names[0].local(), "X");
        assert_eq!(names[1].local(), "Z");

        assert_eq!(v[2].kind, ImportKind::Namespace("N".into()));
        assert_eq!(v[3].kind, ImportKind::Namespace("M".into()));
    }

    #[test]
    fn no_imports_is_empty() {
        assert!(imps("contract C {}").is_empty());
    }
}
