# Slack UX checklist

Reconciled against main on 3 Sep. Genuinely open, in order:

- 1.13 avatars (waits on the editor's inline-image inlay), 1.22 rough edges
  (b) soft-wrap column waits on the vendored editor.
- 2.1 unread rule and cursor on first unread; 2.3 the `3 new` status
  segment (the anchoring half landed); 2.5 `shift-n` next unread; 2.6 list
  row counts, time column, and the muted section; 2.7 DMs in `mpim` and
  unread DMs from `client.counts` at startup (one-to-one DMs landed);
  2.8 (b) live counts, `reaction_*` frames, and tail re-sync on reconnect
  (edits and deletions landed with 3.6).
- 3.1 composer boundary and placeholder; 3.2 `shift-enter`, the muted
  local echo, and keeping the text on a failed send (`enter` sends);
  3.3 completion.
- 5.1 the one key table, once 2.5 and 3.2 have their keys.

4.2 stays deferred.

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

Rule: all mocking is server side. The fake is a fake Slack server, HTTP
endpoints plus websocket, and the unmodified client talks to it through
`RHO_SLACK_API_BASE`. No client-side stubs, no test-only rendering paths,
no state injected into the GUI, no flags that change what the client does
under test. If the fake cannot produce a state, extend the fake.

- [x] 0.1 The fake serves a group DM (`is_mpim`, name
      `mpdm-david--manmeet--keith-1`, three members including self) and a
      private channel (`is_private`).
- [x] 0.2 Seeded messages cover: standard emoji shortcodes (`:thumbsup:`,
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
- [x] 0.3 History spans several days with gaps, and one conversation has
      more than 200 messages so paging is exercised.
- [x] 0.4 The fake carries `last_read` per conversation and answers
      `conversations.info` with it, so an unread rule can be placed
      mid-history.
- [x] 0.5 `examples/fake_slack.rs` seeds all of the above; one run of it
      plus `RHO_SLACK_API_BASE` reproduces the reference shot's conversation
      in rho. The websocket connects, so the status bar does not read
      `disconnected` in screenshots.
- [x] 0.7 The fake can drive live events while rho is open: the example
      exposes a control file or endpoint the QA run can poke to push a new
      message, a thread reply, a reaction, an edit, and a deletion over the
      websocket into a seeded conversation on demand, plus a `--live`
      mode that posts a scripted message every few seconds. Every Phase 2
      live screenshot comes from this.
- [x] 0.6 The fake serves `emoji.list` with the custom emoji, and
      `users.info` with display names that differ from handles.

## Phase 1: naming and reading

- [x] 1.1 Conversation names. Now the raw mpdm name. Slack shows the other
      members' names. Rho: group DM = member display names minus self,
      joined with `, `, no `#`; DM = `@name`; channel and private channel =
      `#name`. Same label in the list, the surface title, the status bar,
      the dealer card, the filed desk heading, and the inbox deal surface's
      status segment, which today reads `inbox 1a065ab070c-2` (a raw inbox
      id, seen by the user). A raw `mpdm-`, `C0…`, or inbox id string
      anywhere is a defect.
- [x] 1.2 Emoji. Now `:shortcode:` literal. Rho: standard shortcodes become
      the glyph (a shortcode table, e.g. the `emojis` crate); workspace
      custom emoji stay `:name:` in the muted class. Applies to bodies,
      reactions, and the conversation list.
- [x] 1.3 Names, decided: the word `you` appears nowhere. The user's own
      messages carry the user's display name like anyone else's, and a
      mention of the user renders as `@<display name>`, the same string the
      author line uses. Self is distinguished by class only: own author name
      and mentions of self in the "you" class, everyone else in the sender
      class.
- [x] 1.4 Message layout, decided: compact, IRC and Discord-compact style.
      Time, author, and the first line of the body on one line:
      `14:27  David   Hope manifold isn't stressing you out too much`.
      Author names pad to a fixed column per conversation so bodies line
      up. Continuation lines of a multi-line body align under the body
      column; if the buffer's language would read that indent as a code
      block, switch the surface to a plain language and style through spans
      (the spans already carry every class). No blank line between
      messages; day separators are the only breaks. Reaction and reply
      lines (1.6, 1.7) sit under the body, aligned to the body column.
- [x] 1.5 Day separators stay `── Fri 21 Aug ──`; times stay `14:27`;
      absolute, never "today" or "2h ago".
- [x] 1.6 Threads isolated, decided. Now replies are interleaved with the
      channel's messages and marked `in thread`. Slack keeps replies out of
      the channel entirely; rho does the same. The channel surface shows
      only top-level messages (`thread_ts` absent or equal to `ts`) plus
      `thread_broadcast` replies, which read `also sent to the channel` in
      the muted class. A parent with replies carries one muted line under
      its body, `↳ 3 replies · 14:41`, and `enter` on it or on the parent
      opens the thread surface, which is the only place replies render.
      A reply arriving over the websocket updates the parent's count line,
      never the channel body. The `in thread` marker goes away.
- [x] 1.7 Reactions. Now absent. Rho: one muted line under the message,
      `👍 3 · 🎉 1`; a reaction the user added renders in the "you" class
      instead of muted, no word for it. Reading them is in scope; adding
      stays deferred.
- [x] 1.8 Edited and deleted. `(edited)` in the muted class at the end of
      the body (landed there rather than after the time, so 1.4's fixed
      clock column never shifts);
      a `message_deleted` frame removes the message from the buffer.
- [x] 1.9 mrkdwn coverage. Every construct seeded in 0.2 renders as
      readable text: bold and italic keep Slack's markers, strike renders as
      `~text~`, inline code as backticks, code blocks fenced, quotes `> `,
      lists `- ` or `1. `, links as `label <url>`, channel links as
      `#name`, user groups as `@handle`, `<!here>` as `@here`, dates as the
      fallback text. Unknown ids never leak (already tested; keep).
- [x] 1.10 Bot and app messages. Author is the bot's display name; `blocks`
      through `render_block`; legacy `attachments` render title, pretext,
      text, and fields. Link unfurls (`is_msg_unfurl`, `is_app_unfurl`)
      collapse to one muted line `↗ title`, never the full preview.
- [x] 1.11 System messages, decided: shown, because Slack shows them.
      `channel_join`, `channel_leave`, `channel_topic`, `pinned_item`
      render as one muted line `— David joined —`.
- [x] 1.12 Files, decided: images preview inline. An image file renders as
      the picture itself under the message, bounded to a sane height, the
      way the agent transcript shows images (reuse that path and
      `rho-image`); fetched through the API with the session's token and
      cookie into the state cache. Any other file is a link line
      `deck.pdf · pdf · 220 KB`; `enter` on it downloads to the cache and
      hands the path to `xdg-open`. `[file: …]` placeholders are a defect.

- [ ] 1.13 Avatars, decided by the user: inline images become a common
      editor primitive, an image variant of `InlayContent` in the vendored
      editor occupying a fixed number of cells at one line height
      (eng-5pha builds it after the dealer follow-ups). Avatars then land
      on that primitive; nothing per-Slack. Original spec follows. a small avatar sits before the author name on
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

Polish from the user's live screenshot of 3 Sep ("still a bit ugly"),
done right after the transcript primitive (2.4) and before 2.10:

- [x] 1.14 Not a defect: display names for everyone, and `maan2003` is
      the user's display name. Nothing changes.
- [x] 1.15 Moot after 1.20. Rows by the longest author start their body one column right
      of the others (padding is off by one when the name fills the
      column). Bodies align in one column across the surface.
- [x] 1.16 Links print twice: `<url|text>` renders as `text <url>` and
      `<url>` as `url <url>`. Render the text once in the link class, the
      URL once only when there is no text; the URL stays reachable through
      line metadata for `enter`.
- [x] 1.17 Unfurls, the user finds them "very weird" as loose lines:
      render as a quote box. Each unfurl line is prefixed with a left bar
      `▎ ` in the link colour and the whole range carries a faint
      background tint through the editor's background highlight, so it
      reads as one attached card. Inside: `title · site` on the first
      line in the link class, then at most two description lines, no
      blank lines. The x.com unfurl took ten loose lines with the whole
      tweet body. `enter` on it opens the URL. Collapsible later.
- [x] 1.18 Images, decided by the user: a small inline version, and a
      full-screen view on click or `enter`. Inline stays a thumbnail
      (about 12 lines high, aspect preserved). Click or `enter` on the
      thumbnail or its file line opens a rho image surface that fits the
      image to the window, `q` or `escape` returns to the conversation;
      no external opener for images. The file line drops the type when
      the extension already says it: `image.png · 141 KB`.
- [x] 1.21 Time placement, corrected after 32-unfurl: the time trails the
      last line of the message's own text, and chrome under a message
      (reactions, reply count, file lines, thumbnails, unfurl cards, bot
      attachment fields) never carries a time. Seen: `▎ A long preview
      body …  16:41` inside the quote box and `image.png · 220 KB  09:40`.
      Also: mrkdwn emphasis (`*bold*`, `_italic_`, `~struck~`) renders as
      raw markers in plain text; keep the markers, style the span (bold,
      italic, strike). And the unfurl tint on the first row starts after
      the bar; square it. And the screenshot rig lacks an emoji font, so
      reactions and `morning! ▯` show tofu; install one so screenshots are
      truthful.
- [ ] 1.22 Rough edges seen by the user on 3 Sep in a live DM (screenshot
      in the session): (a) an unfurl renders as a muted `— Tau of Unix` line
      plus an indented description, not the decided quote box (`▎` bar and
      tint, 1.21), so it reads as a stray dash; every unfurl, link preview
      or otherwise, is the box. (b) a soft-wrapped message continues at
      column 0 (`models can go  18:39`), while a hard line break continues
      at two columns (`  btw fman …`); both continue at two columns, so the
      body's left edge is one line. (c) the image label `image.png · 320 KB`
      sits as a full-height text line above the thumbnail; it becomes the
      muted small caption style used for chrome, and the thumbnail keeps
      its 12-line cap. Screenshot the same DM shape on the fake before and
      after. Two more edges seen by eng-en1p on 3 Sep, in the same change:
      (d) a todo note hangs under the dealt card but its `*` marker sat at
      the left edge, shallower than the `◦` of its own parent, so it read as
      the next root; (e) the note carried no words of its own, only
      `defer … · 7d`, and should carry the dealt card's words.
      Landed 3 Sep, except (b). (a): every attachment is the quote box now,
      preview and app card alike, bar and tint down the left; the dash form
      is gone and with it the styling that muted it. (c): the file line
      (`image.png · 220 KB`) is muted chrome, the thumbnail keeps its
      12-line cap. (d): a note under a card is indented four columns per
      card it hangs under, so its stars start past the card's marker. (e):
      the todo writes the card's own words into the new note, through the
      same `pending_desk_texts` path a pasted note uses. Rig on the fake:
      screens/64-dm.png is the reference DM (box, tinted card, muted file
      caption), screens/69-desk.png the todo note under its card.
      (b) is not done and needs the vendored editor: a soft wrap continues
      at the wrapped line's own leading whitespace, which is zero for
      `name: body`, so there is no client-side lever to make it two columns.
      It wants a minimum soft-wrap indent per editor; b8os does not touch
      the vendored editor.
- [x] 1.23 Images load without flicker. Landed 3 Sep as a fixed-size
      block filled by Slack's thumbnail, not an inlay and not a blur filter
      (gpui has none for images). Original: Seen by the user on 3 Sep: an
      image arriving in a conversation jumps. Two causes to remove: the
      box changes size when the bytes land, and the message item is
      replaced wholesale on arrival. Rho: the box is sized from the file's
      `original_w`/`original_h` at first render (12-line cap, aspect kept),
      so nothing below it moves; until the bytes land it shows Slack's own
      smallest thumbnail (`thumb_80`, or `thumb_64`) scaled up and blurred,
      a blurhash without the encoding step, and a muted box if no thumb is
      cached yet; when the full bytes land, the editor's image inlay is
      replaced by id in place, the transcript item is not rebuilt. Test:
      the item's range and the rows below it do not change across the
      swap; the fake serves file bytes with a `/control` delay so the rig
      shows the placeholder state.
      Landed 3 Sep. The box is a block sized from `original_w`/`original_h`
      (aspect kept, 12-line cap) and the picture inside it is drawn at a
      size spelled out from the same numbers: the editor resizes a block to
      whatever its element measured, so a thumbnail left to be its own tiny
      self shrank the box and moved every row under it. `thumb_64` (or
      `thumb_80`) is fetched alongside the picture and blown up to fill the
      box meanwhile — the upscale is the blur, gpui has no blur filter — and
      a muted box of the same size stands there until even that has landed.
      The arrival rewrites no item: the block reads the file cache when it
      draws, so `settle_images` only asks for a redraw, where it used to
      replace the message. Rig with a 12s `/control` file delay: the two
      screenshots (screens/74-placeholder.png, 75-arrived.png) differ only
      in rows 568..1071, the picture's own box; every row above and below is
      pixel-identical, which is the no-jump rule measured rather than
      described. Tests cover the sizing and that the boxes an item asks for
      do not depend on the cache; rho-slack has no gpui harness, so the
      before/after layout itself is the rig's to prove.
- [x] 1.24 A stale new-agent draft is the only way out of a conversation.
      Seen by b8os in the rig on 3 Sep: `ctrl-k` out of a Slack conversation
      surfaced a leftover "draft" compose buffer as the only buffer, with no
      way back to the desk short of a restart. Reproduce, find why a draft
      that was never submitted stays in the surface timeline, and make
      closing or leaving it land on the desk (or Home once it exists).
      Folds into the retirement of the old entry points after Home if the
      cause is the old new-agent flow. Done with that retirement: the cause
      was the compose surface a cold start opened whether or not anyone
      asked for it, which then sat in the timeline as the only thing to
      fall back to. Cold start now lands on Home and opens no draft; the
      draft page exists only while `n a` composes, and discarding it takes
      it out of the timeline.
- [x] 1.20 Line layout, done with 2.4. Decided by the user: `<name>: <body>  <time>`.
      No time column on the left, no padded author column, no tab after
      the name: the name, a colon, one space, the body. The time goes on
      the right, muted, after the body: at the end of the last line of
      the body, separated by two spaces (assumed; the user said "right
      side", and a true right-aligned column is not text). Continuation
      lines of a wrapped or multi-line body indent two spaces; reaction,
      reply-count, file and unfurl lines under a message indent the same.
      Day rules and system lines unchanged. Replaces the column layout of
      1.4; update its screenshot.
- [x] 1.19 Custom emoji (`:forrest_gump_wave:`) stay as the shortcode in
      muted text until the image inlay (1.13) lands, then render inline
      through it.

## Phase 2: unread, position, and moving around

- [ ] 2.1 Unread rule. Now none. Rho: `── new ──` at `last_read`; opening a
      conversation puts the cursor on the first unread line, not the
      composer and not the top. `G` still goes to the end. The rule stays
      until the surface is closed.
- [x] 2.2 Read marking, decided for now: on open, as Slack does. The user
      holds this loosely; different read semantics may come later, so keep
      the mark call in one place.
      Landed: `Session::open` fetches with `mark_read`, and `mark_read` is the one call that marks (`session.rs`).
- [ ] 2.3 Following the tail. Pinned at the bottom, a new message keeps the
      view at the bottom. Scrolled up, the view does not move and the
      surface's status segment shows `3 new`; `G` clears it.
- [x] 2.4 Done: `crates/rho-transcript` keyed incremental transcript, scroll fill,
      no `shift-p`, screenshots 24-*. History fills as the reader scrolls, decided by the user: a
      conversation or thread must feel complete. When the cursor or
      viewport comes within a screen of a gap (the top of the loaded run,
      or a hole left by downtime), the session fetches the page that fills
      it, one page in flight at a time, and the cursor stays on the line
      it was on while the lines above grow. History-begins makes the top a
      no-op, never a request. There is no manual form: `shift-p` and its
      echo are removed, the user wants no "load older" to exist. Every
      re-render is incremental, decided by the user: a keyed transcript
      primitive (new crate `rho-transcript`, prior art in rho-gui's
      `TranscriptModel`) where each message owns an anchored range; a
      prepend, append, edit, reaction, or deletion edits only its own
      range, so cost is proportional to what changed and cursor, scroll,
      highlights and image blocks outside it survive. Whole-text render
      plus diff is rejected as O(conversation) per event. Blocks 2.8 and
      2.9, so it comes before 2.10. This is
      still on demand under the budget rule: the web client fetches
      exactly the same page when a user scrolls to it.
- [ ] 2.5 Next unread conversation from inside a conversation: `shift-n`,
      the same key Zulip uses in rho. Wraps to the list when nothing is
      unread.
- [ ] 2.6 Conversation list rows. Now `#design @1`, `@ada unread`, a rule,
      then the rest. Rho: `label  @2 · 5 new  14:27`; unread first, then by
      recency; muted conversations at the bottom under a rule. No
      last-message preview. Presence is deferred.
- [ ] 2.8 Live updates, reported broken by the user in real use. Phase 0
      found the causes against the fake: (a) `events.rs::parse` drops
      `message_changed`, `message_deleted`, and every `reaction_*` frame,
      so edits and deletions never reach the buffer; (b) `client.counts`
      is fetched once in `load_roster` and never again, and `note_message`
      does not touch a row's unread or mention counters, so the list is
      stale until restart; (c) reactions have nowhere to land until 1.7.
      A plain new message does append live when the socket is up. Rho: all
      frame kinds parsed and applied in place; the list's counters move on
      every frame and refetch on reconnect; the user's own sent message
      shows at once (the fake now echoes it like Slack); and because the
      real-Slack case may be a dead socket, an open conversation re-syncs
      its tail on Ready, on reconnect, and on every `activity.feed` poll,
      so a dead socket degrades to the poll, never to silence.
- [x] 2.9 The deal is the conversation, done 3 Sep (`99f7eafc`): thread or
      channel opens as the deal view, pinging message tinted with the cursor
      on it, deal keys bound at the conversation's depth, escape to normal,
      bar `#design  needs reply · 0m`; a verdict on a card already quieted by
      the read cursor counts as handled (2.10 removes that gap). Original:
      Now a dealt Slack card is
      a one-line label with nothing behind it; only Page cards open their
      surface as the deal view (`DealCardIdentity::Inbox` with
      `DealerInboxSource::Page`). Rho: dealing a Slack obligation opens the
      conversation surface as the deal view the way an agent card opens
      its transcript: the thread surface when the ping is in a thread, the
      channel or DM otherwise, scrolled so the pinging message is in view
      with the cursor on it and the message itself in a highlight class
      that stays until the surface closes. Enough history above it to
      read what led to it (the surface's normal tail, scrolling for more).
      Deal keys (`d`, `x`, `s`, `S`, `t`, `f`, `q`) work on that surface
      exactly as on an agent surface, and a verdict closes it and moves on.
      The card line still shows in the queue; the surface is what you look
      at. Also seen by the user: `escape` on the dealt inbox surface does
      not leave deal mode for normal mode. On the conversation surface
      `escape` must go to normal mode exactly as on an agent transcript
      (the `ExitDealMode` contract), and the same must hold on any deal
      surface; find why the inbox surface swallowed it and say so. Until 2.9 lands, the interim inbox deal surface (seen by the
      user as a bare message body with nothing around it) must at least
      carry a header line `David · #design · Thu 3 Sep 10:02` above the
      text and the conversation label in the status segment; that part is
      Phase 1 work under 1.1. The deal bar for a Slack deal, seen by the
      user on 3 Sep: `Dawid (dpc), Shaurya / Started looking at this. Some
      initial comments for your … | waiting on you · 1.9h · from Dawid
      (dpc), Shaurya`. Wrong three ways: the message excerpt repeats what
      is on screen, "from …" repeats the conversation name on the left,
      and "you" is banned. Decided: left segment is the conversation only
      (`#design`, `#design › thread`, or the DM names); the state segment
      is the state then its age, `needs reply · 1.9h` when the last word
      is theirs and `replied · 1.9h` when it is the reader's; nothing
      else. The queue card label follows the same words.
- [x] 2.12 Done 3 Sep (`f3004e8d`): `newer messages not loaded` rows, forward
      fill one page per action, fake models `oldest` alone as forward paging,
      cursor-per-frame purchase rule (screens 48–50). A hole in the history is drawn, never hidden. Seen on 3 Sep
      (screen 46): the ping's prefetched window and the conversation's tail
      rendered as one continuous run, message 250 then a day rule then 481,
      with about 230 messages missing and nothing on screen saying so. The
      transcript draws a gap only at the top (`Row::Gap`). Decided: the
      surface opens at the mirror chunk containing the dealt message, not
      the newest; a `newer messages not loaded` row sits under a chunk that
      does not reach the live end, and between any two loaded chunks; a
      scroll to the bottom of such a chunk fetches exactly one page forward
      (`oldest` set, `latest` unset), the same one-page-per-user-action rule
      as `gg`, filled with the existing `mirror_island` gap records. Real
      Slack answers `oldest` without `latest` with the messages closest to
      `oldest`, paging forward (docs.slack.dev, conversations.history), so
      the "after" half of the ping prefetch is right and the fake is wrong:
      the fake must model that, and the both-sides test must run on a
      channel long enough to tell (500 messages, ping in the middle, context
      visible below the ping in the screenshot).
- [x] 2.13 Marking the old backlog done. Asked by the user on 3 Sep. On the
      Slack list, a command `mark read before` opens a minibuffer taking an
      age or a date (`7d`, `2026-08-15`, default `7d`) and shows the count
      it would touch before acting (`14 conversations · 3 threads · enter`).
      Acting means the real Slack action a person would take:
      `conversations.mark` to the latest message of every conversation
      whose newest message is older than the cutoff, one request per
      conversation, no pagination beyond what the mirror already holds;
      threads the user is in get `subscriptions.thread.mark` the same way.
      In rho, reading is not a verdict (decided 3 Sep, see 2.14), so the
      command also writes a `done` verdict on every open Slack card whose
      latest message is older than the cutoff: in the Slack verdict store
      today, on the thread node once slice 2 lands, one log entry each,
      undone as a batch with `shift-u`. Nothing newer than the
      cutoff is touched, ever. Journal `SlackMarkedReadBefore { cutoff,
      conversations, threads }`. The fake counts the mark calls; the test
      asserts exactly one per old conversation and zero for newer ones.
      Screenshot the confirmation line and the list after.
      Landed 3 Sep. `m` on the Slack list (also `space shift-s m`) opens
      `mark read before (7d):`; the line under it counts what the input
      would touch (`1 conversation · 0 threads · enter`) and recounts as it
      is typed. Acting sends one `conversations.mark` per old conversation
      and one `subscriptions.thread.mark` per old followed thread, then
      writes a done verdict on every open thread card older than the
      cutoff; `shift-u` reopens that whole batch as one undo. Conversations
      with nothing unread are left out of the plan: a read conversation is
      not backlog, and marking it would spend a request to change nothing.
      Screenshots: /tmp/rho-slack-ux/screens/{mark-prompt,mark-done}.png,
      the count line and `marked read before 2d: 1 conversation · 0 threads
      · 1 closed` with `#random` moved out of the unread group.
- [x] 2.14 Verdicts are the user's keys only. Landed 3 Sep. Combined
      ping-plus-agent deal screenshot skipped: staging an agent means a
      live model call; the ordering test is the proof.
      Original: Decided by the user on 3
      Sep, recorded in SLACK-DESIGN "A Slack thread is shaped like an
      agent" and "A Slack card outranks an agent of the same wait". Three
      behaviours change. The user's own message no longer quiets a card
      (`Change::Quieted` on `from_you` in `note_message`/`note_loaded`):
      it flips the state word to `replied` and moves the card to the fyi
      curve (0 at the reply, minus a third per day, gone under the floor
      after 3 days), still open, still closable with `d`. `mark_read`
      (rho reading, `channel_marked`, `im_marked`) clears unread counts
      and nothing else; the "reading elsewhere is a verdict too" branch
      goes. A `needs reply` card scores like a blocked agent with a 1.1
      head start and the same 12 per day slope, replacing the pace-0 todo
      curve plus `age_days`; an agent the user just spoke to (recency bonus
      up to 1.5, fed by the user's sends and surface opens, gone within the
      hour) still ranks above it. Tests: own reply keeps
      the card and relabels it; a read on another client keeps the card;
      a reply from them after `d` re-raises; a 2h-old ping outranks a
      2h-old blocked agent but not one the user messaged 10 minutes ago.
      Screenshot a deal queue holding both.
- [x] 2.15 Discard is Slack's ignore thread. Decided by the user on 3 Sep,
      recorded in SLACK-DESIGN under "A Slack thread is shaped like an
      agent". `x` on a thread card calls `subscriptions.thread.remove`
      for that thread (one request, failure lands as a notice, the rho
      discard stands either way); `thread_unsubscribed` on the websocket,
      and a thread the feed stops naming after an unfollow elsewhere,
      discard the card in rho. Rho stores no subscription state. Follow
      again in Slack (`thread_subscribed`) re-raises only on the next
      message from someone else. Fake records the unfollow calls; tests
      cover both directions. Journal `SlackThreadIgnored { thread, by:
      Rho | Slack }`.
      Landed 3 Sep. `x` on a thread card sends one
      `subscriptions.thread.remove` and the discard stands whatever Slack
      answers; a failure is a notice saying the thread is still followed
      there. The other way, `thread_unsubscribed` closes the card, and so
      does a thread the follow list stops naming on the next connect, which
      is the unfollow that happened while rho was off. An unfollow from
      Slack's side drops the thread from the model entirely, so following
      it again there raises nothing until somebody writes in it. Rig: `x`
      on #design sent exactly one remove and dealt the next card; a
      `/control` unsubscribe for #random closed that card with no keystroke
      (screens/{discard,unsub,after-unsub-home}.png).
      Undo re-follows: `shift-u` after `x` sends one
      `subscriptions.thread.add` and the card is dealt again as it was,
      since the discard is the unfollow and a half-visible undo is worse
      than none. rho's own `x` therefore keeps the thread's words while
      dropping the follow, so there is something to bring back; a failure
      is a notice saying the thread is still ignored in Slack. The incoming
      direction keeps no undo entry.
- [x] 2.16 A thread reply the user never saw. Landed 3 Sep. Original: Reported by the user on 3
      Sep: a reply in a thread they had posted in raised nothing, no chime,
      no lamp, not in a manual deal. Two causes in `model.rs`, either one
      enough. First, `participated` is in memory only, filled when rho
      itself sees the user's message, so after a restart, or when the user
      replied from the phone, rho does not know the thread is theirs and
      `reason_for` calls the reply channel traffic. Second, `note_message`
      inserts into `seen` before it decides the reason, so that live reply
      poisons dedup and the activity feed's `thread_v2` item for the same
      `ts`, which would have raised it, is dropped as a duplicate. Fix
      both: `seen` records a message only once it has a reason (channel
      traffic is never "seen"; the feed stays the truth it was designed to
      be); and "threads that are mine" comes from Slack, not from what rho
      happened to watch (the user's rule, 3 Sep: state lives in Slack so
      every client agrees). On connect, `subscriptions.thread.getView`
      lists the followed threads with their `last_read`; live,
      `thread_subscribed` / `thread_unsubscribed` / `thread_marked` keep
      it current (emacs-slack `slack-all-threads-buffer.el`,
      `slack-thread-event.el`). Slack follows a thread for the user when
      they post or are mentioned, so a phone reply counts. The
      `participated` set and any mirror copy of it go. Tests: a live reply
      in a followed-but-never-seen thread raises; the same reply in an
      unfollowed thread does not; the feed item after the live reply is a
      no-op, not a drop. Comes right after 2.14, since 2.14 removes the
      third way this card could have gone quiet (a `channel_marked` from
      the phone).
- [x] 2.10 No inbox in between, decided. Today a Slack obligation is copied
      into the rho inbox (`SlackItems`, `InboxKind::Slack`,
      `SourceReference::SlackThread`) and dealt from there. Rho: the dealer
      takes Slack candidates straight from the session's store, as agent
      cards come from the registry; the inbox is not written or read for
      Slack. Verdict state (done, discard, snooze until, todo) lives in the
      Slack store on disk, keyed on thread plus latest timestamp, so a
      verdict survives a restart and a newer message voids it. Known defect
      this removes: today the card's text is rendered once at ingest,
      before the roster lands, so on a cold start a mention reads
      `@someone` instead of the name; rendering at display time from the
      model fixes it. Filing keeps
      creating the machine-owned desk node. Undo (eng-5pha's verdict undo
      stack) must cover this store the way it covers desk marks: add a
      variant, do not bypass it. Human-entered inbox items are out of scope.
      Landed with tree slice 2: the dealer takes Slack candidates from the session store and `SlackItems`/`InboxKind::Slack` are gone.
- [x] 2.11 Local mirror in rho-db, done (`slack.redb`, derived chunks, gap
      records, history-begins flag, ping prefetch is two history calls, 20 before and
      20 after the ping, plus one replies call; the user chose both-sided
      context over the saved request). Offline screenshots 211-*. Original text: today nothing is cached:
      history is fetched on open and lives only in memory. Rho: a GUI-owned
      redb file `~/.local/state/rho/slack.redb` (0600) holding users and
      avatar hashes, conversations and labels, messages per conversation in
      ts order with reactions, edits, and deletions applied, thread replies
      under their parent, the activity cursor, per conversation
      `last_read`, and 2.10's verdict state; 2.10's store is this file, not
      a separate one. Every surface renders from the mirror first and
      refreshes behind it; a conversation fetches only what is newer than
      its cached newest ts; restart shows the mirror before the socket is
      up; offline, everything cached is readable and sending fails loudly
      into the composer. Fill is on demand only: history when a
      conversation is opened, older pages as the reader scrolls, tails only for
      open conversations and those the feed named; no background walk of
      history, no fan-out over the list, no re-fetch of what the mirror
      holds. Decided: a ping named by the activity feed triggers one bounded
      fetch at ingest, one history call for a small window around the
      parent plus one replies call if it is a thread, so the 2.9 deal
      renders from the mirror with no network wait; that is what the web
      client does when the notification is clicked. Budget rule from the
      user: request volume at or below Slack's web client for a power
      user; every fetch must map to a user action the web client would
      make. The request pattern must look like a person reading; the fake
      counts calls per method and a test asserts that opening the list
      fetches no history. Screenshot: the list and a conversation open
      with the fake stopped.
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
- [x] 3.4 In a thread surface `enter` sends to the thread. "Also send to
      channel", editing, and deleting stay deferred.
      Landed: `Session::send` posts with the source's `thread_ts`, covered against the fake in `tests/transport.rs`.

- [x] 3.5 Sending images. Asked by the user on 3 Sep. A paste of image
      bytes into the composer, or a file path dropped on it, attaches the
      image: the composer shows a thumbnail chip (`image.png · 320 KB`) above
      the text, `enter` uploads and posts the text as the message with the
      file attached, through `files.getUploadURLExternal` and
      `files.completeUploadExternal` (`files.upload` is retired). The sent
      message appears once as the same inline image every other message
      gets, from the mirror, never rendered from local bytes. The fake
      serves both upload endpoints and stores the bytes so the round trip is
      testable; a failed upload fails loudly into the composer with the
      text kept, like a failed send. Journal `SlackFileSent { conversation,
      bytes }`. Screenshot the chip and the sent result on the fake.
      Landed 3 Sep. Three ways in, all the same attachment: `ctrl-v` in the
      composer takes clipboard image bytes (named `image.png` after the
      format, since a clipboard carries no filename), a file dropped on the
      conversation takes the path, and `space s a` (attach file…) is the
      keyboard's way to the same thing, which is what the rig can drive.
      One picture at a time: a second attachment replaces the first and the
      notice says so, rather than a chip quietly changing under the reader.
      The chip is one muted line kept between the transcript and the
      composer (`mock.png · 49 KB`), and `space p c` (clear images) drops
      it. `enter` uploads through `files.getUploadURLExternal`, a POST of
      the bytes to the URL Slack hands out, and
      `files.completeUploadExternal`; the message with the file on it comes
      back over the socket and is drawn from the mirror like anyone else's
      picture, never from the local bytes. A refusal puts the chip and the
      words back in the composer and the error on the transcript's notice
      line. The fake serves all three steps, keeps the uploaded bytes, and
      serves them back at `/files/`, so the picture in the transcript is
      the picture that left; it reads the PNG header for `original_w`/`_h`,
      which is what sizes the box. Tests: the upload round trip (bytes
      served back byte-for-byte, one call to each endpoint), a refused
      upload posting nothing, and the chip's wording. Rig:
      screens/35-02-prompt.png (the prompt), 35-03-chip.png (the chip),
      35-04-sent.png (the picture in the transcript). Budget: two calls for
      a picture message and no `chat.postMessage`, journal
      `slack_file_sent` once with the channel named and the byte count.
      Deviation to note: the paste and drop paths cannot be driven from the
      headless rig (no clipboard tool, no second wayland client to drag
      from), so the prompt is what the screenshots use; the three paths meet
      at the same `attach`.
      Landed 3 Sep, as the account in this item records.

- [x] 3.6 Editing a sent message. Landed 3 Sep; escape on an open edit
      cancels and leaves insert in one press (ruled 3 Sep, done with 3.5).
      Original: Asked by the user on 3 Sep. On one of
      the reader's own messages, `e` in normal mode (the Slack conversation
      context, outside a deal) opens the composer prefilled with that
      message's text and the message tinted while the edit is open;
      `enter` posts `chat.update`, `escape` cancels and restores the
      composer to whatever it held. `up` in an empty composer edits the
      reader's last message, the Slack habit. The updated message
      re-renders from the mirror with the existing `(edited)` marker, one
      item, nothing else redrawn. Not the reader's message: `e` does
      nothing and says so in a notice. The fake serves `chat.update` and
      the socket `message_changed` event so the round trip is the real one.
      Journal `SlackMessageEdited { conversation, ts }`. Screenshot the edit
      open and the result on the fake.
      Landed 3 Sep. `e` in the conversation's normal context (outside a
      deal, where the deal keys still own the row) and `up` on an empty
      composer both open the same edit: the composer holds the message's
      own text, whatever was half-written is put aside, and the message
      carries the dealt tint while the edit is open. `enter` posts
      `chat.update` and gives the composer its held text back; `escape`
      does the same without posting and leaves insert in the one press
      (ruled 3 Sep: two escapes to get out is a trap), and with no edit
      open it falls through to vim as before. What repaints is Slack's
      own `message_changed` coming back down the socket, so the screen
      shows the edit that landed: one item replaced, the `(edited)` marker
      already in the renderer. Someone else's message says so and does
      nothing (`slack: only your own messages can be edited`). The fake
      serves `chat.update` and pushes the socket event, and the transport
      test drives the whole round trip. Rig: screens/36-16-edit-open.png
      (`up`), 36-22-edit-open-key-e.png (`e`), 36-17-edited.png (the
      result), 36-19-cancelled.png (`escape`), 36-21-notyours.png (the
      notice); esc-02-open.png / esc-03-cancelled.png show the one-press
      cancel putting the half-written line back with the mode in normal. Budget: one `chat.update` for the edit and nothing else,
      journal `slack_message_edited` once with the channel named.

## Phase 4: status and health

- [x] 4.1 Status bar. The surface segment reads the conversation label, and
      `· thread` inside a thread. Nothing about the connection while it is
      healthy; degraded shows the lamp and the notice (already). Found in
      Phase 0: the `disconnected` seen in the bar is rho's own daemon
      status, not Slack. Keep it that way, and make sure a Slack outage
      never borrows that word: Slack's state is the lamp plus a notice
      that names Slack.
      Landed: the surface segment is `Session::label`, which appends `· thread` for a thread source; a Slack outage shows the lamp and a notice that names Slack, never `disconnected`.
- [ ] 4.2 Typing indicators and presence: deferred, unchanged.

## Phase 5: one key table

- [ ] 5.1 The list: `enter` open, `s` narrow, `shift-n` next unread, `q`
      close. The conversation: `enter` open the thread under the cursor or
      the file link, `i` compose, `shift-n` next unread,
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
