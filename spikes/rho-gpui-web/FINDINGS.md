# GPUI-web feasibility spike

## Verdict

**Milestone 2 is not feasible with the currently vendored Zed revision as a browser UI replacement.** The Rho protocol/registry side is ready: an isolated wasm crate can link `rho-ui-proto` and `rho-registry`, construct the same `Ready` snapshot data used by the Leptos client, run the registry's workstream/tree ordering, and produce a non-empty GPUI scene using only `div` and text elements. The current GPUI-web renderer does not, however, produce visible pixels in the tested Chromium WebGPU implementations. Both `hello_web` and this Rho rail mount a correctly sized canvas, initialize BrowserWebGPU, render non-empty scenes, submit frames successfully, and remain visually black in browser and CDP captures.

This is a renderer/platform blocker below Rho's UI layer, not evidence that the shared Rho crates need another abstraction. A future gpui-web client could eliminate the duplicated DOM rendering layer once GPUI-web can visibly present frames and the transcript no longer depends on Zed's native-only editor stack. It cannot replace `webui/` today.

Milestone 3 (live iroh) was intentionally not attempted after milestone 2 hit that blocker.

## What built

### Vendored `hello_web`

`vendor/zed/crates/gpui_web/examples/hello_web` builds for `wasm32-unknown-unknown` in release mode. Its generated wasm was **9,682,323 bytes (9.24 MiB)** with the example's `data-keep-debug` and disabled wasm-opt settings.

Two minimal vendored fixes were required:

1. `wgpu` 30 added `RequestAdapterOptions::apply_limit_buckets`; `gpui_wgpu` did not initialize it. The spike sets it to `false`.
2. `WebPlatform::run` schedules asynchronous WebGPU initialization and immediately returns. The example used `Application::run`, so the application was dropped after initialization and the new canvas was immediately removed. It now uses `run_embedded` and retains the returned application handle for page lifetime.

The wasm loads without panic. With COOP/COEP, Chromium reports `crossOriginIsolated`, mounts the canvas and hidden IME input, initializes `BrowserWebGpu`, and submits frames. A software WebGPU browser was needed in this headless environment; ordinary headless Chromium exposed `navigator.gpu` but returned no adapter.

### Rho rail spike

`spikes/rho-gpui-web` is a standalone Cargo workspace. It links the vendored GPUI platform plus `rho-ui-proto`, `rho-registry`, and `rho-core`; seeds `AgentRegistry` with canned `UiWorkstream`/`UiAgentSummary` values; and renders registry-ordered workstream and agent rows with plain GPUI `div`/text elements. It imports no editor, language, or tree-sitter crates.

The generated release wasm is **7,787,778 bytes (7.43 MiB)** with wasm-opt disabled. The first clean build took about 71 seconds in this environment. The browser initialized WebGPU, mounted a 2560×1378 backing canvas plus GPUI-web's hidden input, invoked the rail's `Render` implementation, built a scene containing six quads and text sprites, and reported successful presentation. CDP and compositor screenshots remained entirely black. The same result occurred with Chromium 150 using both SwiftShader and native Vulkan/ANGLE software paths; there were no JavaScript, wasm, wgpu validation, or panic errors.

That makes layout/state integration demonstrably wasm-clean, but does not satisfy the required visible-rendering bar.

## Build workarounds

The Nix `cargo` does not include the wasm standard library. Put the rustup nightly toolchain first on `PATH`. This machine also lacked `rust-src`, which `-Z build-std` requires:

```sh
nix shell nixpkgs#rustup -c \
  rustup component add rust-src --toolchain nightly-x86_64-unknown-linux-gnu
export PATH="$HOME/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/bin:$PATH"
```

Always build release. The repository development profile selects Cranelift and fails for wasm.

Current nightly/wasm-bindgen also requires `__heap_base` to be exported for threaded module preparation. The spike includes this in `.cargo/config.toml`. The vendored example does not, so build it with:

```sh
cd vendor/zed/crates/gpui_web/examples/hello_web
export PATH="$HOME/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/bin:$PATH"
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--shared-memory -C link-arg=--max-memory=1073741824 -C link-arg=--import-memory -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base -C link-arg=--export=__heap_base'
nix shell nixpkgs#trunk -c trunk build --release
```

Build the Rho spike with:

```sh
cd spikes/rho-gpui-web
PATH="$HOME/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/bin:$PATH" \
  nix shell nixpkgs#trunk -c trunk build --release
stat -c '%s bytes %n' dist/*.wasm
```

`dist/` and `target/` are ignored and are not part of the change.

## Serving with the required headers

Trunk 0.21.14 did not emit the inline `serve.headers` from the vendored configuration in this environment, so verification used a static server that explicitly adds both headers:

```sh
cd spikes/rho-gpui-web/dist
python3 -c '
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler
class Handler(SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        super().end_headers()
ThreadingHTTPServer(("127.0.0.1", 8080), Handler).serve_forever()
'
```

Verify the response before debugging wasm:

```sh
curl -sSI http://127.0.0.1:8080/ | grep -i cross-origin
```

A Linux browser in a software-only Wayland session needed unsafe WebGPU flags to expose an adapter:

```sh
chromium --ozone-platform=wayland --enable-unsafe-webgpu \
  --enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE \
  --use-angle=vulkan --use-vulkan=native http://127.0.0.1:8080/
```

Those flags are only a headless-test workaround, not a deployment requirement for browsers with a normal GPU/WebGPU implementation.

## Rendering and performance observations

- `rho-ui-proto` and `rho-registry` compiled for wasm unchanged. The registry remains the correct source for workstream ordering, parent/child depth, attention, naming, and folding policy.
- GPUI-web creates one full-window canvas and a transparent 1×1 hidden input for IME events; it does not create a DOM node per rail row.
- The threaded dispatcher sizes its worker pool directly from `navigator.hardwareConcurrency` (96 in the test browser), which is excessive for a small rail and inflates startup work. A production web client needs a bounded pool or a single-thread configuration.
- GPUI-web currently requests animation frames continuously. Instrumentation during diagnosis observed repeated submission of the unchanged six-quad scene, so idle CPU/GPU behavior needs profiling and likely invalidation-driven scheduling before production use.
- No meaningful paint, text quality, interaction latency, or scrolling performance measurement was possible because captured output was black.
- At 7.43 MiB before compression and with wasm-opt disabled, the minimal rail is much larger than a conventional DOM client. Release wasm-opt, Brotli hosting, dependency feature trimming, and bounded font assets would be required.

## What a real port additionally needs

- **Renderer readiness:** first resolve the black presentation failure on current browsers and add browser screenshot coverage for `hello_web` so successful frame submission cannot masquerade as successful rendering.
- **Fonts:** GPUI-web currently bundles IBM Plex Sans in the renderer. Rho would need an explicit font family/fallback policy, code font, glyph coverage, licensing, preload/cache behavior, and text-quality testing across DPRs.
- **IME and input:** the hidden-input composition path exists, but production needs selection, clipboard, shortcuts, focus restoration, mobile keyboards, accessibility, screen-reader semantics, and composition tests. A canvas rail loses the DOM accessibility semantics the Leptos UI gets naturally.
- **Transcript rendering without Zed editor:** Zed editor buffers, language services, and tree-sitter cannot compile to wasm. A port needs a separate transcript model and virtualized GPUI element renderer for streaming text, markdown/code, tool calls, mail, links, copying/selection, and incremental wrapping. Reusing the native `rho-gui` transcript renderer is not currently possible.
- **Live state:** the existing Leptos iroh connection can be moved largely unchanged: `rho-ui-proto` frame IO, `rho_registry::session` subscriptions, and `rho_registry::store` folding already compile for wasm. Authentication/enrollment UI and reconnect/cancellation policy still need GPUI surfaces.
- **Threading and hosting:** shared wasm memory requires secure context plus COOP `same-origin` and COEP `require-corp`. GitHub Pages does not provide configurable response headers, so the threaded build cannot be hosted there directly. Use a host/CDN that can set headers, or make GPUI-web genuinely single-threaded (including disabling `gpui_web`'s default `multithreaded` feature and shared-memory linker settings). A service-worker COI shim adds startup and caching complexity and is not a good default.
- **Web platform basics:** responsive sizing, DPR/resize races, URL/history integration, downloads, drag/drop, clipboard permissions, accessibility, offline/cache policy, error UI, telemetry, and browser compatibility all remain.

## Strategic assessment

The experiment validates the architectural premise that a GPUI-web client can consume Rho's existing protocol and registry directly; no duplicate state/protocol layer is necessary. It does **not** validate the essential renderer premise. Until vendored GPUI-web visibly presents its own example and a plain-element Rho scene in supported browsers, replacing `webui/` would trade a working production client for an unshippable canvas. Keep the Leptos client and revisit after GPUI-web gains browser rendering tests and the native GUI's editor-dependent transcript has a wasm-capable replacement.
