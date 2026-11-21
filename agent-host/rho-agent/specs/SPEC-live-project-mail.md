# SPEC-live-project-mail: An agent's live mail in a thread

## Record justification

The host grants the live-mail slot in the mail journal, while the notebook gives agents the separate send and edit operations; both must agree which operation can change words already visible to the user.

## Contract

A project agent can send one active mail since the user's latest mail in a thread. A send that would occupy the writer's slot again fails before any mail or workset event is appended and tells the notebook which mail to edit. Only a deliberate edit can change that mail. Other writers have independent slots. Replies still freeze the answered mail, including when it is moved. Deleted and taken-back mail does not occupy a live slot. Project directories and directed agent-to-agent conversations do not use these slots.

This implements `mail.the last mail stays live` in the [product model](../../../product/mail.md).
