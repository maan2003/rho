---
name: ste_checker
description: "ASD-STE100 for Markdown prose: what ste_checker reports and what to do about a finding, and how to rewrite text that does not comply. Read when a dev shell or CI prints findings, before writing docs/glossary.nix, and when asked to simplify or rewrite prose."
---

Two halves. `# Check` is the procedural pass — the ten rules a wordlist and a part-of-speech tag
decide, which `ste_checker` runs for you. `# Rewrite` is the rest of the standard, which needs a
reader; it is what you do to the sentence a finding points at, and what nothing gates.

# Check

`ste_checker` checks Markdown against the procedurally decidable part of ASD-STE100: the approved
wordlist, part of speech, sentence length, noun clusters, passive voice, compound tenses, `-ing`
forms and contractions. The other rules of the standard are semantic — see `# Rewrite`. Findings
are warnings and the exit code is 0 unless `--deny`; `--format json` for a machine.

The standard is a whitelist of approximately 900 words, so a word your project owns is reported
until you declare it. Declare it in `docs/glossary.nix` as a Technical Name (approved as a noun)
or a Technical Verb (approved as a verb); declaring one does not declare the other. Write the
first file with:

```sh
ste_checker --suggest-glossary docs/.readme_assets/*.md > docs/glossary.nix
```

which lists every word outside the vocabulary and reads no glossary of its own.

Then edit it down. **Ordinary English never belongs in the glossary** — a finding on an ordinary
word is asking for the sentence to be written again, and absorbing the word instead disables the
whitelist one word at a time. Zero findings is not the target; the residual is prose debt.

Architecture and the reasoning behind each rule:
[ARCHITECTURE.md](https://github.com/valeratrades/ste_checker/blob/main/docs/ARCHITECTURE.md).

# Rewrite

ASD-STE100 removes the two largest sources of misreading: words with more than one meaning, and
sentences with more than one possible structure. It was built for a technician on a tarmac who
cannot ask the author a follow-up question. An agent that parses another agent's output, a tool
description or an error message is in the same position, and the same rules apply.

Use this half when a finding points at a sentence, when text is dense or ambiguous, or when text
will be read by another agent, a translation pipeline or a non-native reader. Do not use it on
creative or persuasive copy — STE is deliberately flat, and voice is the point there.

## Rules

Rows marked ✓ are what `# Check` reports; there you fix a finding rather than look for one. The
rest are yours alone — no checker sees them.

| Rule | Do | Don't | Checked |
|---|---|---|---|
| One word, one meaning | Pick one verb for one action and reuse it every time ("check", never rotating through "verify" and "confirm") | Rotate synonyms for the same idea across a document | partly |
| One part of speech per word | "Apply oil to the valve" (oil = noun) | "Oil the valve" (oil = verb), if *oil* is approved only as a noun | ✓ |
| Active voice | "The agent deletes the file." | "The file is deleted." — unless the actor is genuinely unknown or irrelevant | ✓ |
| Simple tenses only | "We received the report." | "We have received the report." | ✓ |
| Sentence length | ≤20 words for instructions, ≤25 for descriptions | Long chains of subordinate clauses | ✓ |
| Noun clusters | ≤3 stacked nouns ("fuel pump valve") | "high pressure fuel pump inlet valve assembly" | ✓ |
| No contractions | "do not", "it is" | "don't", "it's" | ✓ |
| One instruction per sentence | "Open the file. Read line 3." | "Open the file and read line 3, then check if it matches." | |
| No ellipsis | Keep subject, verb and article explicit even when it reads longer | Drop words to save space ("Files not backed up will be lost" — which files?) | |
| Paragraph limits | One topic per paragraph, ≤6 sentences | Multi-topic paragraphs | |
| Lists for sequences | A numbered or bulleted list for 3+ steps or conditions | A sequence buried in one prose sentence | |
| Safety first | Open a safety-critical instruction with the command or the condition | Bury the condition mid-sentence | |
| Domain terms | Keep the necessary technical noun or verb, and declare it once in `docs/glossary.nix` | Use jargon that is never defined | ✓ |

`references/writing-rules.md` has the fuller summary of the nine sections, and the citations.

## Process

1. Read the input once for meaning. Do not rewrite before you know what it must still say.
2. Walk it sentence by sentence and flag every violation.
3. Rewrite each flagged sentence, and keep the meaning exact. If a rewrite drops necessary
   precision — a safety condition, a scope qualifier, a number — keep the longer phrasing and
   flag it instead of simplifying in silence.
4. Report as a before/after table.
5. If the input complies, say so. Do not force changes onto compliant text.

```markdown
| Rule violated | Original | Simplified |
|---|---|---|
| Present perfect tense | "We have received your request." | "We received your request." |
| Noun cluster (4+ words) | "the agent task queue priority handler" | "the handler that sets task-queue priority" |
```

After the table, add one line on what you deliberately did not simplify, and why. Worked
examples, the agent-output cases included: `examples/before-after.md`.

## Boundaries

This half works from the principles of the standard, not from its dictionary — the wordlist is in
`# Check`. It is a clarity tool, not a certified STE authoring tool: for real aircraft
maintenance documentation, check word by word against the official standard, free at
<https://www.asd-ste100.org/>.

A safety condition, an exception or a scope qualifier is never dropped to shorten a sentence.
Flag the trade-off instead.
