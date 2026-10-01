# Rho Font

Source Sans 3 a quarter of the way to Source Code Pro. Transcripts mix
prose with paths and identifiers. Full monospace spends width on every
narrow letter, and plain sans makes code look like prose. At a quarter,
a phone line holds about a fifth more text than iA Writer Duo, which
rho shipped before, while code still reads as code.

The two families share a design, so one variable font can interpolate
between them. The build loads both families' ExtraLight, Regular and
Black instance UFOs (upright and italic) and keeps every character
both cover. It adds a `MONO` axis whose masters are the sans at 0 and
the mono at 1. A few glyphs have different point structures in the two
families, such as `i`, `l`, `g` and `0`. For those, the mono master
gets the sans outline centred in the mono advance. The mono outline
becomes a `.mono` alternate, used only from `MONO` 0.5 up. The font is
pinned at `MONO` 0.25, with `wght` kept from 400 to 700. Kerning comes
from the sans, and OpenType features are dropped.

The OFL reserves "Source", so the result carries rho's own name.

Upstream (`main`): <https://github.com/adobe-fonts/source-sans> at
272b22b0, <https://github.com/adobe-fonts/source-code-pro> at 2742c359.
Copyright and license notices are carried in the fonts themselves and
in `LICENSE.md`.

Two files, upright and italic, each with a `wght` axis from 400 to 700.
A theme can ask for a weight the font doesn't ship as a face and get it
interpolated rather than rounded (see `emphasis.strong` in the Rho
OKSolar theme). The terminal grid uses `.ZedMono`, since this face is
proportional.
