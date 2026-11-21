# SPEC-mail-follow-tail: New mail preserves the reader's place

## Record justification

The mail fold knows when new project text arrives; only the editor knows whether its draft is inline or projected separately. The mail surface chooses its response without moving the selection.

## Contract

As new mail or a new version of the latest mail joins the history, an inline draft keeps its viewport-relative position, subject to the editor's no-overscroll bounds. When the draft is stuck, the history viewport stays where it was. The bottom-stuck excerpt shows a muted `↓ new mail` for project mail until it returns inline; it does not show the notice for the user's sending or logged mail. Sending a thread reply puts point in the empty draft without following any mail into the history.

This implements [new mail follows the draft at the tail](../../../product/mail.md#rule-new-mail-follows-the-draft-at-the-tail).
