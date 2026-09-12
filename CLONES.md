# The mirror store

Rho runs many agents against one repository. Each agent needs what a
human contributor has: a full clone — private refs, private config, the
freedom to fetch, push, gc without asking anyone. Naively that is a
`git clone` per agent, which is O(repo × clones) disk and a network
round trip per clone. The mirror store is the primitive that removes
both costs without changing what a clone *is*.

It is three small crates under `crates/rho-git/`:

- `rho-git-proto`: the one-line socket protocol, URL normalization and
  the store key.
- `rho-git-server`: the **keeper**, `MirrorStore`. The daemon runs it
  in-process; it is the only writer of the store root.
- `rho-git-client`: the client library (`Store`, `clone_from_mirror`)
  and the `rho-git` binary, which the agent's view installs as `git`.

## It is a cache, not a workflow

Nothing changes for the agent. `git` is `git`:

```sh
git clone https://github.com/org/repo      # instant after the first time
git fetch                                  # served from the local mirror
git push                                   # to the real remote, as always
```

The wrapper handles three commands when `RHO_GIT_STORE_SOCKET` names a
keeper, and `exec`s the real git (`RHO_GIT`, or the next `git` on PATH)
for everything else, arguments untouched:

- `git clone <url> [dir]` asks the keeper to *ensure* the URL's mirror
  and births the clone from it. Options that shape a clone (`--depth`,
  `--branch`, `--bare`, `--mirror`, `--reference`, `--filter`, ...) go
  to the real git unchanged: the store serves the common case, not
  every case.
- `git fetch ...` and `git pull ...` work out which URL git would read
  (a named remote's URL, a literal URL or path, else the current branch's
  upstream remote or `origin`), ask the keeper to *refresh* that mirror,
  add the mirror to the clone's alternates if it is new there, then run
  the real git with `url.<mirror>.insteadOf=<url>` so the fetch reads
  the mirror. Refspecs and options pass through. So a second remote
  (`git remote add upstream ...; git fetch upstream`) gets a mirror of
  its own on first fetch and the clone borrows from both. `--all` and
  `--multiple` refresh every remote named and pass one rewrite per
  remote: git fetches them in child processes of the real git, which
  read the rewrites from the environment. The rewrite is a per-process
  `-c` option; nothing is written to the clone's config or the store,
  only the alternates line.
- `git subtree add|pull --prefix=<dir> <repository> <ref>` is routed the
  same way, from outside: `git subtree` is a script, and git puts its
  own exec path first on the script's PATH, so the script's nested
  `git fetch` is the real git. The `insteadOf` the wrapper passes reaches
  it through the environment, and the mirror is in the alternates before
  the script starts.

If the keeper cannot be reached the wrapper says so on stderr and runs
the real git against the network. Without the socket variable it is
plain git.

The daemon's own clones (`Workset::clone_repo`, for a new agent's
starting repository) call the keeper directly and birth the clone the
same way, so an agent's `git clone` and the daemon's are the same thing.

## Constraints, then design

Two constraints drive everything:

1. **Storage is O(repo + clones)**, never O(repo × clones). The store
   may grow as history grows — that's O(repo) growth and fine.
2. **Clone creation stays well under a second**, with a small constant
   factor; O(repo-size) one-time costs are acceptable if amortized.

From these, share the minimum: only git's object bytes are shared, and
through the mechanism git has for borrowing them. Everything else —
refs, remotes, config, working tree — is per-clone private state,
exactly as in a clone on its own machine. Nothing in git is patched,
guarded, or banned.

### Shared artifact: git object bytes (alternates)

Every clone has its own private `.git` whose `objects/info/alternates`
points at the mirror's object database, by absolute path, one line per
mirror the clone has fetched from. This is git's first-class borrowing
mechanism:

- Fetch negotiation counts alternate objects as local — a fetch from the
  mirror transfers nothing that the mirror already has, which is
  everything.
- `git gc` in a clone repacks and prunes **only its own** odb; repack
  `-l` even dedups a clone's private objects against the store.
- Push, log, diff, everything else just works: objects are objects.

Because the path is absolute, the store root is mounted at the same
absolute path inside the agent's view as on the host (`WORKSET.md`).

## The store

```
<root>/<slug>-<hash>/     # one per remote URL (store_key)
  git/                    # bare mirror of the remote; the shared odb
  url                     # the remote URL; written last: presence = complete
```

`git/` is laid out like `git clone --mirror`: the remote's branches in
`refs/heads/*`, its tags in `refs/tags/*`, `HEAD` naming the remote's
default branch. So a fetch pointed at the mirror with git's default
refspecs behaves exactly like one from the remote, and a clone born
from it checks out the right branch offline. It holds only mirrored
state; nothing writes to it except the keeper's fetch.

**The one invariant: the store never prunes.** Clones borrow its
objects, so a pruned store object would corrupt clones. Auto-gc is
disabled at init (`gc.auto=0`, `gc.pruneExpire=never`,
`maintenance.auto=false`), and a mirror fetch never deletes objects
(ref pruning is fine; object bytes stay). The store is append-only;
growth is O(repo) by constraint 1.

Init happens in a staging directory renamed into place after the first
fetch succeeds, so a half-made mirror is never served. A refresh is
`git fetch --prune --no-tags origin`, `HEAD` re-pointed at whatever
`ls-remote --symref` says, then `git pack-refs --all`. The keeper runs
git with the user's environment, so credential helpers, ssh and
`git-remote-octo` work as they do for the user.

## The keeper

The keeper serializes work per mirror: one `tokio` mutex per URL, so
concurrent requests for one URL share a single fetch. A mirror fetched
within the *debounce* window (30 s by default) is served as is; a
background loop refetches each *interval* (60 s) every *active* mirror:
one requested at least *active_after* times (5) within *idle* (3 days;
the store's `used` file keeps the last few request times). A mirror
asked for once, or not lately, stays as it is, and every request fetches
what it needs itself. There is no file locking: the daemon is one
process, and git takes its own locks inside a mirror.

The protocol is one line each way on a unix socket, so a client needs
nothing but a socket:

```text
ensure <url>     init the mirror if missing, fetch it if stale
refresh <url>    fetch now (subject to the debounce)
ok <mirror>      the mirror's bare git directory
error <message>
```

## Clone birth

Steady state, creating a clone is a handful of local git commands and
the checkout — no network, no object transfer:

1. `git init` the destination; write `objects/info/alternates` pointing
   at `<mirror>/objects`.
2. `git remote add origin <real remote url>`: `origin` is the real
   remote, so `git remote -v`, `git push` and every tool see exactly
   what a plain clone would.
3. `git fetch <mirror> +refs/heads/*:refs/remotes/origin/* +refs/tags/*:refs/tags/*`:
   the remote-tracking refs a real clone leaves, borrowed rather than
   transferred.
4. `refs/remotes/origin/HEAD` set from the mirror's `HEAD`, and that
   branch checked out tracking `origin/<branch>`, exactly as `git clone`
   would after a network clone. An empty remote leaves HEAD unborn, as
   git does.

A clone is born at the mirror's last-fetched state — `origin/main`,
tags, the works — indistinguishable from a fresh clone on its own
machine, except that it owns no object bytes until it makes commits.

## Collaboration

Clones coordinate through the remote, like coworkers on different
machines: push from your clone, fetch in theirs. The store is not a
rendezvous point; it holds only mirrored remote state, and the keeper
keeps that state current for everyone at once. A clone's private
commits live in its own odb until pushed.

## What this design deliberately does not have

- No git patches, no banned commands. The wrapper intercepts three
  subcommands and passes everything else through, `exec`-transparent.
- No shared mutable state: a clone's remotes, config, refs are its own.
  `git gc`, `git worktree`, rebase — all per-clone, all safe.
- No clone registry in the store, no relative-pointer contract: a clone
  is wherever `git clone` put it and the store does not know it exists.
- Store gc policy is one line: the store never prunes.
- Blast radius of any clone-local disaster is that clone.

## Costs accepted, by constraint

| Cost | When | Why it's fine |
|---|---|---|
| Full fetch of the repo | mirror init, once | O(repo), one-time, off the clone path |
| Mirror refresh | every ensure past the debounce, and every interval while in use | delta-sized, off the clone path |
| Per-ref work at clone birth | every clone | local refs copy, no object transfer |
| Store growth | as history grows | O(repo), append-only |
