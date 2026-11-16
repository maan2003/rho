# SPEC-note-addresses: Note addresses

## Record justification

The address of a note is minted by `rho_dealer::Address::note`, kept as
text in every device's ledger entries and read back by the notes system
(`rho_notes::NoteId`), the chat's private replies and the dealer, so no
one of them owns its form.

## Contract

- A note is `note:<id>`, where `<id>` is the UUID it was minted under in
  its simple form: 32 lowercase hex digits, no hyphens.
- That form is the only one. The same UUID written any other way is not
  the note's address and opens nothing, so one note has one key in the
  ledger, in history and in `find`.
- The id is minted once, by the device that makes the note, and never
  changes. A note's text and what it is about are said about the
  address; editing it never mints a new one.
- Entries written before rho2 already use this form, and must keep
  reading. So must the `label:<id>` addresses older builds minted for
  labels: they still read as addresses, of a kind no system opens.
