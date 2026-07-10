# Rho web UI

A static Leptos (wasm) app that connects to a rho daemon over iroh. The
built `dist/` bundle has no server-side component and can be hosted on any
static host (GitHub Pages etc.); the browser talks to the daemon directly
through an iroh relay.

## Build

This directory is its own cargo workspace (the root workspace's
`.cargo/config.toml` uses nightly-only options, so build the web UI in
release mode with a stable toolchain):

```sh
env CC_wasm32_unknown_unknown=clang \
  nix shell nixpkgs#trunk nixpkgs#rustc nixpkgs#cargo nixpkgs#lld \
    nixpkgs#llvmPackages.clang-unwrapped nixpkgs#llvm \
    -c trunk build --release
```

`CC_wasm32_unknown_unknown=clang` matters: `ring` compiles C for wasm and
silently produces unlinkable objects if a non-wasm `CC` (e.g. the dev
shell's `gcc`) leaks in.

## Use

1. Run the daemon with `rho daemon --iroh`; it prints its endpoint id.
2. Open the hosted page as `https://…/?daemon=<endpoint-id>` (remembered in
   local storage afterwards).
3. First visit shows an enrollment code; run `rho iroh approve <code>` on
   the daemon machine within a minute.
