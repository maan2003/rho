# Rho Font

iA Writer Duo V, renamed. The OFL reserves the names "iA Writer" and
"Plex" for the originals, so a modified version has to carry its own
name — and renaming is the only modification here: the outlines,
metrics, `wght`/`SPCG` axes and named instances are the upstream files
byte for byte, with only name IDs 1-6 and 10 rewritten.

Upstream: <https://github.com/iaolo/iA-Fonts>, `iA Writer Duo/Variable`.
Copyright and license notices are carried in the fonts themselves and in
`LICENSE.md`.

Two files, upright and italic, each with a `wght` axis from 400 to 700 —
so a theme can ask for a weight the font does not ship as a face and get
it interpolated rather than rounded (see `emphasis.strong` in the Rho
OKSolar theme).
