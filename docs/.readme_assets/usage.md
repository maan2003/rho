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
