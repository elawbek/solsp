# solsp

Language server for Solidity, written from scratch in Rust in the
rust-analyzer style: a hand-written parser produces a lossless CST (rowan),
analysis is layered over the tree and a salsa-backed source database, and the
server speaks LSP over stdio.

Design and roadmap live in `docs/superpowers/specs/` (gitignored — local only).

## Layout

```
crates/
  syntax/   solsp-syntax  — lexer -> parser -> CST (rowan) -> typed AST (pure)
  base-db/  solsp-base-db — salsa inputs and tracked parse query
  hir/      solsp-hir     — item model, imports, scopes, name resolution
  ide/      solsp-ide     — diagnostics, document symbols, semantic tokens, navigation
  server/   solsp-server  — LSP over stdio, project state, cross-file features
editors/
  vscode/   solsp-vscode  — VS Code client that launches solsp-server
```

`crates/server` is split into small protocol/feature helpers around the main LSP
loop:

- `capabilities`, `protocol`, `to_proto` — LSP wire-level helpers
- `state` — tracked document store, salsa database, import loading
- `builtins`, `completion_items`, `named_args`, `using_for`, `import_surface` —
  completion/navigation support code
- `diagnostics`, `typecheck`, `syntax_utils` — diagnostics plumbing and shared
  semantic helpers

## LSP features

The server currently advertises and implements:

- incremental text document sync, open/close notifications, and save
  notifications
- `textDocument/documentSymbol`
- `textDocument/semanticTokens/full`
- `textDocument/definition`
- `textDocument/references`
- `textDocument/rename`
- `textDocument/codeLens` and `codeLens/resolve`
- `textDocument/codeAction` for quick fixes
- `textDocument/hover`
- `textDocument/completion`
- `textDocument/signatureHelp`
- `textDocument/publishDiagnostics`

The implementation is intentionally conservative: unknown or unmodeled types are
generally skipped rather than reported, to avoid noisy false positives.

### Project model

- Solidity parser written from scratch with lossless CST support.
- Solidity inline assembly / Yul parsing for blocks, variables, assignments,
  calls, paths, control-flow nodes, function definitions, and Yul semantic
  tokens.
- Salsa-backed file database and tracked parse query.
- Incremental in-memory document store for open files.
- Import graph loading and project/workspace warming from opened files.
- Cross-file import resolution for relative imports, project-root-relative
  paths, Foundry-style dependency layouts, and remappings.
- Named imports, renamed imports, namespace imports, glob imports, and
  re-export traversal.
- File-level, contract-level, function-level, and Yul scopes.
- Name resolution for top-level symbols, contract members, locals, parameters,
  state variables, struct fields, enum variants, events, errors, modifiers,
  user-defined value types, and Yul bindings.
- Cross-file inheritance traversal with cycle/diamond guards.
- Member lookup through base contracts and imported contracts.
- Basic type inference for literals, casts, calls, constructors, variables,
  member expressions, index expressions, arrays, mappings, storage references,
  and function return values.
- User-type inheritance checks for implicit convertibility.

### Navigation

- Go to definition for same-file declarations.
- Go to definition for imported top-level symbols.
- Go to definition for import path strings / import directives.
- Go to definition for namespace import members.
- Go to definition for contract/interface/library members through receiver type.
- Go to definition for inherited members, including cross-file bases.
- Go to definition for overloaded calls selected by argument types.
- Go to definition for named imports and re-exported symbols.
- Find All References across loaded workspace files.
- Find references for declarations and use sites, with optional declaration
  inclusion.
- Cross-file Rename Symbol over loaded references.
- Rename validation for Solidity identifiers and keyword rejection.
- CodeLens reference counts above supported declarations.
- CodeLens command integration with VS Code's `editor.action.showReferences`.

### Hover

- Hover shows Solidity declaration signatures in markdown code blocks.
- Hover works for same-file declarations.
- Hover works for imported and re-exported symbols.
- Hover works for members resolved through receiver types.
- Hover works for inherited members, including cross-file bases.
- Hover disambiguates overloaded calls by argument types when possible.
- Hover on named-argument keys shows the target parameter/field type.
- Hover on literals shows inferred literal type.
- Hover on synthetic builtins shows member type, including globals and value
  members.
- Hover supports `.selector`, `type(X)` members, address members, array/bytes
  members, and builtin global members.
- Hover inside inline assembly shows Yul/EVM builtin signatures and short
  descriptions.

### Completion

- Scope completion for visible locals, parameters, state variables, functions,
  modifiers, events, errors, contracts, interfaces, libraries, structs, enums,
  user-defined value types, and imported symbols.
- Contract-scope completion includes inherited members from cross-file bases.
- Completion for namespace import aliases (`import * as N`) and `N.member`.
- Member completion after `.`.
- Member completion for contract/interface instances, static type receivers,
  libraries, structs, `this`, and `super`.
- External receiver filtering: contract instances expose only public/external
  members; inherited private members are hidden.
- `using L for T` member completion, including imported libraries and elementary
  value types.
- Named-argument key completion for function calls, events, errors, structs,
  modifiers, and constructors.
- Builtin keyword completion.
- Elementary type completion.
- Global builtin completion for `msg`, `block`, `tx`, `abi`, `this`, `super`,
  `type`, and `now`.
- Builtin function completion for `require`, `assert`, `revert`, `keccak256`,
  `sha256`, `ripemd160`, `ecrecover`, `addmod`, `mulmod`, `selfdestruct`,
  `blockhash`, and `gasleft`.
- Builtin member completion for:
  - `block.basefee`, `blobbasefee`, `chainid`, `coinbase`, `difficulty`,
    `gaslimit`, `number`, `prevrandao`, and `timestamp`
  - `tx.gasprice` and `tx.origin`
  - `msg.data`, `msg.sender`, `msg.sig`, and `msg.value`
  - `abi.decode`, `encode`, `encodeCall`, `encodePacked`,
    `encodeWithSelector`, and `encodeWithSignature`
- Address member completion for `balance`, `code`, `codehash`, `call`,
  `delegatecall`, `staticcall`, `transfer`, and `send`.
- Array and `bytes` member completion for `length`; storage dynamic arrays and
  storage `bytes` also expose `push` and `pop`.
- Fixed-size array and `bytesN` member completion for `length`.
- `type(X)` member completion for integer/enum `min` and `max`, and
  contract/interface `name`, `creationCode`, `runtimeCode`, and `interfaceId`.
- `.selector` completion for functions, errors, and events.
- Inline assembly completion for Yul/EVM builtins and opcodes, with snippet
  call insertion and signature/detail metadata.
- Callable completions insert snippet parentheses and trigger signature help.
- Completion items include kind and type/detail metadata where available.

### Signature help

- Signature help on `(` and `,`.
- Positional call signature help with active parameter tracking.
- Signature help for functions, modifiers, structs, constructors, events, and
  errors.
- Overload lists for same-named functions.
- Active overload selection by argument count when possible.
- Parameter labels include both type and name when available.

### Document symbols and semantic tokens

- Nested document symbols for contracts, interfaces, libraries, functions,
  constructors, modifiers, structs, enums, events, errors, state variables, and
  user-defined value types.
- Full-document semantic tokens.
- Semantic coloring for declarations and references including contracts,
  functions, modifiers, parameters, variables, fields, enum members, events,
  errors, user-defined types, comments, literals, and keywords.
- Semantic coloring for inline assembly / Yul definitions, parameters, return
  names, paths, and function calls.

### Diagnostics

- Parser/syntax diagnostics from the lossless parser.
- Undefined Solidity identifiers used as values.
- Undefined Yul assignment targets.
- Call arity diagnostics.
- Positional and named argument type diagnostics.
- Named argument key matching for overload/type checks.
- Overload mismatch diagnostics when no overload accepts the argument types.
- Assignment type diagnostics for simple assignments.
- Local variable initializer type diagnostics.
- Integer literal range diagnostics for integer assignments and returns.
- Return value count diagnostics.
- Return type diagnostics for single return values.
- Tuple return arity diagnostics.
- Invalid address/payable casts from non-value symbols.
- Arithmetic, bitwise, and shift diagnostics for non-numeric operands.
- Comparison diagnostics for incompatible operand types.
- Ordered comparison diagnostics for unordered types.
- Condition type diagnostics for `if`, `while`, `do while`, and `for`.
- Unreachable code diagnostics after `return`, `revert`, `break`, and
  `continue`.
- Function mutability diagnostics:
  - error when `pure` reads state
  - error when `view` or `pure` writes state
  - warning when a function can conservatively be marked `view`
  - warning when a function can conservatively be marked `pure`
  - constant and immutable state reads are allowed in `pure`
  - internal/library/unknown calls are handled conservatively to avoid false
    `view`/`pure` hints
  - storage aliases and storage-returning helpers are tracked for write-through
    effects such as field/index assignment, `push`/`pop`, unary mutation, and
    `delete`
  - rebinding a storage reference itself, such as assigning a storage return
    variable, is not treated as a state write
  - inline assembly/Yul effects are modeled for `sload`, `sstore`, `tload`,
    `tstore`, logs, calls, creates, selfdestruct, environment reads, and pure
    arithmetic/memory opcodes
- Missing explicit function visibility diagnostics.
- Unused local variable diagnostics.
- Unused private/internal function diagnostics.
- Override-aware unused function diagnostics that avoid false positives when an
  override is referenced through a base declaration.
- Unused non-public state variable diagnostics.
- Unused event diagnostics.
- Unused custom error diagnostics.
- Invalid named import diagnostics when the target file does not export the
  requested symbol.
- Unused named import and namespace import diagnostics.
- Unused import suppression via
  `/// forge-lint: disable-next-line(unused-import)`.
- Missing inherited/interface implementation diagnostics.
- Diagnostics for contracts that must be marked `abstract` or implement missing
  abstract/interface functions.
- Semantic diagnostics run on syntactically clean files, on open/save and during
  budgeted background sweeps.
- Forge-std cheatcode/logging calls (`vm`, `console`, `console2`) are skipped in
  expensive type-check paths to reduce noise and latency.

### Code actions

- Quick fixes to add missing function visibility: `public`, `external`,
  `internal`, or `private`.
- Quick fix to implement missing inherited/interface functions.
- Missing implementation stubs preserve signatures, add `override` when needed,
  remove `virtual`, and insert a `revert("Not implemented");` body.
- Quick fix to mark a concrete contract `abstract` when it has unimplemented
  abstract/interface functions.

### VS Code extension

- VS Code client launches `solsp-server` over stdio for `solidity` files.
- Server path resolution order:
  - `solsp.server.path` setting
  - `SOLSP_SERVER_PATH` environment variable
  - bundled extension binary
  - workspace/repo `target/debug` or `target/release` binary
  - `solsp-server` from `PATH`
- CodeLens reference counts open VS Code's native references UI.
- The client id is `solsp`, so VS Code's `solsp.trace.server` JSON-RPC trace
  setting works.

## Build

```sh
cargo build            # builds the workspace (syntax, ide, server)
cargo test             # parser snapshots, ide unit tests, server integration
```

## Development checks

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
npm run compile --prefix editors/vscode
```
