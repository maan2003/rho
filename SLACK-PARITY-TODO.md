# Slack workflow parity

Goal: use Rho as the only Slack client for everyday messaging. Match Slack's
familiar workflows first, preserving Rho's conversation editor/buffer and Vim
interaction. Multi-workspace support is explicitly excluded.

A box closes only after implementation and QA against the fake Slack server.
Use real client paths, not GUI-side mocks; inspect rendered affected states.
Record checks and remaining limitations below. Existing historical checklist
claims are not evidence that a workflow is complete.

## Navigation and discovery
- [x] Persistent channel/DM sidebar, unread badges, favorites, quick switcher, back/forward.
- [x] Find people, start a DM, create group DMs, browse and join channels.

## Finding things
- [ ] Workspace and conversation search, useful filters, pagination, correct thread/context landing.

## Sending and unfinished work
- [ ] Obvious composer, mentions, formatting, multiple attachments, clear sending/failure/retry.
- [ ] Drafts survive navigation and restart and can be found again.

## Threads and message actions
- [ ] Parent context, follow/unfollow, unread replies, also send to channel.
- [ ] Edit/delete, reactions, copy link, forward, mark unread, save for later.

## Awareness and content
- [ ] Mentions and thread activity, notifications, clear connection failures.
- [ ] Files, previews, inline custom emoji, interactive app messages.

## QA acceptance
- [ ] Navigate with mouse and keyboard while conversation remains an editor buffer.
- [ ] Find an older result beyond page one and a reply inside a thread.
- [ ] Message a new person, join a channel, then send and receive through the fake.
- [ ] Resume drafts after restart; send multiple attachments; recover from refused sends.
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
