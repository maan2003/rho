# SPEC-live-project-mail: An agent's live mail in a thread

## Record justification

The host grants the live-mail slot in the mail journal, while the notebook gives agents the separate send and edit operations; both must agree which operation can change words already visible to the user.

## Contract

A project agent can send one active mail since the user's latest mail in a thread. A send that would occupy the writer's slot again fails before any mail or workset event is appended and tells the notebook which mail to edit. An explicit `mail.edit` or `mail.send(..., replace_last=True)` can revise that mail. The opt-in send selects and revises the slot in the same transaction; it returns the existing id and uses the edit path, preserving its recipients, reply target, and ping unless the ping is set. If the slot is empty it posts instead. Other writers have independent slots. Replies still freeze the answered mail, including when it is moved. Deleted and taken-back mail does not occupy a live slot. Project directories and directed agent-to-agent conversations do not use these slots.

This implements `mail.the last mail stays live` in the [product model](../../../product/mail.md).
