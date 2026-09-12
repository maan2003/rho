# The clone store

Rho runs many agents against one repository. Each agent needs what a
human contributor has: a full clone — private refs, private op log,
private config, the freedom to fetch, push, gc, and reindex without
asking anyone. Naively that is a `jj git clone` per agent, which is
O(repo × clones) disk and a network round trip per clone. The clone
store is the primitive that removes both costs without changing what a
clone *is*.

This infrastructure is not rho-specific, so it lives in the jj fork:
`jj_lib::clone_store` (`vendor/jj/lib/src/clone_store.rs`, tests in
`vendor/jj/lib/tests/test_clone_store.rs`), the store server
`jj_lib::clone_store_server`, and the `jj store` maintenance commands
(`vendor/jj/cli/src/commands/store.rs`). Rho uses it like any other jj
feature.

## It is a cache, not a workflow

Nothing changes for the user of jj. With one setting, the ordinary
commands get fast:

```sh
export JJ_STORE=~/.local/state/rho/stores     # or git.clone-store in config
jj git clone https://github.com/org/repo      # instant after the first time
jj git fetch                                  # served from the local mirror
jj git push                                   # to the real remote, as always
```

`jj git clone` looks up the remote URL's store under the root,
initializes it on first use, refreshes it otherwise, and materializes
the clone from it. `jj git fetch` in a clone whose remote has a store
refreshes the store and fetches from its mirror; the clone never talks
to the network for reads. Push goes to `origin`, which is the real
remote. Narrower clones (`--branch`, `--tag`, `--depth`, a remote not
named `origin`) take the normal network path.

With a second setting the stores are owned by a server:

```sh
jj store serve --socket /run/user/1000/jj-store.sock    # keeps every store fetched
export JJ_STORE_SOCKET=/run/user/1000/jj-store.sock     # clients ask it instead
```

Clients then never write a store: they ask the server to ensure or
refresh the URL's store and read from the path it returns. The server
holds one lock per store, so concurrent clones of one URL share a single
fetch, and it refetches every store on an interval so new clones are
born on the latest remote state without waiting. Rho's daemon embeds
this server; the store root is mounted read-only inside agent namespaces.

`jj store fetch [URL...]` and `jj store list` maintain a root by hand.

## Constraints, then design

Two constraints drive everything:

1. **Storage is O(repo + clones)**, never O(repo × clones). The store
   may grow as history grows — that's O(repo) growth and fine.
2. **Clone creation stays well under a second**, with a small constant
   factor; O(repo-size) one-time costs are acceptable if amortized.

From these, share the minimum: only the two O(repo)-sized artifacts are
shared, and each through a mechanism that natively understands
borrowing. Everything else — refs, remotes, config, op store, working
copy — is per-clone private state, exactly as in a clone on its own
machine. Nothing in jj or git is patched, guarded, or banned.

### Shared artifact 1: git object bytes (alternates)

Every clone has its own private git repo whose `objects/info/alternates`
points at the store's object database, by absolute path. This is git's
first-class borrowing mechanism:

- Fetch negotiation counts alternate objects as local — a fetch from the
  mirror transfers nothing that the mirror already has, which is
  everything.
- `git gc` in a clone repacks and prunes **only its own** odb; repack
  `-l` even dedups a clone's private objects against the store.
- Push, log, diff, everything else just works: objects are objects.

### Shared artifact 2: the jj commit index (reflinked segments)

jj's default index is a set of immutable, content-addressed segment
files. Two facts make them shareable:

- An index may always be a **superset** of an operation's visible set,
  so a newborn clone can borrow the template's full index wholesale.
- Reindexing replaces a repo's *links* to segments, never the segment
  bytes, so one clone reindexing cannot disturb another.

Clone creation reflinks the template's segment files (hardlink fallback
on filesystems without reflink) and associates the clone's initial
operation with the template's index via one copied op-link file.

## The store

```
<root>/<slug>-<hash>/     # one per remote URL (store_key)
  clone-store             # completion marker
  git/                    # bare mirror of the remote; the shared odb
  template/               # lazily built jj repo over git/, for seeding
    repo/                 # jj repo (store/git_target -> ../../../git)
    ref-state             # fingerprint of git/'s refs at last refresh
  template.lock, fetch.lock
```

The store's `git/` is a plain bare mirror: `remote.origin` fetches
`+refs/heads/*:refs/remotes/origin/*` and `+refs/tags/*:refs/tags/*`,
and `origin/HEAD` records the remote's default branch so clones check
out the right branch offline. It holds only mirrored state — no
`refs/heads` of its own; nothing writes to it except fetch.

**The one invariant: the store never prunes.** Clones borrow its
objects, so a pruned store object would corrupt clones. Auto-gc is
disabled at init (`gc.auto=0`, `gc.pruneExpire=never`,
`maintenance.auto=false`), and store fetch never deletes objects (ref
pruning is fine; object bytes stay). The store is append-only; growth is
O(repo) by constraint 1.

Fetch is a flock plus plain `git fetch --prune --no-tags`, `remote
set-head --auto`, and `git pack-refs --all`, then a template refresh.
Packing matters for the clone path: packed refs carry precomputed
peeled tag targets, so listing the store's refs never reads a tag
object.

### The template: an amortized cache

Seeding clones needs a jj index built over the store's git. Building it
is the one O(repo) jj cost, so it is lazy and amortized:

- **Store init does not build it.** Init is a git fetch, nothing more.
  (The server builds it right after init, so its clients never do.)
- The **first clone** builds it (staged, renamed into place) and pays
  seconds on a big repo — once per store.
- Every store fetch refreshes it with a **delta import**, where
  freshness is produced. A clone only reads it; when a client cannot
  refresh a stale template (read-only store), the stale one is still a
  valid seed and the clone imports the difference itself.

Template staleness is never a correctness problem; a crashed refresh
just means more work for the next one.

## Clone birth (the fast path)

Steady state, creating a clone is O(refs) file writes and data copies —
no network, no object walks, no subprocess spawns — plus the checkout:

1. The store's refs (with peeled tag targets) and remote URL are read
   in-process with gix — packed-ref peel hints mean zero object reads.
2. The clone's git dir is written directly (`.git` when colocated,
   `.jj/repo/store/git` otherwise): the files `git init` plus `remote
   add --no-tags origin <real remote url>` would produce, `/.jj/` in
   `info/exclude`, alternates to the store odb, and `packed-refs`
   verbatim from the store's ref listing — the same
   `refs/remotes/origin/*` and `refs/tags/*` a real `git clone` leaves.
3. Stock jj repo over that git repo; the template's index segments are
   reflinked in and its op link copied.
4. The template's **view** — heads, remote bookmarks, remote tags, local
   tags, git_refs — is copied wholesale as plain data in one
   transaction. Working-copy and git-HEAD state is cleared: that is
   per-workspace, never shared.
5. A stock workspace is attached and `jj git clone` checks out the
   remote's default branch, exactly as it would after a network clone.

The copied `git_refs` match the written `packed-refs` exactly (both come
from the same ref listing), so the clone's own later fetches import
incrementally on top.

A clone is born at the store's last-fetched state — `main@origin`, tags,
the works — indistinguishable from a fresh clone on its own machine,
except that it owns no object bytes beyond its own working-copy commit.

## Collaboration

Clones coordinate through the remote, like coworkers on different
machines: push from your clone, fetch in theirs. The store is not a
rendezvous point; it holds only mirrored remote state, and the server
keeps that state current for everyone at once. A clone's private
commits live in its own odb until pushed.

## What this design deliberately does not have

- No jj patches, no banned commands, no marker semantics. Stock jj.
- No shared mutable state: a clone's `git remote`, config, refs, op log
  are its own. `jj op undo`, reindex, gc — all per-clone, all safe.
- No clone registry in the store, no relative-pointer contract, no
  `jj store clone`: a clone is wherever `jj git clone` put it and the
  store does not know it exists.
- Store gc policy is one line: the store never prunes.
- Blast radius of any clone-local disaster is that clone.

## Costs accepted, by constraint

| Cost | When | Why it's fine |
|---|---|---|
| Full fetch of the repo | store init, once | O(repo), one-time, off the clone path |
| Template index build | first clone (or server init) | amortized over all clones |
| Template delta refresh | every store fetch | delta-sized, off the clone path |
| Per-ref work at clone birth | every clone | O(refs) data copies, no object reads |
| Store growth | as history grows | O(repo), append-only |
