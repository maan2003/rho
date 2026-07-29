# Zed editor on `wasm32-unknown-unknown`

## Goal and build contract

The browser editor is a remote-buffer client. It must retain Zed's GPUI editor
element, text model, cursor/selection behavior, keyboard editing, and scrolling,
but it does not need local files, Git, language servers, DAP, Node, persistence,
or a native workspace/project. Those integrations should be absent from the
wasm dependency graph rather than emulated.

All checks below used the rustup nightly toolchain first on `PATH`,
`--target wasm32-unknown-unknown --release`, and the threaded wasm flags in
[the spike config](.cargo/config.toml). Development builds are not supported
because the repository selects Cranelift. A browser build requires COOP/COEP;
see [FINDINGS.md](FINDINGS.md).

## Dependency map (2026-04-02)

Direct checks from `vendor/zed` establish the current boundary:

| Crate | Wasm status | First blocker / action |
| --- | --- | --- |
| `sum_tree`, `clock`, `collections` | **clean** | No changes required. |
| `util` | **clean after gating** | Its process, command, shell, archive, and filesystem modules were compiled on wasm even though their `smol`, `async-fs`, `dirs`, `which`, and `libc` dependencies are native-only. Compile those modules only off wasm; use `/` as the inert wasm home path. |
| `rope`, `text` | **clean after the util gate** | Their production use of `util` is target-neutral (`debug_panic`, UTF-8 helpers, range helpers). |
| `language_core` | **clean with real tree-sitter** | tree-sitter 0.26.9 C runtime cross-compiles with unwrapped clang and tree-sitter-language 0.1.7's wasm libc provider. The Wasmtime grammar-loader feature is disabled on the browser target. |
| `fs` | **interface/model layer clean** | Wasm retains `Fs`, metadata, events, and `MTime`, but excludes `RealFs`, Git, process execution, native watching, archive extraction, and trash integration. |
| `language` | **clean** | Browser builds use the real parser/query stack, omit dynamic Wasmtime grammar loading, and substitute an LSP data-model-only crate for the native language-server process runtime. |
| `buffer_diff`, `multi_buffer` | **clean** | Both compile with the browser language stack and real tree-sitter node/tree types. |
| `editor` | **not wasm-shaped** | All native integrations are unconditional dependencies and imports. The first compile failure is currently tree-sitter, before the later native failures become visible. |

The tree-sitter `wasm` Cargo feature is not the browser runtime feature. In
this revision it enables a Wasmtime-backed host for dynamically loaded grammar
modules and pulls Wasmtime/Cranelift. Browser builds instead compile the real
C runtime directly into the client wasm. Updating `tree-sitter-language` from
0.1.5 to 0.1.7 supplies the minimal wasm libc sources/headers that tree-sitter
0.26.9's build script expects. An unwrapped clang is required: Nix's clang
wrapper injects `-fzero-call-used-regs=used-gpr`, which wasm rejects.

Static grammar linking is the proven shortest path: `tree-sitter-json`'s
`parser.c` cross-compiles when given the same minimal headers. Dynamic grammar
loading remains possible later, but is unnecessary for the demo and its
Wasmtime-based native implementation must stay out of the browser bundle.

### Native editor dependency families to sever

`cargo tree -p editor --target wasm32-unknown-unknown` shows these families
still reachable:

- **Local machine/project:** `editor -> fs`, `git`, `project`,
  `workspace`, `breadcrumbs`, `client`, and `worktree`. These bring
  directory/file APIs, process execution, project RPC, and native task
  executors. They should be target-gated out; a browser editor should receive
  a preconstructed remote buffer.
- **Language tooling:** `editor -> lsp`, `dap -> node_runtime`, project
  formatters/prettier, and language registry filesystem/HTTP loading. Remove
  these from the wasm graph. Keep only target-neutral buffer/display types.
- **Persistence:** `editor -> db -> sqlez -> libsqlite3-sys`. Session and
  workspace persistence are native-only and should not be part of the browser
  element.
- **Network/runtime:** the client/project side reaches `tokio`, `smol`,
  async sockets, TLS, and `aws-lc-sys`. Browser remote-buffer transport
  belongs to Rho's web protocol layer, not inside the editor crate.
- **Parsing:** browser builds retain the real tree-sitter runtime and statically
  linked grammar objects, but exclude the Wasmtime grammar loader. Native keeps
  Zed's dynamic extension-grammar support unchanged.

Git itself is Zed's abstraction crate in this graph rather than a direct
`libgit2-sys` edge, but it still owns native askpass, filesystem, process,
and repository behavior and must be absent on wasm.

## Severance plan

1. **Finish the portable text/model seam.**
   - Keep the wasm-only `util` module gates.
   - Keep the real tree-sitter runtime, query/syntax-map facilities, and one
     statically linked proof grammar. Disable only Wasmtime-based dynamic
     grammar loading on the browser target.
   - Move `fs`, `http_client`, `lsp`, `rpc`, and toolchain/registry
     loading behind the native/default feature or target cfg.
   - Build `language`, `buffer_diff`, and `multi_buffer` for wasm before
     touching editor UI code.

2. **Define a reduced editor target.**
   - Make editor's local-machine dependencies target-specific: `client`,
     `dap`, `db`, `fs`, `git`, `lsp`, `project`, `workspace`,
     telemetry, and edit-prediction integrations.
   - Apply matching `cfg(not(target_family = "wasm"))` gates to their modules,
     actions, registrations, and workspace-item implementation.
   - Retain the core `Editor` view, `EditorElement`, display map, movement,
     selection, input handling, scrollbar, theme/settings reads, and
     `MultiBuffer` ownership. Introduce no local-I/O facade for wasm.

3. **Mount the browser demo.**
   - Construct a canned singleton `MultiBuffer`/buffer entirely in memory,
     create the reduced `Editor` view, focus it, and mount it beside or in
     place of the current rail.
   - Verify text and cursor primitives occur in scene logs, key input mutates
     the buffer, wheel input changes scroll state, and frames submit without
     browser/wasm/wgpu errors. Black software-renderer captures are not a
     failure criterion.
   - Record release wasm byte size from `dist/*.wasm`.

4. **Connect remote buffers later.**
   - Rho's daemon/web protocol should own snapshots and edits. The browser
     adapter applies remote edits to the in-memory model and sends local edit
     operations back with revision identity. Reconnect/rebase, presence, and
     conflict policy are still unspecified and are required for a usable
     remote editor.

5. **Highlighting.**
   - Register statically linked browser grammars at startup and use Zed's
     existing query/syntax-map path. Start with JSON; bundle size is currently
     secondary to proving editor behavior.

## Milestone status

- **Milestone 1 — dependency map and plan:** complete.
- **Milestone 2 — core text stack:** complete. `sum_tree`, `clock`,
  `collections`, `util`, `rope`, `text`, `language_core`, `language`,
  `buffer_diff`, and `multi_buffer` check successfully. Real tree-sitter types
  and parsing remain present; local filesystem implementations and LSP process
  execution are absent.
- **Milestone 3 — reduced editor:** not started; it depends on the remaining
  language/multibuffer dependency severance.
- **Milestone 4 — browser editor demo:** not started. The existing plain GPUI
  rail remains the last successful browser scene.
- **Milestone 5 — highlighting:** deferred.

### Reproduction

```sh
cd vendor/zed
export PATH="$HOME/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/bin:$PATH"
cargo check -p sum_tree --target wasm32-unknown-unknown --release
cargo check -p rope --target wasm32-unknown-unknown --release
cargo check -p text --target wasm32-unknown-unknown --release
CC_wasm32_unknown_unknown=/path/to/unwrapped/clang \
  cargo check -p language_core --target wasm32-unknown-unknown --release
CFLAGS_wasm32_unknown_unknown=-I/path/to/tree_sitter_wasm/include \
CC_wasm32_unknown_unknown=/path/to/unwrapped/clang \
  cargo check -p tree-sitter-json --target wasm32-unknown-unknown --release
```

The standalone Zed lockfile was stale relative to the already-vendored wgpu 30
manifest and is rewritten by current Cargo during these commands; that
unrelated resolver update is not part of this editor plan.
