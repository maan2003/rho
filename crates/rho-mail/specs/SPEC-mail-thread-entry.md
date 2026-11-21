# SPEC-mail-thread-entry: A new thread view starts with point in the draft

## Record justification

The thread builds its displayed document from the mail fold, but the editor owns the viewport. Setting its top must neither move the point nor allow blank overscroll. A live surface reopened as a recent place is not a new view.

## Contract

A fresh thread starts at the top, whether opened by name or by deal. Point is in the draft, using any remembered position in its text; newest mail follows below. A deal remembers its seen card version without selecting or addressing a message. A sent reply reaches the thread's agents and settles only that version, even in the local outbox. A host refusal restores the wait and retains the words; a reply sent by name does not settle a card. Returning to an existing view does not change either its point or viewport. A search result still selects its match.

This implements `chat.Thread` in the [product model](../../../product/chat.md).
