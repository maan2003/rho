# Architecture

```mermaid
flowchart TD
    MD["Markdown file"]
    DOC["harper_core::Document<br/>(markdown, default curated dict)"]
    UNL["code fences, inline code<br/>→ TokenKind::Unlintable"]
    POS["Brill tagger → UPOS per token"]
    CHK["chunker → sentences"]
    TAG["tags.rs: ordered cascade<br/>Tags { pos, immune, headings }"]
    CTX["Ctx { wordset, glossary, config }"]
    GLO["glossary.nix<br/>names · verbs"]
    DICT["dictionary rules<br/>unapproved-word · wrong-pos<br/>unknown-word"]
    STRUCT["structural rules<br/>sentence-length · noun-cluster<br/>passive · compound-tense · ing-verb · contraction"]
    OUT["Vec&lt;Finding&gt;  (rule + harper Lint)"]
    REP["report: miette (human) | serde_json (json)"]

    MD --> DOC
    DOC --> UNL
    DOC --> POS
    DOC --> CHK
    POS --> TAG
    TAG --> DICT
    TAG --> STRUCT
    CHK --> STRUCT
    GLO --> CTX
    CTX --> DICT
    CTX --> STRUCT
    DICT --> OUT
    STRUCT --> OUT
    OUT --> REP
```

Where this is going, and the measured gaps that decide the order: [ROADMAP.md](./ROADMAP.md).

## Bird's eye view

ASD-STE100 restricts English to one meaning per word, one part of speech per word, short
sentences and the active voice. Roughly ten of its 53 rules are decidable from a
part-of-speech tag and a wordlist; the rest need a reader. This crate implements the ten.

**The wordlist is a whitelist.** The standard approves ~900 words plus the Technical Names and
Technical Verbs the project declares, and rejects everything else. openSTE ships both halves —
909 approved rows and 1042 "avoid" rows — and the third dictionary rule, `unknown-word`, fires on
the words that are in neither. That arm is why `docs/glossary.nix` is load-bearing rather than a
nicety: without one, a software repo's own vocabulary is 30% of its content words.

Everything upstream of the rules is Harper's: it parses the Markdown (so code fences never
reach a rule), tags every word with a Universal Dependencies POS, and splits sentences. Harper's
`UPOS` is the same tagset openSTE annotates its wordlist with, so there is no mapping layer —
only an equivalence table for the places where the standard is coarser than UD, and a cascade
(`tags.rs`) over the places where the tagger is simply wrong.

## Code map

### `wordset.rs`
Loads the vendored openSTE wordlist once, into `HashMap<word, (approved, POS)>` plus a
replacement table. **Each row restricts one (word, part of speech) pair, not the word.**
`work/VERB/unapproved` means *work* is a fine noun and a forbidden verb; matching without
the tag flags both, which is the failure this whole design exists to avoid.

`equivalent()` is the bridge between UD's tagset and the standard's: modals are verbs,
possessives are adjectives, particles and subordinators are not separate categories. It is not
what `tags.rs` replaces — the cascade normalises the tag a rule *sees*, this maps *categories*,
and rule 4 of the cascade depends on `(SCONJ, CCONJ)` surviving. Nine pairs remain; `(ADP, ADV)`
and `(ADV, ADP)` were deleted once measurement showed they suppressed nothing.

### `glossary.rs`
`docs/glossary.nix`, and a strict recursive-descent parser over the data subset of Nix that holds
it: attribute set, identifier keys, lists, double-quoted strings, `#` comments. Anything else —
`let`, `import`, interpolation, a function — is an error, not a partial parse. Nix rather than a
word-per-line list because the standard asks a Technical Name to be *defined* once; hand-written
rather than `rnix` (a full CST parser) or `nix eval` (~200 ms against a 100 µs lint, and absent on
a `cargo binstall` machine).

`names` and `verbs` are separate maps, and a word is muted only in the part of speech it was
declared in. A flat list mutes `list`, `pass` and `share` as verbs too, which is the reading STE
rejects. `names` also covers ADJ — an attributive noun (`chat ID`) is tagged ADJ, and the standard
has no separate category for one. That pair is deliberately not in `equivalent()`, which is a
claim about the two tagsets rather than about position.

### `tags.rs`
An ordered, cascading disambiguator after LanguageTool's `en/disambiguation.xml`. Harper's tokens
are immutable from outside its crate, so `Tags` is a side table indexed by `doc.get_tokens()`,
and the rules read it instead of `pos_tag`.

`immune` is LT's `action="immunize"` — hyphenated-compound halves the tokenizer split, past
participles `passive-voice` already owns, and contractions `contraction` already owns (neither
half of `won't` is a word openSTE has a row for). The dictionary rules honour it; `prose_words()` does
not filter on it, because `aux_pairs` is positional and dropping tokens would invent pairs.

Five retagging rules, in order: imperative, subordinator, quantifier, noun run, gerund. One
left-to-right pass, at most one write per token, and a rule may only move a token to a part of
speech the curated dictionary already admits for it. No fixpoint: two rules that need each
other's output would be a bug in the pair, not a reason for a loop.

### `rules/`
Each rule is `fn(&Document, &Tags, &Ctx) -> Vec<Lint>`, collected in `RULES`. Not `impl Linter`:
that trait takes only `&mut self, &Document`, and every rule here needs the glossary.
`Lint` itself is reused from Harper for its span, message and suggestion machinery.

`prose_words()` is the shared entry point — `(index, token)` for word tokens outside headings.
Section titles are not sentences, and in the README case `readme_fw` generates them.

`noun_cluster` counts maximal runs of NOUN over `Tags`, not `iter_nominal_phrases`: the chunker's
`np_member` is a second tagger output, broken by the same errors and not rewritten by the cascade.

### `tests/fuzz/`
A deterministic metamorphic fuzzer on `v_utils::fuzz` — the same FRNG/minimizer/corpus kernel
`trading_data` and `dockviewers_core` sit on, so a harness here is a table of targets
(`main.rs`), the generators they draw from (`draw.rs`) and one `CORPUS.txt` of minimized repros.

There is no oracle for *is this finding right*: that judgement lives in the corpus counts and the
precision/recall floor of `tests/integration/`. What a fuzzer can decide is everything claimed
**around** a finding, so every target is either an invariance or a structural claim — spans index
the source in both units, one text reads the same way twice, a fenced document is silent, the
glossary and `--text-type` only ever subtract, a suggested skeleton parses and absorbs what it was
written from, and a glossary file round-trips while a damaged one is rejected rather than
half-read. `FUZZ_SEED` / `FUZZ_SIZE` / `FUZZ_TARGET` / `FUZZ_RUNS` replay and widen.

### `report.rs`
Harper spans are **char** indices into the original source; miette and editors want
**bytes**. One `ByteOffsets` table per file converts them, with an assertion that the result
lands on a character boundary.

## Invariants

- **The tool is an aid, not a gate.** Findings are warnings and the exit code is 0 unless
  `--deny`. The cascade narrows the tagger's errors but cannot close them; a checker that cannot
  be wrong would have to be far weaker.
- **Spans index the original text**, never a rendered form, so an editor can act on them.
- **No fallback on a bad wordset.** `wordset.rs` asserts on load that the vendored file still
  has its shape; if upstream changes, the process dies rather than checking against a
  half-loaded dictionary.
- **The glossary is part of the design, not a workaround.** ASD-STE100 provides for a
  per-project list of Technical Names and Technical Verbs. It is the only pressure valve the
  whitelist has, and `--suggest-glossary` exists so the first one is not written by hand.
- **A row in the wrong part of speech is not an absent row.** openSTE carries exactly one row per
  word, and `work/VERB/unapproved` is how it says *work is fine as a noun*. `unknown-word` fires
  only when there is no row for the word or its lemma; anything weaker breaks
  `part_of_speech_decides_approval` and flags the function words.
- **`unknown-word` only speaks about a form it can reduce.** The whitelist arm is exactly as good
  as the lemmatizer under it, so an inflection with no reachable base (`chosen`, `gone`) is
  silent rather than reported.
- **The corpus counts in `ste_checker/tests/integration/main.rs` are a snapshot, not a target.** They
  exist so that a change to the wordset, the equivalence table or a rule moves a visible
  number. Do not adjust a rule to make them go down.

## Cost

Model load ≈600 ms once per process; lint ≈100 µs per file; wordset load ≈2.6 ms. Pass every
file to one invocation.
