# SPEC-usage-addresses: Usage addresses

## Record justification

The address is written and read by the agents system (`usage`), and kept
as text by the frame in history, in ledger keys and in what the desk syncs
to the user's other devices, so no one of them can own the format alone.

## Contract

- A usage chart is addressed `usage:<chart>/<days>`, where `<chart>` is one
  of `rate-limit`, `model-cost`, `usage-share` or `agent-cost`, and
  `<days>` is `7` or `30`: eight addresses in all.
- That is the only spelling. Any other text under the `usage` kind (another
  chart word, another window, a part missing, leading zeros, a trailing
  `/`) is not a usage address and opens nothing, because two addresses are
  the same thing only when their text is, and a verdict or a note filed
  under one spelling would be lost under another.
- An address names a picture over a window that ends now, not a snapshot:
  opening it after a restart or on another device asks the attached hosts
  again, and what it shows is what they answer.
