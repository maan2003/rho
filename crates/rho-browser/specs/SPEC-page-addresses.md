# SPEC-page-addresses: Page addresses

## Record justification

The page id is minted by the bundled extension (`extension/service-worker.js`),
read into `PageId` by the native-host bridge and by the `page` system
(`ui`), and kept as text by the frame in history and in ledger keys, so no
one of them can own the format alone.

## Contract

- A browser page is addressed `page:<id>`, where `<id>` is the UUID the
  extension minted for the tab, in lowercase hyphenated form: the text
  `crypto.randomUUID()` gives and the extension sends as `page_id`.
- That is the only spelling. Another spelling of the same UUID (`web-`
  prefixed, upper case, simple, braced) is not a page address, because two
  addresses are the same thing only when their text is, and a verdict filed
  under one spelling would be lost under another.
- The id is the extension's and outlives a GUI run: Chrome restores the tab
  with its id, so an address in the ledger still names that tab after a
  restart. An address whose tab is gone still parses; opening it shows the
  browser's own error rather than nothing.
