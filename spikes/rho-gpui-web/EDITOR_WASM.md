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
| `editor` | **blocked at crate ownership split** | The parser/model stack is clean, but editor still unconditionally reaches `db -> sqlez -> smol -> polling`, `client/rpc -> tokio -> mio`, and `project -> terminal/node_runtime/git/workspace`. Native project/client types also appear directly throughout the main `Editor` implementation and element modules. |

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

### Concrete editor symbol inventory (2026-04-02)

The crate boundary has to account for Rust method ownership, not just Cargo
edges. `Editor` is defined in a 12.9k-line `editor.rs`, `EditorElement` in a
12.7k-line `element.rs`, and `DisplayMap` in a 4.6k-line module backed by about
15k lines of map layers. Portable editing and native integration methods are
currently interleaved in inherent `impl Editor` blocks. A second crate cannot
add inherent native methods to a core-owned `Editor`, so simply moving the
struct and leaving its project methods behind is not a viable incremental
split.

The concrete production-module dependencies are:

- **Portable leaf modules now:** `display_map/{block_map,crease_map,
  custom_highlights,dimensions,invisibles,row_scale_map,tab_map,wrap_map}` are
  already expressed in terms of GPUI, language, `multi_buffer`, text, and
  theme types. `selection`, `selections_collection`, and most of `movement`
  are similarly model-only. `blink_manager`, indentation rendering, and the
  scroll amount/autoscroll value types are also portable.
- **Display-map leaks to relocate:** `InlayId` is owned by `project`; folding
  ranges and semantic token types come from `project::lsp_store`; diagnostic
  severity comes from both project settings and `lsp`; and the inlay map uses
  `editor::inlays::{Inlay, InlayContent}` plus hover-highlight types. These are
  data-model types, not project services, and must move to the core boundary
  (or a lower language model crate). Display-map companion conversion also
  calls the project-oriented split module and should be injected or kept as an
  optional native extension. `block_map` calls one static spacer renderer on
  `EditorElement`; that renderer must become a block-map callback/helper.
- **Incidental native names in portable behavior:** `movement` imports only
  `workspace::searchable::Direction` (a two-way navigation enum) and project
  diagnostic settings in tests; `scroll` imports persistence plus
  `WorkspaceId`/`ItemId` even though its coordinate and autoscroll machinery
  is portable; `editor_settings` imports project diagnostic severity. These
  value types/settings hooks should be core-owned, with persistence remaining
  native.
- **Native feature modules:** bookmarks, code actions/context menus/lens,
  completions, diagnostics acquisition, document colors/links/symbols,
  folding-range acquisition, hover providers, inlay-hint acquisition,
  runnables/tasks, semantic-token acquisition, Git/blame, persistence, split
  workspace items, and the clangd/rust-analyzer extensions directly call
  project/workspace/LSP/RPC/DB/Git services and stay native. `items.rs` is the
  workspace/collaboration serialization layer and stays native.
- **Main `Editor` native state:** the struct directly stores `Project`,
  `Workspace`/`WorkspaceId`, collaboration view and collaborator IDs, project
  completion/semantics/edit-prediction delegates, code actions, debugger and
  runnable state, LSP hover/rename/signature/document-highlight state,
  Git-blame state, workspace navigation history, and serialization state.
  Project event subscriptions and workspace item registration are also in the
  main module. These fields and methods need a native extension state/delegate;
  the core struct retains the `MultiBuffer`, `DisplayMap`, selections, scroll
  manager, focus/input state, editing transactions, style/highlights, and
  rendering configuration.
- **`EditorElement` split:** basic text, cursor, selection, gutter, scrollbar,
  block, inlay, and input rendering is portable. Header/path controls,
  Git-blame/diff UI, debugger breakpoints, runnable/code-action gutters,
  project settings, workspace tab-bar behavior, hover/context menus, and
  Markdown LSP popovers are native render contributions. `element/header.rs`
  stays native; `element/mouse.rs` is portable after delegating its project
  file/AI-setting checks. The element needs a core-defined render delegate for
  optional gutter/header/popover contributions rather than project handles.

The implementation remains in the single `editor` crate, with an explicit
default-on `native` integration feature. This preserves the existing
`editor::Editor` identity and all native consumers while portable leaves and
core `Editor`/`EditorElement` behavior are separated behind cfgs. A separate
implementation crate or `editor::Editor(EditorCore)` wrapper would add
indirection (and, for the wrapper, change GPUI entity types) without helping
the inherent-impl split. The browser workspace can depend on `editor` with
default features disabled without native feature unification leaking in.

The extraction order is: relocate the four display-map model leaks and spacer
callback; move/check the complete display-map stack; move selection/movement
and portable scrolling; split `EditorElement` render contributions; finally
separate the core `Editor` fields/input methods from native feature state. Each
step must keep the facade's native check green and add a release wasm check of
the implementation crate.

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
- **Milestone 3 — reduced editor:** blocked after dependency trial. The main
  editor crate must be split into a target-neutral editor/element core plus
  native integration modules (or receive a large API-compatible project/client
  model layer). Merely target-gating Cargo dependencies is insufficient because
  project, workspace, client, DAP, database, Git, and LSP integration types are
  embedded throughout `editor.rs`, display-map extensions, and element code.
- **Milestone 4 — browser editor demo:** not started. The existing plain GPUI
  rail remains the last successful browser scene.
- **Milestone 5 — highlighting:** deferred.

### Split progress (2026-04-02)

- The complete implementation was initially moved to an `editor_core`
  implementation crate behind an `editor` compatibility facade. That
  intermediate split was subsequently collapsed back into the single `editor`
  crate because the facade provided no feature-isolation benefit. `editor`
  itself now owns the default-on `native` feature, so existing native crate
  paths, GPUI entity types, inherent methods, and behavior remain unchanged.
- The native dependency families are optional dependencies of `editor`
  activated by `native`: breadcrumbs, client, DAP, DB, edit prediction,
  feature flags, file icons, filesystem use, fuzzy matching, Git, LSP,
  Markdown, menus, project, RPC, tasks, telemetry, and workspace. This removes
  them from the no-default-features direct dependency set while preserving the
  default native graph.
- `cargo check -p editor` in the Zed workspace and `cargo check -p rho-gui` at
  the repository root pass after both changes.
- The release wasm check now reaches `editor` Rust compilation with the
  established nightly/unwrapped-clang setup. It currently reports about 300
  unresolved native references because source modules and the interleaved
  `Editor`/`EditorElement` implementation have not yet been feature-gated.
  The highest-leverage next step is module-level gating of the native feature
  list above, followed by relocating the four DisplayMap data types and then
  separating native `Editor`/element impl blocks. No browser demo is possible
  until that source split is complete.

### Stage 1 — portable display-map model types

- Moved `InlayId`, `LspFoldingRange`, and `TokenType` into `language`, where
  both native project integrations and the browser display map can use the
  same value types. `project::InlayId`,
  `project::lsp_store::LspFoldingRange`, and
  `project::lsp_store::TokenType` remain re-exports for native compatibility.
- Moved `LanguageServerId` into `language_core`; both the native `lsp` crate
  and the browser data-model-only `lsp-stub` re-export that single identity.
  This prevents semantic highlight metadata from depending on a language
  server process crate.
- Moved the editor's diagnostic display cutoff into `language` as
  `DiagnosticSeverityFilter`. The old
  `project::project_settings::DiagnosticSeverity` path remains an alias. The
  distinct name at the lower layer avoids colliding with the protocol's
  `language::DiagnosticSeverity` re-export.
- `DisplayMap` and its fold/inlay layers now consume these portable paths
  directly. Native editor behavior and public paths are unchanged.

### Stage 2 — native integration module boundary

- Put the cohesive service-backed modules behind `editor/native`: bookmarks,
  code actions/context menus/lens/completions, diagnostics acquisition,
  document colors/links/symbols, edit prediction, folding-range acquisition,
  Git/blame, header UI, hover providers/popovers, LSP inlay-hint acquisition,
  linked editing, LSP extensions, Markdown actions, mouse context menus,
  navigation/workspace integration, persistence, runnables/tasks, semantic
  token acquisition, split workspace items/views, signature help, and the
  clangd/rust-analyzer extensions.
- Kept the portable modules compiled: display-map layers, selection and
  movement, input, scrolling, folding transforms, indentation, clipboard,
  rewrap, the base inlay value/rendering model, and `EditorElement` itself.
  `element/mouse.rs` also remains portable as planned; its remaining project
  checks must be delegated or narrowly gated during the interleaved-impl
  split rather than dropping mouse selection/scroll behavior.
- The no-default-features error count fell from about 293 to 178. Remaining
  errors are concentrated in the deliberately interleaved `Editor` (203
  diagnostic locations in the first post-gate check) and `EditorElement` (27),
  plus narrow portable touchpoints in input, display map, inlays, scroll, and
  mouse handling. Those require field/impl-level separation; further
  module-level gating would remove required editing or rendering behavior.

### Stage 3 — interleaved implementation split (in progress)

- Feature-gated the native `Editor` fields and their construction/subscription
  paths while retaining the buffer, display map, selections, scroll manager,
  focus/input state, editing transactions, highlights, and render state on the
  portable struct.
- Kept keyboard deletion and insertion portable: backspace, delete, tab,
  undo/redo, and focus handling now gate only linked-editing, edit-prediction,
  hover, blame, diagnostics, and LSP refresh side effects. The actual buffer
  edits, selection updates, cursor movement, and autoscroll remain compiled.
- Moved `Direction` into `language` (with the old edit-prediction re-export)
  and moved `InlayHighlight` into the base inlay model, eliminating two more
  native type leaks from portable selection/display-map code.
- Added a wasm construction path to the existing `Editor::new`/
  `new_internal` implementation by conditionally omitting the project handle;
  the real `MultiBuffer`, `DisplayMap`, selection collection, blink manager,
  focus subscriptions, and style machinery are still initialized.
- The release wasm check is down to 76 errors. The split companion conversion
  callbacks are now native-only while the complete display-map stack remains
  portable, and movement now uses the portable `language::Direction` directly.
  Remaining diagnostic locations are concentrated in `Editor` (94),
  `EditorElement` (21), fold persistence (12), selection (9), input (8),
  element mouse handling (4), and two each in scroll, clipboard, and actions,
  plus one config setting hook. Remaining clusters are the
  native contributions interleaved in `EditorElement`/mouse, persistence hooks
  in fold/selection/scroll, split companion conversion in `DisplayMap`, and a
  smaller set of settings/event methods in `Editor` and input. No portable
  editing or painting module has been gated wholesale.

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
