# Slack shortcode aliases

`slack-shortcodes.tsv` is a compact, generated projection of every
`short_names` entry in `emoji.json` from
[iamcal/emoji-data](https://github.com/iamcal/emoji-data), release **v16.0.0**,
commit `2771d0b1b3af25c069086e68e38f901c3dda8bdf`.

Authoritative source:

- <https://github.com/iamcal/emoji-data/blob/2771d0b1b3af25c069086e68e38f901c3dda8bdf/emoji.json>
- Source SHA-256: `1d602e65be88772bf8cc368ce16b855d719eeddbafe128d471b80203f494d29f`
- Generated TSV SHA-256: `1d97599283c82dd65146bfe543316f64888346c47cb6db9ab0c8a34cbf227a28`

The projection contains 1,972 sorted `short-name<TAB>qualified-Unicode` rows
(37,046 bytes), rather than bundling the 1,313,457-byte source JSON. Generate it
by decoding each entry's hyphen-separated `unified` code points and emitting
one row for every value in `short_names`.

The source and generated data are MIT-licensed. The upstream license is copied
verbatim in `LICENSE`.
