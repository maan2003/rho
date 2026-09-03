# Slack UX checklist

Owner: the primary agent (eng-en1p) with the user. The engineer works the
items in order and does not reorder or skip. Items marked "decided" carry
the user's call and are not up for debate; anything else the engineer finds
unclear goes back to the owner before it is built.

Every item is judged the same way:

1. What rho shows now.
2. What Slack does, and whether Slack is actually better here.
3. Rho's version: a text buffer the user reads and edits with vim keys, no
   chrome, no ids, everything yankable and searchable, the same shape as the
   agent transcript wherever the two overlap.
4. A screenshot from the fake, under `/tmp/rho-slack-ux/screens/NN-item.png`.

Reference shot, from a real group DM on 2026-09-03, showing the starting
state: raw `#mpdm-david--manmeet--keith-1` label, emoji as `:shortcode:`,
`[file: image.png]`, no reply counts, no reactions, no unread rule, a bare
composer line, `@you` mentions.

## Phase 0: the fake can reach the real state

Nothing else lands until this does; every screenshot below comes from it.

- [ ] 0.1 The fake serves a group DM (`is_mpim`, name
      `mpdm-david--manmeet--keith-1`, three members including self) and a
      private channel (`is_private`).
- [ ] 0.2 Seeded messages cover: standard emoji shortcodes (`:thumbsup:`,
      `:sweat_smile:`), one custom emoji (`:forrest_gump_wave:`), a file
      attachment (`files: [{name: image.png, mimetype: image/png, size,
      url_private}]`), a self mention (`<@ME>`), reactions on a message
      (`reactions: [{name, users, count}]`), a parent with replies
      (`reply_count`, `latest_reply`, `reply_users`), an edited message
      (`edited`), a `thread_broadcast`, a `channel_join`, a bot message with
      `blocks` and `bot_profile`, a legacy `attachments` unfurl, and mrkdwn
      with bold, italic, strike, inline code, a code block, a quote, a list,
      a `<https://…|label>` link, a `<#C…|name>` link, `<!here>`,
      `<!subteam^S1|@design-team>`, and a `<!date^…>` token.
- [ ] 0.3 History spans several days with gaps, and one conversation has
      more than 200 messages so paging is exercised.
- [ ] 0.4 The fake carries `last_read` per conversation and answers
      `conversations.info` with it, so an unread rule can be placed
      mid-history.
- [ ] 0.5 `examples/fake_slack.rs` seeds all of the above; one run of it
      plus `RHO_SLACK_API_BASE` reproduces the reference shot's conversation
      in rho. The websocket connects, so the status bar does not read
      `disconnected` in screenshots.
- [ ] 0.7 The fake can drive live events while rho is open: the example
      exposes a control file or endpoint the QA run can poke to push a new
      message, a thread reply, a reaction, an edit, and a deletion over the
      websocket into a seeded conversation on demand, plus a `--live`
      mode that posts a scripted message every few seconds. Every Phase 2
      live screenshot comes from this.
- [ ] 0.6 The fake serves `emoji.list` with the custom emoji, and
      `users.info` with display names that differ from handles.

## Phase 1: naming and reading

- [ ] 1.1 Conversation names. Now the raw mpdm name. Slack shows the other
      members' names. Rho: group DM = member display names minus self,
      joined with `, `, no `#`; DM = `@name`; channel and private channel =
      `#name`. Same label in the list, the surface title, the status bar,
      the dealer card, and the filed desk heading. A raw `mpdm-` or
      `C0…` string anywhere is a defect.
- [ ] 1.2 Emoji. Now `:shortcode:` literal. Rho: standard shortcodes become
      the glyph (a shortcode table, e.g. the `emojis` crate); workspace
      custom emoji stay `:name:` in the muted class. Applies to bodies,
      reactions, and the conversation list.
- [ ] 1.3 Names, decided: the word `you` appears nowhere. The user's own
      messages carry the user's display name like anyone else's, and a
      mention of the user renders as `@<display name>`, the same string the
      author line uses. Self is distinguished by class only: own author name
      and mentions of self in the "you" class, everyone else in the sender
      class.
- [ ] 1.4 Message layout, decided: compact, IRC and Discord-compact style.
      Time, author, and the first line of the body on one line:
      `14:27  David   Hope manifold isn't stressing you out too much`.
      Author names pad to a fixed column per conversation so bodies line
      up. Continuation lines of a multi-line body align under the body
      column; if the buffer's language would read that indent as a code
      block, switch the surface to a plain language and style through spans
      (the spans already carry every class). No blank line between
      messages; day separators are the only breaks. Reaction and reply
      lines (1.6, 1.7) sit under the body, aligned to the body column.
- [ ] 1.5 Day separators stay `── Fri 21 Aug ──`; times stay `14:27`;
      absolute, never "today" or "2h ago".
- [ ] 1.6 Threads isolated, decided. Now replies are interleaved with the
      channel's messages and marked `in thread`. Slack keeps replies out of
      the channel entirely; rho does the same. The channel surface shows
      only top-level messages (`thread_ts` absent or equal to `ts`) plus
      `thread_broadcast` replies, which read `also sent to the channel` in
      the muted class. A parent with replies carries one muted line under
      its body, `↳ 3 replies · 14:41`, and `enter` on it or on the parent
      opens the thread surface, which is the only place replies render.
      A reply arriving over the websocket updates the parent's count line,
      never the channel body. The `in thread` marker goes away.
- [ ] 1.7 Reactions. Now absent. Rho: one muted line under the message,
      `👍 3 · 🎉 1`; a reaction the user added renders in the "you" class
      instead of muted, no word for it. Reading them is in scope; adding
      stays deferred.
- [ ] 1.8 Edited and deleted. `(edited)` in the muted class after the time;
      a `message_deleted` frame removes the message from the buffer.
- [ ] 1.9 mrkdwn coverage. Every construct seeded in 0.2 renders as
      readable text: bold and italic keep Slack's markers, strike renders as
      `~text~`, inline code as backticks, code blocks fenced, quotes `> `,
      lists `- ` or `1. `, links as `label <url>`, channel links as
      `#name`, user groups as `@handle`, `<!here>` as `@here`, dates as the
      fallback text. Unknown ids never leak (already tested; keep).
- [ ] 1.10 Bot and app messages. Author is the bot's display name; `blocks`
      through `render_block`; legacy `attachments` render title, pretext,
      text, and fields. Link unfurls (`is_msg_unfurl`, `is_app_unfurl`)
      collapse to one muted line `↗ title`, never the full preview.
- [ ] 1.11 System messages, decided: shown, because Slack shows them.
      `channel_join`, `channel_leave`, `channel_topic`, `pinned_item`
      render as one muted line `— David joined —`.
- [ ] 1.12 Files, decided: images preview inline. An image file renders as
      the picture itself under the message, bounded to a sane height, the
      way the agent transcript shows images (reuse that path and
      `rho-image`); fetched through the API with the session's token and
      cookie into the state cache. Any other file is a link line
      `deck.pdf · pdf · 220 KB`; `enter` on it downloads to the cache and
      hands the path to `xdg-open`. `[file: …]` placeholders are a defect.

- [ ] 1.13 Avatars, decided: a small avatar sits before the author name on
      each message line in the compact layout, one line tall, a fixed width
      so the name and body columns never shift. Source: `profile.image_48`
      from `users.info` or `users.list`, keyed by user id plus
      `avatar_hash` so a changed picture refetches and an unchanged one
      never does. Cache in two layers: on disk under the state cache
      (`~/.local/state/rho/slack-avatars/<hash>.png`, written once, never
      expired), and in memory for the open session. Fetch lazily when a line
      first renders; until the bytes arrive the slot is blank, not a
      placeholder glyph. The avatar is decoration through the editor's
      inlay path, never text: yank and search see only the name. Bot
      messages use `bot_profile.icons.image_48` the same way. If the vendored
      editor cannot place an image inline at a fixed width without breaking
      line layout, report that before working around it.

## Phase 2: unread, position, and moving around

- [ ] 2.1 Unread rule. Now none. Rho: `── new ──` at `last_read`; opening a
      conversation puts the cursor on the first unread line, not the
      composer and not the top. `G` still goes to the end. The rule stays
      until the surface is closed.
- [ ] 2.2 Read marking, decided for now: on open, as Slack does. The user
      holds this loosely; different read semantics may come later, so keep
      the mark call in one place.
- [ ] 2.3 Following the tail. Pinned at the bottom, a new message keeps the
      view at the bottom. Scrolled up, the view does not move and the
      surface's status segment shows `3 new`; `G` clears it.
- [ ] 2.4 Older history stays on `shift-p`; the echo reports `loaded 100
      older` through the Messages buffer, and the cursor stays on the line
      it was on.
- [ ] 2.5 Next unread conversation from inside a conversation: `shift-n`,
      the same key Zulip uses in rho. Wraps to the list when nothing is
      unread.
- [ ] 2.6 Conversation list rows. Now `#design @1`, `@ada unread`, a rule,
      then the rest. Rho: `label  @2 · 5 new  14:27`; unread first, then by
      recency; muted conversations at the bottom under a rule. No
      last-message preview. Presence is deferred.
- [ ] 2.8 Live updates, reported broken by the user in real use: an open
      conversation does not change when new messages arrive. Rho: a
      websocket `message` for the open conversation appends at once, with
      2.3's tail rule; a reply updates the parent's count line and the open
      thread surface; `reaction_added`, `reaction_removed`,
      `message_changed`, and `message_deleted` update in place; the
      conversation list's unread counts move without reopening it; the
      user's own sent message shows without a refetch. Find the real cause
      first (a dropped socket, a missed frame type, or a view that only
      re-renders on open) and say which it was.
- [ ] 2.7 Direct messages raise cards. Slack's `activity.feed` does not
      carry DMs, only mentions, reactions, and thread replies; a DM never
      reaches the inbox today. Rho: a websocket `message` in an `im` or
      `mpim` from someone else, or an unread DM in `client.counts` at
      startup, raises the same obligation card as a mention.

## Phase 3: composing

- [ ] 3.1 Composer boundary. Now a bare line under the last message. Rho:
      the same shape as the agent transcript's prompt: its rule and its
      marker, then the input; a muted placeholder `message #design` while
      empty. Whatever the transcript does, the Slack surface does too.
- [ ] 3.2 Sending. `enter` in insert sends; `shift-enter` inserts a newline;
      a pasted block with newlines never sends by itself. The sent message
      appears at once in the muted class until the server's echo replaces
      it. On failure the text stays in the composer and the error goes to
      the Messages buffer. Typed text is never lost.
- [ ] 3.3 Completion. `@` completes over conversation members, `:` over
      emoji, `#` over channels, through rho's prompt completion. The editor
      shows `@ada`; the wire carries `<@U1>`.
- [ ] 3.4 In a thread surface `enter` sends to the thread. "Also send to
      channel", editing, and deleting stay deferred.

## Phase 4: status and health

- [ ] 4.1 Status bar. The surface segment reads the conversation label, and
      `· thread` inside a thread. Nothing about the connection while it is
      healthy; degraded shows the lamp and the notice (already), and the
      word `disconnected` appears only then.
- [ ] 4.2 Typing indicators and presence: deferred, unchanged.

## Phase 5: one key table

- [ ] 5.1 The list: `enter` open, `s` narrow, `shift-n` next unread, `q`
      close. The conversation: `enter` open the thread under the cursor or
      the file link, `i` compose, `shift-p` older, `shift-n` next unread,
      `ctrl-k` back to the channel, `G` end and clear `new`, `q` close.
      Composer: `enter` send, `shift-enter` newline, `escape` normal mode.
      Documented once, in `SLACK-DESIGN.md`, and every binding has a test.

## Still deferred

Adding reactions, file upload, edit and delete, presence and typing, message
search, automatic token extraction, dialogs and modals.

## Done means

The reference conversation, re-seeded in the fake, renders as: the label
`David, Keith`; emoji as glyphs; the image shown inline; compact one-line
headers with the user's own name, never `you`; a reply
count under any threaded parent; reactions under any reacted message; the
`── new ──` rule at the right place with the cursor on it; the composer with
the transcript's boundary; and a screenshot per item in
`/tmp/rho-slack-ux/screens/`.
