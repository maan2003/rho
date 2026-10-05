# corpus

Verbatim `usage*.md` and `installation*.md` from `~/s/*/docs/.readme_assets/`, plus the
`*__architecture.md` and `*__methodology.md` pair carrying most of the English here — the text
this checker was calibrated against, in the shape it runs on: prose mixed with code fences,
tables and headings, written before the checker existed.

The `.readme_assets` half is thin on prose: 494 words over 13 files, which left `noun-cluster`
and `compound-tense` at zero and every count too small to move. The two longer documents are
what make each rule report a number a regression can shift.

It exists so a rule change moves a visible number. `../integration/main.rs` asserts a finding
count per rule over these files; the wordset, the POS-equivalence table and every rule feed those
counts. Read a move as a regression signal, not as a score to drive down — never edit a file here
to make a finding go away.

Two siblings read the same text:

- `../corpus.glossary.nix` — the Technical Names and Verbs these projects own, for the run that
  measures what a glossary buys back.
- `../corpus.truth` — hand-labelled spans over a subset, for precision and recall.

Adding or dropping a file trips the size assert, and recalibrating is then a deliberate step.
