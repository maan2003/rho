# Leaving a Slack conversation marks it read

*eng-bgkw, 2026-09-07.*

Reading a channel in rho did not tell Slack it had been read, so the phone
and the Slack app went on badging messages the reader had already seen.
`ConversationView::mark_read` existed and had no caller anywhere in rho-gui:
the Zulip narrows do a summary-buffer exit when the reader leaves one, and
the Slack surface had nothing. It does now, from
`display_surface_with_method`, when the surface being replaced is not the one
being shown — so leaving a conversation for anything else marks it at the
newest message loaded, which is what Slack itself does when a channel is
opened. What changes for the user: a conversation read here stops asking
everywhere they read Slack, and rho's own unread rule keeps the place it had
when they opened it, because the surface takes that cursor once and holds it
for as long as they are reading.
