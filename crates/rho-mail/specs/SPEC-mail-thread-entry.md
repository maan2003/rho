# SPEC-mail-thread-entry: A new thread view starts with point in the draft

## Record justification

The thread builds its displayed document from the mail fold, but the editor owns the viewport. Setting its top must neither move the point nor allow blank overscroll. A live surface reopened as a recent place is not a new view.

## Contract

A fresh thread starts at the top, whether opened by name or by deal. Point is in the draft, using any remembered position in its text; newest mail follows below. A deal names the target for a direct reply but does not select a mail. Returning to an existing view does not change either its point or viewport. A search result still selects its match.

This implements `mail.Thread` in the [product model](../../../product/mail.md).
