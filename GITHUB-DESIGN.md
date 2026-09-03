# GitHub in rho

Status: design. Companion to `SLACK-DESIGN.md`; the same shape wherever the
two overlap, and the same rules (typed journal, no ids in the UI, fake
server on the server side for every screenshot).

Prior art, cloned under `~/src`: `emacs-pr-review` (4.5k lines, 24 GraphQL
documents, the on-demand model), `forge` (10.9k lines, the local-mirror
model), `octo.nvim` (issues and PRs as editable buffers). Rho takes forge's
mirror and pr-review's scope, read side only for now.

## The problem

GitHub is the second place work arrives: review requests, mentions,
assignments, failing checks, comments on threads the user is in. Today
none of it reaches the dealer, and reading a PR means the browser. The
user wants the reading half of a GitHub day inside rho: the inbox as cards,
the PR or issue as a buffer, the diff from the local checkout.

## Decisions

### Client side, in rho-gui, its own crate

`crates/rho-github`: a protocol half (REST and the few GraphQL documents,
the mirror, the model) with no GPUI dependency, and a `ui` feature with the
surfaces, exactly as `rho-slack` is split. No daemon involvement. The
daemon's `octo-server` and `rho-pr-monitor` keep serving engineers
untouched; unifying the two is a later decision, once the mirror exists.

### A classic personal access token, entered by hand

Fine-grained tokens cannot read notifications, so the token is a classic
PAT with `repo`, `notifications`, and `read:org`. One prompt registers it
(`github token:`), stored owner-only at
`~/.local/state/rho/github-credentials.json`, replaced by running the prompt
again. `gh auth token` reuse and GitHub Enterprise base URLs are deferred.

### Ingest is the global notifications feed, nothing else

`GET /notifications` is the whole feed: every reason GitHub has
(`review_requested`, `mention`, `assign`, `ci_activity`, `comment`,
`author`, `state_change`, `subscribed`, `team_mention`, `security_alert`).
Polled on `X-Poll-Interval`, with `If-Modified-Since` set to the exact
`Last-Modified` GitHub returned, so an idle poll is a `304` that costs
nothing against the limit. No `involves:@me` search, no per-repo tracking,
no events feed; if the user is not notified, rho does not know, and that is
the accepted trade for now.

A notification thread is an inbox obligation with a typed source
(`SourceReference::GithubThread { repo, number, kind, thread_id }`), the
reason as the card's state word (`review requested · 2.0d · from
owner/repo#123`), and `updated_at` as the verdict key: a newer update voids
a skip and re-raises the card, as a Slack reply does. Reason drives rank:
`review_requested`, `mention`, `assign` outrank `comment` and `subscribed`,
which outrank `ci_activity` on green and sit below it on red.

Verdicts: done and discard mark the thread read on GitHub
(`PATCH /notifications/threads/{id}`); snooze and todo do not touch GitHub;
`f` files a machine-owned heading under the desk with a `github` tag, title
`owner/repo#123: <title>`.

### A local mirror, so the UI never waits on GitHub

`~/.local/state/rho/github.redb`, owned by the GUI, holds everything rho has
ever fetched: notification threads, issues and PRs, comments, reviews,
review comments, check runs, users, and the ETag per URL. Every list and
surface renders from the mirror first and refreshes behind it; the network
is never on the render path. Refresh is conditional (`If-None-Match` per
URL) and driven by `updated_at`: a thread whose `updated_at` matches the
mirror is not refetched. Avatars cache as in Slack (`github-avatars/`).

Rate limits are read from every response (`X-RateLimit-Remaining`,
`X-RateLimit-Reset`); under 10% remaining, polling slows to the reset time
and the lamp lights with a notice that names GitHub. A `403` with
`Retry-After` (secondary limit) is obeyed exactly. No request is ever
retried in a loop.

### REST first, GraphQL only where REST has no answer

REST for everything that is polled or refreshed, because it has ETags.
GraphQL is allowed but minimised: one query when a PR surface opens, for
`reviewThreads { isResolved }` and `reviewDecision`, which REST does not
expose. Nothing else. GraphQL never polls.

### Diffs come from the local checkout only

If the PR's repository is cloned in a rho workspace, rho fetches
`refs/pull/N/head` (and the base) through the daemon's workspace and shows
the diff in a diff surface built on the vendored `buffer_diff`, with review
comments anchored under their hunks. If the repository is not local, there
is no diff: the surface says `no local checkout of owner/repo` and shows
each review comment with its `path:line` and the `diff_hunk` snippet GitHub
attaches to it. No API diff, ever.

### Surfaces

- The GitHub list: notification threads, unread first by reason rank, then
  by `updated_at`; row `owner/repo#123  review requested  2.0d  title`.
  `enter` opens, `s` narrows, `q` closes, `shift-n` next unread, same keys
  as the Slack list.
- The topic surface, one for issues and PRs: a header block (title, state,
  author, labels, assignees, reviewers, checks summary as one line per
  run), the body, then the timeline in the compact layout chosen for Slack:
  comments, reviews with their review comments grouped by file, commits as
  one muted line each, state changes as muted lines. `enter` on a review
  comment opens the local diff at that hunk when the repo is local; `d`
  opens the whole diff.
- The diff surface: local `buffer_diff` view of the PR, review comments
  inline under their hunks, `ctrl-k` back to the topic.

Read only. No composing, no reviewing, no merging in this pass; those are
the next design, once reading is right. Anything that needs a write today
goes through the browser or to an engineer.

### Journal

Typed events with the thread named, never numbered:
`GithubConnected`, `GithubDisconnected { reason }`,
`GithubItemIngested { thread: GithubThread { repo, number, kind }, inbox_id }`,
`GithubThreadRead { thread }`.

### The fake

`rho_github::fake`: a real HTTP server serving the REST endpoints rho calls
(`/notifications` with `Last-Modified`, `304`, and `X-Poll-Interval`;
threads; issues; pulls; comments; reviews; review comments; check runs;
users; the GraphQL endpoint for the one query), with a `/control` route to
push new notifications and comments while rho is open, and rate-limit
headers it can be told to lower. All mocking is server side; the client is
never modified for a test.

## What stays the same for the human

The dealer, verdict keys, deal history, inbox, filing, the desk. GitHub
rows appear only where the user filed them.

## Deliberately deferred

- Writing anything: comments, reviews, merges, labels, resolving threads.
- `involves:@me` search and per-repo tracking.
- API diffs, and `gh auth token` reuse.
- GitHub Enterprise.
- Unifying with `octo-server` and `rho-pr-monitor`.
- Search across the mirror.

## Symptoms to watch for

- A poll that is not a `304` when nothing changed.
- A surface that waits on the network before rendering anything.
- An id, node id, or raw URL visible anywhere.
- A rate-limit reply retried.
- A dark connection with no lamp.

## What done means

A normal GitHub day, reading side: every review request, mention,
assignment, and failing check arrives as a card without opening github.com;
opening the card shows the PR or issue with its description, comments,
reviews, and checks at once from the mirror; the diff is the local one with
the review comments in place; verdicts mark GitHub read; the screenshot per
item comes from the fake.
