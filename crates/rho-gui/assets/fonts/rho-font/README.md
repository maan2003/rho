# Rho Font

Source Sans 3 0.70 of the way to Source Code Pro, set word by word by
rho's typesetter (`src/typeset.rs`). Transcripts mix
prose with paths and identifiers. Full monospace spends width on every
narrow letter, and plain sans makes code look like prose. Part of the
way between, a phone line holds more text than iA Writer Duo, which
rho shipped before, while code still reads as code.

The two families share a design, so one variable font can interpolate
between them. The build loads both families' ExtraLight, Regular and
Black instance UFOs (upright and italic) and keeps every character
both cover. It adds a `MONO` axis whose masters are the sans at 0 and
the mono at 1. A few glyphs have different point structures in the two
families, such as `i`, `l`, `g` and `0`. For those, the mono master
gets the sans outline centred in the mono advance. The mono outline
becomes a `.mono` alternate, used only from `MONO` 0.5 up. The `.mono`
alternates keep the sans advance at `MONO` 0, so every advance
interpolates. The font keeps the whole `MONO` axis, defaulting to 0.7,
with `wght` from 400 to 700. Kerning comes
from the sans, and OpenType features are dropped.

Source's x-height is smaller than Duo's, so the fonts declare 942 units
per em instead of 1000. That scales every glyph up until the x-height
matches Duo's, and existing font-size settings read as they did.

The OFL reserves "Source", "Plex" and "iA Writer", so the result carries rho's own name.

The typesetter keeps each word's shaped width and redraws its letters
inside it: each letter picks a `MONO` instance from 0.7 to 1 (`m` and
`w` from 0 to 0.4) so the optical gaps between letters come out even.
Monospaced words would line up but space unevenly; this keeps the
rhythm of a grid while narrow letters stay narrow.

Plex's slab `i`, as drawn for iA Writer Duo and scaled to 942 units per
em, sits at U+E000 in the upright font. The typesetter draws every
upright `i` with it; Source's `i` reads too thin beside its neighbours.
Its Regular and Bold match Duo's, and the axis ends are extrapolated.

Upstream (`main`): <https://github.com/adobe-fonts/source-sans> at
272b22b0, <https://github.com/adobe-fonts/source-code-pro> at 2742c359,
<https://github.com/iaolo/iA-Fonts> for the `i`.
Copyright and license notices are carried in the fonts themselves and
in `LICENSE.md`.

Two files, upright and italic, each with `MONO` and `wght` axes.
A theme can ask for a weight the font doesn't ship as a face and get it
interpolated rather than rounded (see `emphasis.strong` in the Rho
OKSolar theme). The terminal grid uses `.ZedMono`, since this face is
proportional.
