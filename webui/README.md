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

The browser and authenticator must support the WebAuthn PRF extension. Rho
requires user verification on every connection and derives a stable,
daemon-specific iroh identity without storing its secret key.

## Hosting security

Host the page on a dedicated origin with no unrelated applications or
third-party scripts. The generated page includes a restrictive CSP, but static
hosts should also send it as an HTTP header and must send
`Content-Security-Policy: frame-ancestors 'none'`; browsers do not enforce that
directive from a meta tag. A reverse proxy in front of GitHub Pages can add the
required header.

All code served by the origin is trusted with the enrolled daemon identity once
the user approves the passkey prompt. After an origin or publishing-pipeline
compromise, revoke the enrolled iroh endpoint, clear the origin's browser site
data (including service workers), verify the deployment, and enroll again.
"Use a new passkey" clears local credential metadata and creates a new browser
identity, but does not revoke the previous endpoint on the daemon.

Revoke an old identity locally with:

```sh
rho iroh revoke <endpoint-id>
```

Revocation prevents reconnection. Restart the daemon as part of compromise
recovery to terminate any connection that was already established.
