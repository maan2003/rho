# SPEC-mail-thread-entry: A new thread view starts with point in the draft

## Record justification

The thread builds its displayed document from the mail fold, but the editor owns the viewport. Setting its top must neither move the point nor allow blank overscroll. A live surface reopened as a recent place is not a new view.

## Contract

A fresh thread, opened by name or by deal, puts point in the draft and requests the first displayed project mail after the latest user mail as the top line. With no later project mail it requests the latest displayed mail. If that top would leave blank space beneath the draft, the editor clamps to its last full viewport. If the whole thread fits, its unused space is above the opening mail, not below the draft. A deal names the target for a direct reply but does not select a row. Returning to an existing view does not change either its point or viewport. A search result still selects its match.

This implements `mail.Thread` in the [product model](../../../product/mail.md).
