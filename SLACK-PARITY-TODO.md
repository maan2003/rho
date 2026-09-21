# Slack workflow parity

Goal: use Rho as the only Slack client for everyday messaging. Match Slack's
familiar workflows first, preserving Rho's conversation editor/buffer and Vim
interaction. Multi-workspace support is explicitly excluded. Composition stays in the same
editor/buffer model too. Commands use Vim keys, transient menus, and minibuffer
prompts; the sidebar is a compact editor-backed awareness pane, without command
rows, toolbar, or composer buttons. This supersedes the
original sidebar/button presentation recorded in the historical QA below.

A box closes only after implementation and QA against the fake Slack server.
Use real client paths, not GUI-side mocks; inspect rendered affected states.
Record checks and remaining limitations below. Existing historical checklist
claims are not evidence that a workflow is complete.

## Compact message and emoji QA

- Rho OKSolar P3 body text measures 6.509:1 against its editor background.
  Sidebar unread/mention state uses color, not bold weight; the conversation
  has a 4px inset and an avatar gutter.
- Lightly rounded avatars are consistently 1.25 lines high. Slack's 192px
  images are preferred for HiDPI. Initials fill pending or unavailable avatars;
  the gutter and folded author rows do not wait for the image download.
  Message slots reserve at least 1.25 lines for avatars, without blank source
  rows. Only single-visual-row messages are vertically centered. Multiline,
  wrapped, and footer-bearing messages start at the avatar's top and flow down.
  Same-author follow-ups (including edited messages) share
  an avatar within five minutes on the same day. They need no avatar minimum
  and use quarter-line gaps; author groups retain half-line gaps.
  Wrapped paragraphs, leading code, and image attachments retain a common text margin.
  Names remain available for copy/search; initials remain on avatar failure.
  Message and last-reply timestamps are hidden.
- Message text renders hanging list indents and bullets, inline-code backgrounds,
  semantic mention highlights, and combined bold/italic/strike/underline.
  Paragraph gaps are half a line; code whitespace and literal HTML stay literal.
- Edited messages use a display-only `✎` suffix, with no extra source row.
  Reactions and thread counts share one muted footer; own reactions stay highlighted.
- Date headings have a half-line gap below them and are centered by anchored editor row alignment, not replacement
  blocks. Text, selection and mouse hits use the same alignment.
- Desktop Slack notifications are disabled. Mentions, DMs and followed-thread
  replies still update in-app cards, unread state and the activity lamp.
- Fixed the shared WGPU BGRA/RGBA upload conversion; avatars, custom emoji,
  and other images retain their source colors.
- Standard color emoji are bundled in Rho via Noto Color Emoji. Slack alias
  names come from a pinned, compact iamcal emoji-data table; emoji-presentation
  symbols prefer the color font rather than a monochrome symbol face. Slack
  `::skin-tone-2` through `::skin-tone-6` sequences render their variants,
  including supported joined emoji. Custom images and aliases stay on the
  bounded Slack asset path.
- Verification: `cargo test -p rho-slack --features ui,fake`: 239 passed.
  `cargo test -p rho-gui --lib`: 370 passed, 4 ignored, including the
  bundled-font, 6.5:1-theme, and Slack rendering regressions.
  `cargo test -p rho-transcript --lib`: 11 passed, 2 ignored.
  After hiding timestamps and disabling popups:
  `cargo test -p rho-gui --lib slack_tests`: 43 passed, 1 ignored.
  The live-event card regression sets a desktop app identity and asserts no
  system notifications for incoming mentions, DMs and followed-thread replies.
  A rendered geometry test checks half-line and quarter-line gaps, centered-row
  mouse hits, unchanged text, and two viewport widths. The pure geometry suite
  passes 4 tests. Root spacing regressions
  also cover prepends, actual soft wraps, hidden fold rows, and attachment blocks. The two atlas upload
  helper tests pass in an isolated Rust harness; the vendored graphics test
  workspace itself is blocked by its existing SQLite dependency conflict.
- Inspected native captures of compact messages, empty and typed composers,
  standard/skin-tone/joined emoji, custom emoji, and corrected image colors:
  `/src/slack-qa/screens/gutter-emoji-final.png` and `gutter-emoji-draft.png`.
- The former centered-date replacement blocks were broken; the original screenshot
  check missed the defect. They remain removed. Inspected anchored centering,
  mixed short/long messages, attachments, and the compact backlog:
  `/src/slack-qa/screens/spacing-mixed-final.png`, `spacing-short-long-final.png`,
  and `spacing-backlog.png`.
- Row-spaced editors omit minimaps, whose separate display map cannot mirror gaps.
  Ordinary editor layouts are unchanged.
- Cold-load QA holds avatar responses at the fake server, then releases them.
  The first populated frame already conceals names and reserves gutter width.
  The regression asserts unchanged display text, height and gutter width after
  release, including another author with no avatar. Inspected both states:
  `/src/slack-qa/screens/avatar-loading.png` and `avatar-loaded.png`.
- Compact-avatar QA verifies 1.25-line images and initials, centered single-line
  text, and top-aligned multiline/wrapped text. Inspected
  `/src/slack-qa/screens/compact-avatar-loading.png` and `compact-avatar-loaded.png`.
  Geometry tests also check adjacent short-message gaps and mouse hit positions.
- Grouped-message QA verifies consistent first-line alignment, shared avatars
  for edited follow-ups, quarter-line group spacing, date padding, and a single
  reaction/thread footer. The inline edit marker preserves Markdown source.
  Inspected `/src/slack-qa/screens/grouped-footer-loading.png`,
  `grouped-footer-loaded.png`, and `grouped-footer-thread.png`. Enter on the
  combined footer opens the matching thread.
- Single-row alignment regression resizes the same message from wrapped to
  one visual row and back. Only the one-row state is centered. Inspected
  `/src/slack-qa/screens/single-row-align.png` with short, multiline, wrapped,
  and footer-bearing messages.
- Message-text QA uses the app-store comparison message at full and narrow
  widths, plus nested lists, combined styles, literal HTML, and code whitespace.
  Inspected `/src/slack-qa/screens/message-text.png`, `message-text-narrow.png`,
  and `message-text-edge-cases.png`. Tests cover message-boundary isolation,
  9./10. marker widths, nested indentation, resizing, and click mapping.
- QA uses only the local fake Slack server. Its avatar and custom emoji
  fixtures are solid-color PNGs, not real profile photographs.

## Navigation and discovery
- [x] Editor-backed channel/DM sidebar and full list, unread/mention counts,
  favorite stars, quick switcher, back/forward, and keyboard pane navigation.
- [x] Discovery and conversation commands available through the Slack transient.
- [x] Find people, start a DM, create group DMs, browse and join channels.

## Finding things
- [x] Workspace and conversation search, useful filters, pagination, correct thread/context landing.

## Sending and unfinished work
- [x] Obvious composer, mentions, formatting, multiple attachments, clear sending/failure/retry.
- [x] Drafts survive navigation and restart and can be found again.

## Threads and message actions
- [x] Parent context, follow/unfollow, unread replies, also send to channel.
- [x] Edit/delete, reactions, copy link, forward, mark unread, save for later.

## Awareness and content
- [x] Mentions and thread activity in-app, clear connection failures; no desktop notifications.
- [x] Files, previews, inline custom emoji, interactive app messages.

## QA acceptance
- [x] Navigate with mouse and keyboard while conversation remains an editor buffer.
- [x] Find an older result beyond page one and a reply inside a thread.
- [x] Message a new person, join a channel, then send and receive through the fake.
- [x] Resume drafts after restart; send multiple attachments; recover from refused sends.
- [x] Exercise message/thread actions and confirm server state, not only rendered optimism.
- [x] Inspect screenshots of sidebar, search, composer, thread, emoji, errors and activity.
- [x] Run combined relevant Rust tests and check the final diff.

## Explicitly tracked larger Slack surfaces
Huddles and canvases remain known broader Slack parity gaps, outside the everyday
workflow checklist agreed above. Do not present this milestone as full Slack
product parity. Multi-workspace is excluded by the user.

## Verification evidence

- Combined command:
  `cargo test -p rho-gui -p rho-slack -p rho-window -p rho-journal -p rho-fake-slack --features rho-slack/ui,rho-slack/fake`
  — 646 passed, 0 failed, 4 ignored. Log: `/src/slack-qa/verification.log`.
- Explicit `one_arriving_message_costs_what_it_touches -- --ignored --nocapture`
  passed separately: 306 sidebar rows cost 6.66 ms for the listing and 8.54 ms
  for the conversation, versus 3.63 ms / 9.52 ms with six rows. These are this
  debug QA run's measurements, not a production latency guarantee.
- `cargo fmt --all -- --check` and `git diff --check` passed.
- Fake-backed GUI/transport tests verify new DM reuse, group membership,
  directory/join, destination-correct sends, thread-context search landing,
  scoped versus workspace search, pagination, and separate file-search identity.
- Mouse QA opened backlog result 459 on page two without pagination navigating
  away. File search found `review.pdf`; clicking downloaded it into the cache.
- A draft survived full GUI restart. Two files reached fake history with
  distinct 17-byte and 34-byte contents. A refused send retained exact text,
  displayed Retry, and posted it after the refusal was cleared.
- Session-owned send tests cover closing/reopening the composing view during
  an in-flight request, duplicate gating, durable pending cleanup, later typing,
  partial upload failure, and restart recovery without automatic replay.
- Edits, reactions, forwarding, deletion, follow/unfollow, broadcast, and
  mark-unread were checked against fake state. Clipboard copy matches the
  server permalink. Saved inventory retains author/snippet and refreshes live.
- Stopping the fake exposed a persistent connection reason; restart reconnected,
  caught up, and cleared it without resetting the accumulated client mirror.
- App QA exercised button confirmation/cancel, external-select suggestions,
  modern modal required/server validation, retained correction text, successful
  submission (`Deployment queued`), and Escape (`views.close` reached the fake).
  Transport tests cover all implemented selector/state shapes and view updates.
- Rendered QA inspected sidebar/navigation, search pages, editor composer,
  attachment chips, thread controls, activity/saved inventories, preview images,
  custom emoji/aliases, formatting, failure/recovery, and app prompts. It found
  and fixed contrast, focus, empty Saved-table initialization, emoji concealment,
  menu mouse/key routing, and a generic visualization-fence regression.
- Representative inspected captures are in `/src/slack-qa/screens/`, including
  `composer-final.png`, `formatting-final23.png`, `modal-required-final.png`,
  `modal-server-error-final.png`, `app-confirmation-final.png`,
  `external-select-final.png`, `connection-failed22.png`, and
  `connection-recovered22.png`.
- Legacy-dialog invalid text/select corrections passed targeted GUI checks.
  Rendered QA showed the retained required-field error; corrected submission
  reached `dialog.submit`. Final `cargo test -p rho-gui --lib slack`:
  54 passed, 0 failed, 1 ignored (the separately executed cost benchmark).
  Log: `/src/slack-qa/final-slack-gui.log`.

## Deliberate limits and environment gaps

- Saved for later is explicitly labeled local to Rho; Slack has no supported
  current Later API. It does not imply cross-client synchronization.
- File+thread-broadcast is explicitly refused rather than silently dropping the
  broadcast flag. Text replies support also-send-to-channel.
- Desktop Slack notification delivery is intentionally disabled.
- External document opening could not be verified because `xdg-open` is absent.
  File download, cache, search, and inline image rendering were verified.
- Some standard Unicode emoji glyphs are absent in the QA font environment.
  Custom image emoji and aliases were rendered and inspected.

## Rho-native presentation correction

The persistent sidebar, conversation toolbar, composer buttons, and search
pagination buttons have been removed. Conversation navigation uses the existing
editor list and quick-switch minibuffer. `Space Shift-S` exposes discovery,
search, inventories, message actions, attachment add/remove, follow, favorite,
and broadcast commands through the existing transient UI. Attachment and send
state remain buffer text. Enter respects the broadcast toggle.

Verified with 55 Slack GUI tests (one timing test ignored), 227 Slack crate
tests, `cargo build -p rho-gui --bin rho-gui`, formatting and diff checks.
The new keyboard regression sends one broadcast reply and one thread-only reply
through the fake server and checks their wire flags. Inspected rendered list,
composition with attachment, transient menu, and multi-page search in the
isolated Wayland GUI. Captures are under `/src/slack-qa/screens/style-*.png`.

## Sidebar awareness restored

The sidebar is again persistent beside desktop Slack conversations and results.
It reuses the full conversation-list editor rather than adding a second consumer
of the session's row-edit stream. Favorite changes redraw their row; unread and
mention counts update live. `Ctrl-W H/L` changes pane focus; Vim motions and
Enter navigate without replacing the conversation until a row is opened.

Verified: 56 Slack GUI tests and 227 Slack crate tests passed, including initial
selection before roster arrival, shared list identity, sidebar Enter routing,
favorite add/remove, and a pushed mention updating the visible list. Build,
formatting, and diff checks passed. Inspected composition and sidebar-focused
states: `/src/slack-qa/screens/sidebar-final.png` and `sidebar-focused.png`.
