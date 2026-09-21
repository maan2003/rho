# Slack workflow parity

Goal: use Rho as the only Slack client for everyday messaging. Match Slack's
familiar workflows first, preserving Rho's conversation editor/buffer and Vim
interaction. Multi-workspace support is explicitly excluded. Composition stays in the same
editor/buffer model too; controls decorate that editor rather than replacing it
with a separate message form.

A box closes only after implementation and QA against the fake Slack server.
Use real client paths, not GUI-side mocks; inspect rendered affected states.
Record checks and remaining limitations below. Existing historical checklist
claims are not evidence that a workflow is complete.

## Navigation and discovery
- [x] Persistent channel/DM sidebar, unread badges, favorites, quick switcher, back/forward.
- [x] Find people, start a DM, create group DMs, browse and join channels.

## Finding things
- [x] Workspace and conversation search, useful filters, pagination, correct thread/context landing.

## Sending and unfinished work
- [ ] Obvious composer, mentions, formatting, multiple attachments, clear sending/failure/retry.
- [x] Drafts survive navigation and restart and can be found again.

## Threads and message actions
- [ ] Parent context, follow/unfollow, unread replies, also send to channel.
- [x] Edit/delete, reactions, copy link, forward, mark unread, save for later.

## Awareness and content
- [ ] Mentions and thread activity, notifications, clear connection failures.
- [ ] Files, previews, inline custom emoji, interactive app messages.

## QA acceptance
- [x] Navigate with mouse and keyboard while conversation remains an editor buffer.
- [x] Find an older result beyond page one and a reply inside a thread.
- [x] Message a new person, join a channel, then send and receive through the fake.
- [x] Resume drafts after restart; send multiple attachments; recover from refused sends.
- [ ] Exercise message/thread actions and confirm server state, not only rendered optimism.
- [ ] Inspect screenshots of sidebar, search, composer, thread, emoji, errors and activity.
- [ ] Run combined relevant Rust tests and check the final diff.

## Explicitly tracked larger Slack surfaces
Huddles and canvases remain known broader Slack parity gaps, outside the everyday
workflow checklist agreed above. Do not present this milestone as full Slack
product parity. Multi-workspace is excluded by the user.

## Evidence and remaining work
Implementation in progress; acceptance remains open until combined QA.
- Navigation: `cargo test -p rho-slack`: 200 passed; `cargo test -p rho-gui --lib slack_tests`: 25 passed, 1 benchmark ignored (before other feature integration).
- Fake transport proves new DM reuse, group membership, unjoined-channel discovery/join, and send destination.
- Fake-backed GUI test exercises multi-recipient completion and sends `hello` to the resulting group.
- Inspected initial sidebar, channel, group composer, and channel-directory captures. Fixed missing text color and delayed insert-mode activation found in QA.
- Favorites reopen test confirms persistence, removal, and scope isolation.
- Saved-for-later is Rho-local: Slack has no supported current Later API. This must be labeled rather than implying cross-client synchronization.
- Combined screenshots and final test totals will replace intermediate evidence on completion.

### Integrated navigation QA
- `cargo test -p rho-gui --lib slack_tests`: 29 passed, 1 performance benchmark ignored.
- `cargo test -p rho-slack`: 211 passed across unit/config/mirror/transport suites at first integration.
- Inspected `/src/slack-qa/screens/header6.png`, `thread6.png`, `thread-followed6.png`,
  `starred6.png`, `back6.png`, and `forward6.png`: mouse selection opens the selected
  message's thread, parent/replies remain an editor buffer, favorite moves into Starred,
  muted room is subdued, back/forward restores the correct conversation.
- Fake `subscriptions.thread.getView` returned C1 / 1789809000.000000 after clicking
  Follow and an empty list after Unfollow. Both UI states were inspected.
- Remaining acceptance stays open for integrated search, composer, emoji and app followups.


### Integrated workflow QA
- Full GUI pass before final followups: 346 passed, 4 ignored, 1 regression
  (`visualization_refs_become_inline_editor_blocks`) found and being corrected.
  Do not treat this intermediate run as green.
- Latest Slack GUI subset: 38 passed, 1 performance benchmark ignored.
- Search page-two mouse selection opened backlog message 459; pagination did not
  navigate away. File search returned `review.pdf`, and clicking downloaded it
  into the state cache. External opening is unavailable in this QA environment
  (`xdg-open` is absent).
- Draft text survived a full GUI restart. Multi-file send reached fake history
  with distinct 17-byte and 34-byte files. Refused text send displayed Retry,
  retained exact text, and posted it once the fake refusal was cleared.
- Session-owned send tests cover closing the original view, reopening the same
  source during the request, duplicate gating, and durable pending cleanup.
- Edited message content and forwarded permalink were checked through fake
  history. Save displayed the persisted author/snippet; reactions and delete
  reached the fake. Mark-unread navigation and persistence are covered by the
  fake-session regression.
- Modern modal mouse opening, all four fixture inputs, refused submission, and
  corrected successful submission were exercised through real socket/API paths.
  The UI reported `Deployment queued`. Error visibility/correction is still in
  final QA.
- Notification dedup/focus suppression tests pass. Actual OS notification
  delivery is unavailable: the headless session has no notification daemon.
