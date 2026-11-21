# SPEC-mail-follow-draft: New mail preserves the reader's place

## Record justification

The mail fold knows which mail arrived or was edited; only the editor knows whether the draft is inline or projected over the history.

## Contract

The newest mail appears below the draft. A viewport at the top stays there. When the draft is stuck, the history viewport preserves its place, subject to document bounds, and a muted `↑ new mail` marks project mail until the draft is inline again. The user's own pending and logged mail never mark the notice. A send clears the draft and puts point in the new draft without following mail.

This implements [new mail lands below the draft](../../../product/mail.md#rule-new-mail-lands-below-the-draft).
