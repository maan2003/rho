# SPEC-mail-follow-tail: New mail preserves the reader's place

## Record justification

The mail fold knows when new project text arrives; only the editor knows whether its draft is inline or projected separately. The mail surface chooses its response without moving the selection.

## Contract

An arriving project mail or a new version of the latest mail preserves the current viewport-relative position of an inline draft, subject to the editor's no-overscroll bounds. When the draft is stuck, the history viewport does not follow. The bottom-stuck excerpt shows a muted `↓ new mail` until it returns inline. The user's local posting acts on the draft and history as before, not as incoming mail.

This implements [new mail follows the draft at the tail](../../../product/mail.md#rule-new-mail-follows-the-draft-at-the-tail).
