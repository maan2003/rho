# ste_checker
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.98+-ab6000.svg)
[<img alt="crates.io" src="https://img.shields.io/crates/v/ste_checker.svg?color=fc8d62&logo=rust" height="20" style=flat-square>](https://crates.io/crates/ste_checker)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs&style=flat-square" height="20">](https://docs.rs/ste_checker)
![Lines Of Code](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/valeratrades/b48e6f02c61942200e7d1e3eeabf9bcb/raw/ste_checker-loc.json)
<br>
[<img alt="ci errors" src="https://img.shields.io/github/actions/workflow/status/valeratrades/ste_checker/errors.yml?branch=main&style=for-the-badge&style=flat-square&label=errors&labelColor=420d09" height="20">](https://github.com/valeratrades/ste_checker/actions?query=branch%3Amain) <!--NB: Won't find it if repo is private-->
[<img alt="ci warnings" src="https://img.shields.io/github/actions/workflow/status/valeratrades/ste_checker/warnings.yml?branch=main&style=for-the-badge&style=flat-square&label=warnings&labelColor=d16002" height="20">](https://github.com/valeratrades/ste_checker/actions?query=branch%3Amain) <!--NB: Won't find it if repo is private-->

`ste_checker` reads Markdown. It reports the text that does not agree with ASD-STE100
(Simplified Technical English).

![showcase](./docs/.assets/showcase.png)

These are the rules that a program can apply: the approved wordlist, the part of speech
of each word, the length of each sentence, noun clusters, the passive voice, compound
tenses, -ing forms and contractions.

The standard has 53 rules. This program has 10 of them. A reader who understands the
text must apply the other 43. ASD and STEMG do not endorse this program, and it
certifies nothing. Use it as an aid, not as a gate.

A word can be approved in one part of speech and not in another. *Work* is an approved
noun and an unapproved verb. `ste_checker` tags each word before it reads the wordlist.
Thus "Do not work the lever" is a finding, but "The work is done" is not.
<!-- markdownlint-disable -->
<details>
<summary>
<h2>Installation</h2>
</summary>

With nix, run it without an installation:

```sh
nix run github:valeratrades/ste_checker -- --help
```

With [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), get it from the releases:

```sh
cargo binstall ste_checker
```

`cargo install ste_checker` also makes it from the source.

</details>
<!-- markdownlint-restore -->

## Usage
Give all the files to one process. The language model loads one time, then each file
costs approximately 100 microseconds.

```sh
ste_checker docs/.readme_assets/usage.md docs/.readme_assets/installation.md
```

Findings are warnings, and the exit code is 0. To make them errors, add `--deny`. For
a machine-readable report, add `--format json`.

ASD-STE100 has approximately 900 approved words. All other words are not approved. Thus
each project must declare its Technical Names and Technical Verbs in `docs/glossary.nix`.
Use `--glossary` for a different file.

```nix
{
  names = [ "server" { name = "oauth"; desc = "the OAuth 2.0 authorization flow"; } ];
  verbs = [ "parse" ];
}
```

A name is approved as a noun, and a verb is approved as a verb. For the initial file,
send `--suggest-glossary` to the glossary:

```sh
ste_checker --suggest-glossary docs/.readme_assets/*.md > docs/glossary.nix
```

It writes each word that is not in the approved vocabulary, and it does not read the
file it writes. Remove the words that are usual English, then write a description for
the words that stay.

Set `text_type` to `description` in the configuration file to permit 25 words in a
sentence instead of 20. Put rule names in `disable` to switch rules off.

### Attribution
The wordlist in `ste_checker/vendor/openste.json` is [openSTE](https://github.com/openste/openste)
v1.01, with the MIT license in `ste_checker/vendor/LICENSE-openste`. Part-of-speech tags, the
Markdown parser and the sentence splitter come from [Harper](https://github.com/Automattic/harper),
with the Apache-2.0 license.

ASD-STE100 is a specification of the AeroSpace and Defence Industries Association of Europe. This
project has no relation to ASD or to the STE Maintenance Group.


<br>

<sup>
	This repository follows <a href="https://github.com/valeratrades/.github/tree/master/best_practices">my best practices</a> and <a href="https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md">Tiger Style</a> (except "proper capitalization for acronyms": (VsrState, not VSRState) and formatting). For project's architecture, see <a href="./docs/ARCHITECTURE.md">ARCHITECTURE.md</a>.
</sup>

#### License

<sup>
	Licensed under <a href="LICENSE">Blue Oak 1.0.0</a>
</sup>

<br>

<sub>
	Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>

