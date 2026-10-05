# Roadmap

Baseline over `ste_checker/tests/corpus/`, before and after
item 5 — the inversion of the dictionary filter from a blacklist to the whitelist the standard
actually specifies:

```
                          no glossary        + starter glossary
                          before   after     before   after
 unapproved-word            44       43        35       35
 wrong-pos                  16       16         4        7
 unknown-word                0      150         0      147
 ing-verb                    9        6         9        6
 passive-voice               5        5         5        5
 sentence-length             2        2         2        2
 contraction                 2        2         2        2
 noun-cluster                0        0         0        0
 compound-tense              0        0         0        0
                           ───      ───       ───      ───
                            78      224        57      204
```

`unknown-word` is the whole of the change. The rest moves for two reasons: the glossary splits
into `names` and `verbs`, so `default`, `error` and `target` are Technical Names and their verb
readings are no longer muted along with them; and the new gerund rule in the cascade takes three
`ing-verb` findings and one `unapproved-word`.

The starter glossary now buys back only 20 of 228 findings, because its 23 words were chosen
against the *blacklist* — they are words openSTE already had rows for. A glossary written against
the whitelist is the ~90 entries `--suggest-glossary` proposes, and writing that list is the
consumer's job, not this repo's.

Measured over `ste_checker/tests/corpus.truth`, under `corpus.glossary.nix`:

```
 precision 0.961 (149/155)      recall 0.961 (149/155)
```

The six false positives are all tagger artifacts (`Flag`, `Default` and `Error` read as verbs,
`use` in `macros use inline`, `manual` in `manual fixing`, `Example config:`) — the eight of the
first labelling pass, less the two the gerund rule took, which is the whole reason that rule
shipped alone. The six misses are two
hyphenated-compound halves the immunize rule silences, two 3rd-person forms whose base the
lemmatizer cannot reach (`syncs`), and two passives with an adverb between the auxiliary and the
participle. Both numbers are printed by the test and asserted against a floor of 0.90, not a
target — a change that trades one for the other has to be visible rather than netting out.

LanguageTool + TechScribe report P=0.86 / R=0.98. That is a direction, not a benchmark: it is an
unpublished vendor number with no corpus or protocol behind it, and it is over their corpus rather
than ours.

## 0. Steal from LanguageTool — done

Two premises of the original entry did not survive contact, and are recorded here because they
constrain everything after.

**The content cannot be stolen.** `en/disambiguation.xml` is `Copyright (C) 2008 Marcin
Miłkowski` under LT's LGPL-2.1; this crate is BlueOak-1.0.0. TechScribe's STE customization is
closed and licence-keyed. Their tagset is Penn Treebank and openSTE is UPOS, so the rule bodies
would have needed translating regardless. What was taken is the *technique*: an ordered cascade,
`filter`/`immunize` actions, and marker scope — written against our own corpus.

**Retagging alone does not fix `noun-cluster`.** `iter_nominal_phrases` splits on `np_member`, a
second chunker output that the cascade does not touch, so fixing the tag would not have rejoined
the phrase. `noun_cluster` now counts maximal NOUN runs over `Tags` instead.

What shipped, in `src/tags.rs`:

- **Lemma fallback.** `DictWordMetadata::derived_from` + `Dictionary::get_word_from_id`, gated on
  the token's own form flags so a derivation (`quicker` → `quick`) cannot inherit its lemma's
  row, and checked against the glossary on the lemma as well as the surface form.
  `IrregularVerbs`/`IrregularNouns`/`get_plurals`/`get_singulars` were not needed — the measured
  misses are all affix-regular.
- **Two immunize rules.** Hyphenated-compound halves the tokenizer split (`Argument-less`,
  `1-fold`), and past participles governed by `be`, which `passive-voice` already owns. The price,
  asserted in the tests so it stays visible: every hyphenated compound is silenced, `well-known`
  included.
- **Four retagging rules** — imperative, subordinator, quantifier, noun run — in one
  left-to-right pass, one write per token, and only ever to a part of speech the curated
  dictionary already admits.
- **`equivalent()` pruned by measurement**, not by argument. Ten of eleven pairs suppressed
  nothing on the corpus once the cascade ran, but a pair is a claim about the two tagsets, not a
  corpus artifact; only `(ADP, ADV)` and `(ADV, ADP)` — particle versus preposition, which the
  cascade is the right layer for — were deleted.

Still open, each deferred for want of a second instance:

- `Example config:` — the imperative rule on a colon-terminated fragment with no finite verb.
- `macros use inline` — the noun-run rule on a plural subject followed by its own verb. Agreement
  is what LT's `grammar.xml` unification exists for.
- Sentence-boundary detection, LT's chunker, and `grammar.xml` unification (originally bullets 3,
  4 and 6). `MIN_WORDS` in `sentence.rs` is still a guess, but `sentence-length` finds 2 and
  `compound-tense` 0 on the corpus; there is no measured failure to chase yet.

## 1. `readme_fw` integration — shipped in v_flakes v1.6.80

`v_flakes.readme-fw`'s `shellHook` binstalls this crate from crates.io and runs it over
`docs/.readme_assets/{usage,install*}.md` on every dev shell entry, right after it writes
`README.md` from those same assets. Findings go to stderr; the exit code is ignored, so nothing
gates. Every `readme_fw` consumer gets it — repos pick it up as they bump their `v_flakes` pin.

Two things changed shape against the original plan:

- **Not a pre-commit hook.** `pre-commit` shows a hook's output only when the hook fails, so a
  warning-severity hook prints nothing — the one thing it exists to do. Nix prints the findings
  itself instead.
- **Config moved out of the config file.** `v_utils`' `Settings` derive prints `warning: no config
  file found` to stderr unconditionally and by design, which every consumer's shell entry would
  have carried. `v_utils` is gone; `text_type` and `disable` are plain clap flags that nix passes.

`.sh` assets are skipped — shell scripts are not prose. `.typ` waits on item 3.

## 2. LLM target — shipped as the second half of `skill/`

The `asd-ste100` skill (MIT, `danyuchn/asd-ste100-skill`) works from STE's principles and ships no
wordlist, so it composes with rather than duplicates the dictionary rules. Its rules, references
and examples are now the `# Rewrite` half of `skill/SKILL.md`, against the `# Check` half that
documents this binary. It covers what no checker can do procedurally: one-topic paragraphs,
ellipsis, list-for-sequences, topic sentences.

Nothing invokes it — an agent reads it when a finding points at a sentence, or when asked to
rewrite. A pass that runs over files which already cleared the procedural rules stays open.

## 3. `.typ` support

`harper-typst` exists; Typst assets are currently unchecked.

## 4. `--suggest-glossary` — shipped with item 5

Every `unknown-word` finding, aggregated by lemma, split into `names`/`verbs` by how the word was
tagged, ordered by frequency, annotated with the count and with whether the word also occurs
inside code in the same input. Deliberately unfiltered: the in-code signal measured 0.6/0.6
against a hand-drawn line, which is enough to order a list a human edits and not enough to
pre-filter one. A machine can propose the list; it cannot draw that line.

The flag does not read the default `docs/glossary.nix`. Its output is normally redirected over
that path, and the shell truncates the file before the process starts, so discovering it there
means parsing the empty file the run is about to write. `--glossary` still asks for one by name.

## 5. Invert the filter — done

`rules/dictionary.rs` is one `verdict()` behind three arms, and the third, `unknown-word`, fires
when neither the surface form nor the lemma has an openSTE row. `docs/glossary.nix` replaces the
word-per-line `docs/.ste_glossary` (no repo had written one), holds `names` and `verbs`
separately, and carries a `desc` the standard asks a Technical Name to have. Nothing reads `desc`
yet.

**One premise of the plan inverted on contact, and it is the interesting part.** Item 0 measured
`IrregularVerbs`/`IrregularNouns` as worth 0 and the plan closed them permanently — a correct
measurement of the *blacklist*, where an unresolved inflection simply finds no row and stays
silent. Under the whitelist an unresolved inflection is a *finding*, and `has`, `found`, `broke`,
`chosen`, `men` and `teeth` were all reported as out of vocabulary although their lemmas are
approved. 37 of 155 first-cut findings, all of them ordinary English, which is the one thing this
arm must never report. Both tables are now consulted, and what neither reaches (irregular past
participles) is silenced by a general rule: `unknown-word` only speaks about a form it can reduce
to a base.

Three smaller decisions, each visible in a test:

- **`wrong-pos` stays on the surface row.** Sharing `verdict()` briefly gave it the lemma
  fallback, which turned every gerund (`monitoring`) into a `wrong-pos`. An inflection's part of
  speech is its lemma's; a form flag that agrees with the row while the tagger does not is a
  disagreement about the form, which `ing-verb` owns.
- **The glossary matches ADJ for names.** `chat ID` tags `chat` as ADJ, and requiring a second
  declaration for the attributive reading would make the valve useless. The pair is not in
  `equivalent()`, which is about the tagsets rather than about position.
- **Suggestions collapse inflections but not derivations.** `derived_from` is both, so `browser`
  proposed itself while `messages` proposed `message`.

`contraction` gained an immunize rule, or `won't` reports twice.

**This repo had a `docs/.ste_glossary` after all** — the plan checked for consumers and missed the
crate's own. It is now `docs/glossary.nix`, extended with the vocabulary of grammar (`noun`,
`verb`, `wordlist`, `contraction`) that the standard's 900 words have no reason to carry and this
program cannot describe itself without.

**The one measured precision item.** A cascade rule for `-ing` tagged VERB after NOUN/ADJ/PROPN,
which the standard permits inside a Technical Name. It takes `LLM-based filtering`, `channel
watching` and `poll/info forwarding` — the three clear `ing-verb` false positives — and with them
`manual fixing`, which item 0 deferred as a NOUN→ADJ rule for want of a second instance. A gerund
after a noun is the same construction in both, so that is the resolution rather than collateral.
It shipped alone, after the labelling, so its effect on the number is attributable: precision
0.949 → 0.961.

**Blast radius.** 17 repos consume `readme_fw`, which runs this crate with no flags on every dev
shell entry. Each sees roughly three times the findings until it has a `docs/glossary.nix`.
Nothing gates — the exit code stays 0 — but the pin bump is a visible change across the fleet, and
the `readme_fw` shellHook should learn to print `run ste_checker --suggest-glossary` when the file
is absent. That is a `v_flakes` change, not one this repo can make.

## Not on the roadmap

**Closing the false-positive budget to zero.** The original plan expected `--deny` to exit 0 on
the corpus with a good glossary. It does not, and it should not: the residual is real ASD-STE100
debt in the text, not tool error. A glossary large enough to reach 0 would have to absorb ordinary
English, which is the one thing the glossary must never do.

**The remaining ~43 semantic rules.** They belong to item 2. This tool is a writing aid, not a
compliance certificate, and neither ASD nor STEMG endorse it.
