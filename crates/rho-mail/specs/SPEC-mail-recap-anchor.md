# SPEC-mail-recap-anchor: Thread recap anchor

## Record justification

The mail fold owns original and revised text, the attention ledger owns verdict dates, and the thread draws the chosen section; none alone has the data that makes the choice device-independent.

## Contract

Read each project arrival or revision against the latest earlier user mail or verdict in that thread. More than ten minutes (or no earlier user activity) makes it eligible. Of the eligible versions containing a recap, show only the most recent, before the current body of its mail. A later warm mail, verdict, or edit does not hide it. On replay derive the same choice; opening and leaving a thread supply no input. An explicit removal or rewind of the mail removes its recap along with its active contribution.

`mail_section` includes the heading and ends at the next heading of the same or higher level, or at the end of the text. `message_item` hides that range in other mail only if its body has non-whitespace text outside the range. Otherwise it keeps the full mail, heading included, in the normal reading colour; that mail is not a second recap anchor.

This implements `mail.a cold mail carries a recap` in the [product model](../../../product/mail.md). Host prompts and reminders are separate.
