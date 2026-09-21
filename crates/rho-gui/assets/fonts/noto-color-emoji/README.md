# Noto Color Emoji

Rho bundles the unmodified `NotoColorEmoji.ttf` from the authoritative
[googlefonts/noto-emoji](https://github.com/googlefonts/noto-emoji) release
**v2.051** (Unicode 17.0 update), peeled commit
`8998f5dd683424a73e2314a8c1f1e359c19e8742`.

Source:

- `fonts/NotoColorEmoji.ttf`
- <https://github.com/googlefonts/noto-emoji/raw/8998f5dd683424a73e2314a8c1f1e359c19e8742/fonts/NotoColorEmoji.ttf>
- SHA-256: `72a635cb3d2f3524c51620cdde406b217204e8a6a06c6a096ff8ed4b5fd6e27b`
- Size: 10,673,480 bytes

The font supplies standard color emoji, including skin-tone variants and
ZWJ sequences, when the host has no emoji font installed. GPUI's cosmic-text
backend recognizes its `NotoColorEmoji` PostScript name and selects it through
normal script fallback; it does not need to be named in Rho's settings.

The font is large because it contains CBDT bitmap strikes. Bundling it adds
about 10.2 MiB before package compression, in exchange for deterministic emoji
coverage on minimal systems.

Copyright 2022 Google Inc. Licensed under the SIL Open Font License 1.1.
The upstream license is reproduced verbatim in `LICENSE`.
