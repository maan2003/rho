# ste_checker

`ste_checker` checks Markdown against the part of ASD-STE100 (Simplified Technical English)
that a program can decide: the approved wordlist, the part of speech of each word, sentence
length, noun clusters, passive voice, compound tenses, -ing forms, contractions and semicolons.
`human.send` in `rho-agent` gates on some of these rules.

Upstream is <https://github.com/valeratrades/ste_checker>, vendored as a squashed subtree. Rho
keeps only the library: the CLI, its reporting and its build script are gone, so the crate
needs no `clap`, `miette` or `color-eyre`. The `semicolon` rule is Rho's.

ASD and STEMG do not endorse this program, and it certifies nothing.

### Attribution
The wordlist in `ste_checker/vendor/openste.json` is [openSTE](https://github.com/openste/openste)
v1.01, with the MIT license in `ste_checker/vendor/LICENSE-openste`. Part-of-speech tags, the
Markdown parser and the sentence splitter come from [Harper](https://github.com/Automattic/harper),
with the Apache-2.0 license.
