# SolSP

SolSP is a Solidity language extension for Visual Studio Code powered by a
bundled Rust language server.

It provides navigation, completion, hover, semantic highlighting, diagnostics,
CodeLens, quick fixes, Call Hierarchy, and graph commands for Solidity projects,
including Foundry-style import layouts and remappings.

## Features

### Language Server

- Rust language server written from scratch.
- Lossless Solidity parser and CST.
- Salsa-backed source database.
- Incremental document sync.
- Workspace/import graph loading.
- Cross-file analysis over loaded project files.
- Conservative semantic analysis: unknown or unmodeled types are skipped instead
  of reported noisily.

### Project and Import Model

- Relative imports: `./X.sol`, `../X.sol`.
- Project-root-relative imports: `contracts/X.sol`, `src/X.sol`.
- Foundry-style dependency layouts: `lib/...`.
- Node package layouts: `node_modules/...`.
- Remappings from `remappings.txt` and `foundry.toml`.
- Named imports.
- Renamed imports.
- Namespace imports: `import * as N`.
- Glob imports.
- Re-export traversal.
- Cross-file inheritance traversal with cycle/diamond guards.

### Navigation

- Go to definition for same-file declarations.
- Go to definition for imported top-level symbols.
- Go to definition for import path strings.
- Go to definition for namespace import members.
- Go to definition for contract/interface/library members through receiver type.
- Go to definition for inherited members, including cross-file bases.
- Go to definition for overloaded calls selected by argument types when possible.
- Find All References across loaded workspace files.
- Cross-file Rename Symbol over loaded references.
- Rename validation for Solidity identifiers and keyword rejection.

### Completion

- Scope completion for visible locals, parameters, state variables, functions,
  modifiers, events, errors, contracts, interfaces, libraries, structs, enums,
  user-defined value types, imported symbols, and builtins.
- Member completion after `.`.
- Member completion for contract/interface instances, static type receivers,
  libraries, structs, `this`, and `super`.
- External receiver filtering: contract instances expose only public/external
  members.
- `using L for T` member completion, including imported libraries and elementary
  value types.
- Namespace import completion for aliases and `N.member`.
- Named-argument key completion for functions, events, errors, structs,
  modifiers, and constructors.
- Import path completion inside import strings:
  - `/` triggers suggestions only inside import paths
  - `./` and `../` complete relative paths
  - bare paths complete from the project root
  - directories and `.sol` files are listed
  - hidden/build directories and non-Solidity files are filtered out
  - directory listings are cached briefly for responsive typing
- Solidity keyword and elementary type completion.
- Global builtin completion: `msg`, `block`, `tx`, `abi`, `this`, `super`,
  `type`, and `now`.
- Builtin function completion: `require`, `assert`, `revert`, `keccak256`,
  `sha256`, `ripemd160`, `ecrecover`, `addmod`, `mulmod`, `selfdestruct`,
  `blockhash`, and `gasleft`.
- Builtin member completion for `block`, `tx`, `msg`, and `abi`.
- Address member completion: `balance`, `code`, `codehash`, `call`,
  `delegatecall`, `staticcall`, `transfer`, and `send`.
- Array and `bytes` member completion: `length`, plus `push`/`pop` for storage
  dynamic arrays and storage `bytes`.
- Fixed-size array and `bytesN` member completion for `length`.
- `type(X)` member completion for integer/enum `min` and `max`, and
  contract/interface `name`, `creationCode`, `runtimeCode`, and `interfaceId`.
- `.selector` completion for functions, errors, and events.
- Inline assembly completion for Yul/EVM builtins and opcodes.
- Callable completions insert snippet parentheses and trigger signature help.

### Hover and Signature Help

- Hover shows Solidity declaration signatures in markdown code blocks.
- Hover works for same-file declarations, imports, re-exports, members, and
  inherited members.
- Hover disambiguates overloaded calls by argument types when possible.
- Hover on named-argument keys shows the target parameter/field type.
- Hover on literals shows inferred literal type.
- Hover on synthetic builtins shows member type.
- Hover supports `.selector`, `type(X)` members, address members, array/bytes
  members, and global builtin members.
- Hover inside inline assembly shows Yul/EVM builtin signatures and short
  descriptions.
- Signature help on `(` and `,`.
- Signature help for functions, modifiers, structs, constructors, events, and
  errors.
- Overload lists for same-named functions.
- Active parameter tracking.

### Diagnostics

- Parser/syntax diagnostics.
- Undefined Solidity identifiers used as values.
- Undefined Yul assignment targets.
- Call arity diagnostics.
- Positional and named argument type diagnostics.
- Overload mismatch diagnostics.
- Assignment and local initializer type diagnostics.
- Integer literal range diagnostics.
- Return value count and return type diagnostics.
- Tuple return arity diagnostics.
- Invalid address/payable cast diagnostics.
- Arithmetic, bitwise, shift, comparison, and condition type diagnostics.
- Unreachable code diagnostics after `return`, `revert`, `break`, and
  `continue`.
- Function mutability diagnostics:
  - `pure` reads state
  - `view`/`pure` writes state
  - can-be-`view` hints
  - can-be-`pure` hints
  - storage aliases and storage-returning helpers
  - write-through effects for field/index assignment, `push`/`pop`, unary
    mutation, and `delete`
  - inline assembly/Yul effects for storage, transient storage, logs, calls,
    creates, selfdestruct, environment reads, and pure opcodes
- Missing explicit function visibility diagnostics.
- Unused local variable diagnostics.
- Unused private/internal function diagnostics.
- Override-aware unused function diagnostics.
- Unused non-public state variable diagnostics.
- Unused event diagnostics.
- Unused custom error diagnostics.
- Invalid named import diagnostics.
- Unused named import and namespace import diagnostics.
- Unused import suppression with:

```solidity
/// forge-lint: disable-next-line(unused-import)
```

- Missing inherited/interface implementation diagnostics.
- Abstract contract diagnostics.
- Forge-std cheatcode/logging calls (`vm`, `console`, `console2`) are skipped in
  expensive type-check paths to reduce noise and latency.

### Document Symbols and Semantic Tokens

- Nested document symbols for contracts, interfaces, libraries, functions,
  constructors, modifiers, structs, enums, events, errors, state variables, and
  user-defined value types.
- Full-document semantic tokens.
- Semantic coloring for declarations and references.
- Semantic coloring for comments, literals, keywords, user-defined types, enum
  members, events, errors, fields, parameters, variables, functions, and
  modifiers.
- Semantic coloring for inline assembly/Yul definitions, parameters, return
  names, paths, and function calls.
- Bundled Solidity TextMate grammar for immediate first-pass highlighting.

### CodeLens, Code Actions, and Graphs

- CodeLens reference counts above supported declarations.
- CodeLens opens VS Code's native references UI.
- Large-file guard: reference-count CodeLens is disabled for files over the
  large-file threshold to keep hover/completion responsive.
- Quick fixes to add missing function visibility.
- Quick fix to implement missing inherited/interface functions.
- Quick fix to mark contracts `abstract` when needed.
- Native LSP Call Hierarchy prepare/incoming/outgoing support.
- `SolSP: Show Inheritance Graph` command.
- `SolSP: Show Function Call Graph` command.
- Graph commands return Mermaid graph data with source metadata.

## Settings

| Setting              | Default | Description                              |
| -------------------- | ------- | ---------------------------------------- |
| `solsp.server.path`  | `""`    | Absolute path to a custom server binary. |
| `solsp.trace.server` | `off`   | Trace JSON-RPC traffic for debugging.    |

Server lookup order:

1. `solsp.server.path`
2. `SOLSP_SERVER_PATH`
3. bundled extension binary
4. workspace/repo `target/debug` or `target/release`
5. `solsp-server` on `PATH`

## Install from a VSIX

Choose the VSIX that matches your platform:

```text
solsp-vscode-linux-x64.vsix
solsp-vscode-linux-arm64.vsix
solsp-vscode-win32-x64.vsix
solsp-vscode-win32-arm64.vsix
solsp-vscode-darwin-x64.vsix
solsp-vscode-darwin-arm64.vsix
```

Install from command line:

```sh
code --install-extension path/to/solsp-vscode-linux-x64.vsix --force
```

Or install from Visual Studio Code:

```text
Extensions -> ... -> Install from VSIX...
```

Reload the VS Code window after installation, then open a folder containing
Solidity files.

## Performance Notes

- `didOpen` publishes syntax diagnostics immediately and schedules semantic
  diagnostics for idle/background work.
- `didChange` debounces diagnostics.
- Import graph reload runs only when import directives change.
- Expensive semantic diagnostics run with a budget during background sweeps.
- Directory listings for import-path completion are cached briefly.
- Large generated/helper files avoid expensive reference-count CodeLens scans.

## Packaging

Release VSIX packages include a platform-specific `solsp-server` binary.

Supported VSIX targets:

- `linux-x64`
- `linux-arm64`
- `win32-x64`
- `win32-arm64`
- `darwin-x64`
- `darwin-arm64`
