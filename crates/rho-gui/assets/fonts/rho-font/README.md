# Rho Font

Source Sans 3 interpolated toward Source Code Pro, set line by line by
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
interpolates. The font keeps the whole `MONO` axis, defaulting to 0.6,
with `wght` from 400 to 700. Kerning comes
from the sans, and OpenType features are dropped.

Source's x-height is smaller than Duo's, so the fonts declare 942 units
per em instead of 1000. That scales every glyph up until the x-height
matches Duo's, and existing font-size settings read as they did.

The OFL reserves "Source", "Plex" and "iA Writer", so the result carries rho's own name.

The typesetter sets every line of Rho Font itself. Each letter side is
spaced by its own ink, the way a designer sets sidebearings, to have the
white of an `n`'s side, so a word's width comes from its letters, not
from a grid. When the editor
wraps a paragraph, the typesetter chooses the breaks and each line's
`MONO` value between 0.5 and 0.7 together, keeping the right edge even
(Knuth–Plass, with `MONO` as font expansion). `m` and `w` take their
sans form.

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
