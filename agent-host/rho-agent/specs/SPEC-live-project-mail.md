# SPEC-live-project-mail: An agent's live mail in a thread

## Record justification

The host grants the live-mail slot in the mail journal, while the notebook gives agents the separate send and edit operations; both must agree which operation can change words already visible to the user.

## Contract

A project agent can send one live message since the user's latest mail in a thread. A send that would occupy the writer's slot again fails before any mail or workset event is appended and tells the notebook which mail to edit. The refusal names the message to edit and points to flashes for progress. A flash does not occupy the slot and cannot ping. `chat.edit` revises the current words. Other writers have independent slots. A user's thread-wide reply freezes the exact dealt version it settles, including when moved; that version never controls delivery. Deleted and taken-back mail does not occupy a live slot. Words to another agent (`agents.message`) do not use these slots.

This implements `chat.the last message stays live` in the [product model](../../../product/chat.md).
