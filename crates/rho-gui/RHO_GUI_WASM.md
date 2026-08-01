# rho-gui on `wasm32-unknown-unknown`

## Stage 1 build contract

`rho-gui` keeps a default-on `native` feature. Native builds retain the existing
binary and all daemon, local-machine, audio, terminal, and shell behavior. A
release `wasm32-unknown-unknown` build with default features disabled retains
the dashboard, transcript model and editor-backed transcript preview, portable
rendering/state code, and the shared registry/store. Browser connection source
porting may follow the view split, but the transport design remains direct iroh
on both targets: do not introduce a connection trait/enum abstraction.

The wasm toolchain is the flake-pinned nightly toolchain and release profile. The
tree-sitter C runtime and statically linked Markdown grammars use the wasm libc
provider and an unwrapped clang.

## Full source inventory before the split

| Source | Classification | Boundary and coupled consumers |
| --- | --- | --- |
| `main.rs` | native entry point | CLI, tracing subscriber, profiling files, rustls provider, Wayland app boot, daemon attachment, native initialization. Its module declarations must move to `lib.rs` so the library is the source of truth. |
| `connection.rs` | mixed, port after views | Tokio/`gpui_tokio`, Unix channels and native RPC features are native; iroh itself is the browser transport too. Its channel types are consumed throughout `workspace.rs`, `zed_remote.rs`, `terminal_view.rs`, `shell_view.rs`, and `realtime_client.rs`; those payload paths must be gated together until the direct iroh wasm source path is enabled. Keep the browser connection direct, not behind a new abstraction. |
| `zed_remote.rs` | connection-coupled | Remote buffer/project synchronization and language registry setup are driven by the connection. Markdown's portable static grammar registration must not depend on this module. |
| `realtime_client.rs` | portable orchestration | RTC lifecycle and the dedicated daemon channel are shared. `rho-rtc` supplies target-specific native or browser media. |
| `terminal_view.rs`, `shell_view.rs` | native | Long-lived daemon channels and terminal/shell payloads. Workspace surface variants/actions that construct or operate these views are matching consumers. |
| `commands.rs` | native integration | Project-backed completion provider uses `project`. Generic candidate/token matching needed by the minibuffer can remain portable or move to its portable owner. Draft/agent editor completion setup is the consumer. |
| `diff_view.rs` | native integration | Remote project diff/save behavior depends on `zed_remote`; workspace diff surface and actions are its consumers. Pure diff preparation is portable in principle but is not needed by the stage-1 dashboard/transcript client. |
| `rho_assets.rs` | portable | Embedded fonts/themes/settings implement GPUI's target-neutral `AssetSource`. |
| `render/mod.rs`, `render/elision.rs` | portable | Pure protocol-to-text/style rendering and elision plans. |
| `render/markdown.rs` | portable after seam move | Static `tree-sitter-md` languages and theme syntax lookup are portable. Replace its call into connection-coupled `zed_remote` with a library-owned/static registry path. |
| `style.rs`, `highlights.rs` | portable | Editor display blocks/highlights over `MultiBuffer`; no local machine service. |
| `transcript/` | portable | Transcript model, incremental registry-store frames, editor excerpts, blocks, elisions, and inlays. `VisualizationClient` is currently a connection payload and must be absent or directly connection-backed when that code is enabled. `project::InlayId` imports are stale model imports and should use its portable owner in `language`. |
| `visualization.rs` | portable view, connection-coupled fetch | GPUI image view is portable; byte fetching currently names the connection client and stays coupled to direct connection support. |
| `dashboard.rs` | portable | Registry ordering and editor-backed dashboard/rail UI. It currently takes the monolithic `Workspace` context and uses stale `project::InlayId`; the view logic itself has no native I/O. |
| `agent_view.rs` | portable transcript preview | Agent transcript/draft editors and transcript model. Native workspace completion and visualization handles must be omitted when their direct dependencies are disabled rather than gating the preview. |
| `draft_view.rs` | portable editor view with native completion hook | Editor buffer/inlays/highlights are portable. Project completion is the only native integration and may be omitted when `native` is disabled. |
| `editor_config.rs` | portable | Target-neutral editor behavior configuration. |
| `pane.rs` | portable | Pure pane tree/surface-key model. |
| `minibuffer.rs` | portable view | GPUI editor and candidate UI are portable. It currently binds handlers to monolithic `Workspace`; retain it once the portable workspace shell exists. |
| `transient.rs` | portable view | Menus/charts and workspace callbacks are target-neutral; individual callbacks which invoke native-only workspace operations must be gated with those operations. |
| `workspace.rs` | mixed; split in place | `Workspace` is the GPUI root and portable dashboard/pane/minibuffer/transient composition, but directly stores `Connection`, `RemoteProject`, terminal/shell/diff/realtime surfaces and tasks. Native fields, `SurfaceView` variants, constructors, event/channel handlers, key actions, and render arms must all be gated as producer/consumer groups. The wasm root may start disconnected until its direct iroh path is enabled. |
| `tests.rs` and inline test modules | native test graph unless individually portable | Existing native behavior tests remain under native/default builds. Portable unit tests can be enabled separately after the production graph checks. |

## Cargo dependency families

Native-only feature members: `clap`, `dirs`, `gpui_tokio`, `fs`, `project`,
`node_runtime`, `languages/load-grammars`, `rho-iroh-auth`, `rho-profiling`,
`rho-rtc`, `rho-rpc/native-client`, `rustls`, `rodio`, `search`,
`command_palette`, `tokio`, `tracing-subscriber`, `vim`, `vim_mode_setting`,
`prefix-id/redb`, and Wayland/font-kit GPUI platform features. `connection.rs`,
native views, and `zed_remote.rs` are the source owners of those dependencies.
The wasm iroh/rho-rpc selection belongs in `rho-gui` itself; it is not a new
transport layer.

Portable direct families: GPUI without a native platform feature, `editor`
without its default `native` feature, language/text/multi-buffer/buffer-diff,
theme/settings/ui/assets, tree-sitter and the two static Markdown grammars,
`rho-core`, `rho-registry`, `rho-ui-proto`, futures, serde, and pure utility
crates. Native feature forwarding must re-enable `editor/native` (and any
equivalent downstream defaults) so existing native behavior does not change.

## Execution log / handoff

- Inventory written before source or Cargo gating.
- Browser connection target is direct iroh; no transport
  abstraction is introduced.
- Browser realtime opens a dedicated stream on that already-authenticated iroh
  connection. Provider media remains direct between the browser WebRTC peer and
  ChatGPT; the shared Iris driver alone forwards typed delegation state through
  the daemon.
- Added a library target and made the unchanged native binary require the
  default-on `native` feature. Actions and view modules now belong to the
  library; the binary remains the native application/bootstrap owner.
- The `native` feature owns native editor integration, Wayland/font-kit,
  project/language loading, Tokio, native RPC, terminal/shell, audio/realtime,
  profiling, CLI, persistence, and native command/search/Vim dependencies.
- The no-default-features library retains the dashboard, agent transcript
  preview, draft/editor configuration, render/Markdown, transcript,
  highlights/style, minibuffer, pane model, assets, registry/store aliases,
  and a disconnected browser `Workspace` root that mounts the real dashboard.
  Stage 2 will populate this root using direct iroh; it is deliberately not a
  transport abstraction.
- Moved portable inlay model imports to `language::InlayId` and exported the
  already-portable `InlayHighlight` model from `editor` instead of reaching
  through editor's native-only hover-link service module.
- Static Markdown language registration now has a wasm-local
  `LanguageRegistry`; native keeps the full `zed_remote` registry and dynamic
  language initialization.
- `tree-sitter-md` additionally needs `<wchar.h>`, `isdigit`, and `strcmp`.
  Extended Zed's existing minimal `tooling/tree_sitter_wasm/include` libc
  provider with ASCII implementations; no grammar or parser behavior is
  stubbed out.
- Verification completed:
  - `cargo check -p rho-gui`
  - `cargo check -p vim -p editor`
  - `PATH="$HOME/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/bin:$PATH" \
    CC_wasm32_unknown_unknown=/nix/store/482i7k840pk0cmv6qhf4rf5gyah1lq8l-clang-21.1.8/bin/clang \
    CFLAGS_wasm32_unknown_unknown=-I/home/maan2003/src/rho/vendor/zed/tooling/tree_sitter_wasm/include \
    cargo check -p rho-gui --release --target wasm32-unknown-unknown --no-default-features`

## Stage 2 browser application

`crates/rho-gui-web` is the thin Trunk entry point. It boots `gpui_web`, loads
Rho's assets and editor settings, and mounts `rho_gui::workspace::Workspace`.
GPUI retains its multithreaded dispatcher. Its CSP-safe local `wasm_thread`
bootstrap uses module workers and the Trunk-emitted module-preload shim URL
without JavaScript `eval`; COOP/COEP remain required for shared wasm memory.
The canonical `Workspace` now lives in `workspace.rs` on both targets and owns
the registry, store, subscriptions, model cache, and lifecycle transitions.
Its browser child contains only browser transport/chrome, touch layout, and
render state. The browser uses direct iroh (ALPN `rho/ui/3`) with the same
WebAuthn PRF identity/enrollment flow and feeds frames into the same workspace
state and `AgentModel` synchronization path as native.

Build the static bundle (release is required) with:

```sh
nix build .#webui
```

Open `dist/index.html` through an HTTP server with the COOP/COEP headers from
`Trunk.toml`, using `#daemon=<daemon-endpoint-id>`. Click the unlock prompt to
perform WebAuthn. If enrollment is required, approve the displayed code with
`rho iroh approve <code>` and reload.

## Multi-host connection integration

`hosts::Hosts` and the target-selected concrete `connection::Connection` are
now shared by native and wasm. The browser still dials iroh directly; there is
no connection trait or transport enum. `daemon_targets_from_page` accepts
repeated query or fragment values. Plain endpoint ids receive generated names,
while `daemon=<name>@<endpoint-id>` supplies a stable display name. The list is
remembered in local storage, with the old single-daemon key retained as a
fallback.

Attaching a browser host deliberately leaves it locked and emits
`AuthorizationRequired`. UI code must call `Hosts::authorize(host_id)` from a
user gesture for that one host; it must not batch WebAuthn prompts. Enrollment,
status, control messages, focused-agent stream priority, and agent frames are
tagged with `HostId`. Terminal, shell, and realtime attachments each open a
dedicated stream on the selected host connection. Their `ChannelTask` uses a
browser-local abortable pump on wasm, so dropping the owning model cancels both
directions just as dropping the native Tokio-backed owner does.
