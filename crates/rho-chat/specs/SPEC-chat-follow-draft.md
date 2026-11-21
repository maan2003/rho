# SPEC-chat-follow-draft: New messages preserves the reader's place

## Record justification

The chat fold knows which messages arrived or was edited; only the editor knows whether the draft is inline or projected over the history.

## Contract

The newest message appears below the draft. A viewport at the top stays there. When the draft is stuck, the history viewport preserves its place, subject to document bounds, and a muted `↑ new` marks kept project messages until the draft is inline again. The user's own pending and logged messages never mark the notice. A send clears the draft and puts point in the new draft without following messages.

This implements [new messages land below the draft](../../../product/chat.md#rule-new-messages-land-below-the-draft).
