# SPEC-chat-thread-entry: An opened thread starts with point in the draft

## Record justification

The thread builds its displayed document from the chat fold, but the editor owns the viewport, and the frame decides whether a screen is opened or returned to through history. Setting its top must not allow blank overscroll.

## Contract

A thread opened by name or by deal starts at the top, whether its view is new or kept from an earlier visit. Point is in the draft, using any remembered position in its text; newest message follows below. A card remembers how far its thread was read without selecting or addressing a message. A sent reply reaches the thread's agents and settles the thread through the newest mail in view when it was sent, even in the local outbox; mail after that still waits. A host refusal restores the wait and retains the words; a reply sent by name does not settle a card. Returning to it through history, back or forward, changes neither its point nor its viewport. A search result still selects its match.

This implements `chat.Thread` in the [product model](../../../product/chat.md).
