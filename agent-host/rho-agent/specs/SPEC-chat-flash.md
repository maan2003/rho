# SPEC-chat-flash: A flash is visible until it is both old and superseded

## Record justification

The notebook chooses the kind, the host enforces expiry from the journal, and devices fold the same deletion into threads and rows; none owns the complete contract.

## Contract

`chat.send` requires an explicit kind and a thread, and the kind becomes the post's one voice. Only a message pings and occupies the live slot. A flash never pings, goes only to the user, and a user reply to its thread goes to every agent in the thread.

The clock starts at the original post, not at an edit. The host deletes a flash only if it is at least twenty minutes old and has a newer visible item in the same thread, from any writer. Either a deadline or a new item can supply the second condition. Re-exec keeps the original clock. The deletion is the same journal event devices already fold for a deleted message; search and visible history no longer show its words.

Implements `chat.Flash` in the [product model](../../../product/chat.md), alongside [SPEC-live-project-mail](SPEC-live-project-mail.md).
