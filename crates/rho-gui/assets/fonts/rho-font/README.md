# Rho Font

Source Sans 3 0.6 of the way to Source Code Pro. Transcripts mix
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
interpolates. The font is pinned at `MONO` 0.6, with `wght` kept from
400 to 700, so plain shaping sets it. `m` and `w`, and the letters built
on them, keep their sans form. Kerning comes from the sans and reaches
every form, and OpenType features are dropped.

Source's x-height is smaller than Duo's, so the fonts declare 942 units
per em instead of 1000. That scales every glyph up until the x-height
matches Duo's, and existing font-size settings read as they did.

The OFL reserves "Source", "Plex" and "iA Writer", so the result carries rho's own name.

Every glyph that differs from Source Sans (the mono master, the `.mono`
forms and Duo's `i`) takes the spacing of its Source Sans twin at the
same weight, the way a designer sets sidebearings. Each side measures
where its ink reaches farthest and the mean white behind that, up to 0.1
em, across the x-height for lowercase and the glyph's own height
otherwise, and gets its twin's measure.

Plex's slab `i`, as drawn for iA Writer Duo and scaled to 942 units per
em, is the upright font's `i`; Source's `i` reads too thin beside its
neighbours.
Its Regular and Bold match Duo's, and the axis ends are extrapolated.

Upstream (`main`): <https://github.com/adobe-fonts/source-sans> at
272b22b0, <https://github.com/adobe-fonts/source-code-pro> at 2742c359,
<https://github.com/iaolo/iA-Fonts> for the `i`.
Copyright and license notices are carried in the fonts themselves and
in `LICENSE.md`.

Two files, upright and italic, each with a `wght` axis from 400 to 700.
A theme can ask for a weight the font doesn't ship as a face and get it
interpolated rather than rounded (see `emphasis.strong` in the Rho
OKSolar theme). The terminal grid uses `.ZedMono`, since this face is
proportional.
