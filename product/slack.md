# slack

What asks through the Slack bot comes to the user as mail. The user's own Slack account is used in the browser.

## bet Slack comes as mail from a bot on a host
status: the host bot is not built; the user's Slack is in rho's browser.
instead of: a custom Slack client in the window, signed in as the user.
serves: Answer the world.
because: what the bot needs the user to answer can come in the same deal as the work, while the browser keeps the rest of Slack available under the user's own account.
costs: the bot cannot see the user's own direct messages, and colleagues see its replies as `rho`.

## journey A colleague needs me
status: the host bot is not built; the user reads and replies under their own account in rho's browser.
goal: the user answers a colleague whose words reached the Slack bot.
starts when: someone reaches the bot in Slack.
steps:
1. the user opens the Slack mail that waits, `deal.Clear what waits`
2. the user reads what the colleague wrote, `mail.Answer a mail`
3. the user replies to the mail, `mail.Answer a mail`
done when: the bot has posted the user's words as `rho`, and the card no longer waits.
day: Answer the world.

## concept Bot
status: not built; the user's Slack is in rho's browser.
purpose: turn the Slack that reaches rho into mail the user can answer.
principle: a colleague mentions the bot in a channel it is in; the user receives a mail from `slack@harness`, replies, and the colleague sees the answer from `rho`.
sees: the mail source `slack@harness`, the Slack words it carries, and `rho` as the name on its posts in Slack.
does:
- receive: take mentions in channels the bot is in, direct messages to the bot, and messages in threads the bot is in; the user's own direct messages do not reach it
- post: send the user's reply into Slack as `rho`
- answer on its own: answer using only what rho itself knows
- show chatter: show the latest chatter from starred channels as the project's latest words, a mail that does not ping, not as waiting mail

The rest is Slack in a `browser.Page` under the user's own account. The user can return to it with a `browser.Bookmark`.

Historical laptop walks of the signed-in Slack client, now superseded by the bot and the browser:

Find something said in Slack:
> reviewed: 2026-11-20 · laptop · reviewer · 2 findings

Catch up after time away:
> reviewed: 2026-11-20 · laptop · reviewer · 2 findings

Connect a Slack workspace:
> reviewed: 2026-11-20 · laptop · reviewer · 1 finding
