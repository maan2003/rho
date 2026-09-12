# A rewrite whose message is deleted closes, and keeps your words

*eng-bgkw, 2026-09-08.*

You press `e` on something you said, start rewriting it, and meanwhile it
is deleted — from the Slack app on your phone, or by anyone else who can.

Slack will not update a message that is not there. The rewrite stayed open
over a row that had gone from the screen; enter sent `chat.update`, Slack
refused it, and the refusal put you back into the same rewrite with the
same words. Pressing enter again did the same thing. The only way out was
escape, and nothing on screen said so — no error line, no notice, just a
key that appeared to do nothing.

Observed before the change: after the deletion the transcript holds no
rows, `editing_message` is still set, and there is no error anywhere; after
enter, the state is what it was before enter, word for word.

Now the rewrite closes as soon as the deletion arrives, and the words stay
in the composer, over whatever had been held aside for the edit — the same
as any other refusal, because text that was typed is never dropped on the
floor. Enter then sends them as a new message, which is the only thing
Slack will now accept. One line says what happened: *that message was
deleted; your rewrite is in the composer*.

The deletion is read from the update log rather than from the transcript:
the log says the message was deleted, where an absent row could also be a
run that has not been drawn yet. It costs the updates that just arrived.
