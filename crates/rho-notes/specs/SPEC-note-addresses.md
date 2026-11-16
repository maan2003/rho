# SPEC-note-addresses: Note and label addresses

## Record justification

The address of a note or a label is minted by `rho_dealer::Address::note`
and `Address::label`, kept as text in every device's ledger entries and
read back by the notes system (`rho_notes::NoteId`, `LabelId`) and the
dealer, so no one of them owns its form.

## Contract

- A note is `note:<id>` and a label is `label:<id>`, where `<id>` is the
  UUID it was minted under in its simple form: 32 lowercase hex digits, no
  hyphens.
- That form is the only one. The same UUID written any other way is not
  the thing's address and opens nothing, so one thing has one key in the
  ledger, in history and in `find`.
- The id is minted once, by the device that makes the note or label, and
  never changes. A label's name and parent, and a note's text, are said
  about the address; renaming or moving never mints a new one.
- Entries written before rho2 already use this form, and must keep
  reading.
