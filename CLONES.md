# The clone store

Rho runs many agents against one repository. Each agent needs what a
human contributor has: a full clone — private refs, private op log,
private config, the freedom to fetch, push, gc, and reindex without
asking anyone. Naively that is a `jj git clone` per agent, which is
O(repo × clones) disk and a network round trip per clone. The clone
store is the primitive that removes both costs without changing what a
clone *is*.

This infrastructure is not rho-specific, so it lives in the jj fork:
the library is `jj_lib::clone_store` (`vendor/jj/lib/src/clone_store.rs`,
tests in `vendor/jj/lib/tests/test_clone_store.rs`) and the CLI is the
`jj store` command family (`vendor/jj/cli/src/commands/store.rs`). Rho
uses it like any other jj feature.

## CLI

```sh
jj store init <root> <remote-url>        # mirror the remote into a new store
jj store fetch <root>                    # prefetch (clones fetch on their own regardless)
jj store clone <root> <id>               # instant clone, born at last-fetched state
jj store workspace <root> <id> <dir> \
    [--name NAME] [--at COMMIT_HEX]      # colocated git worktree + jj workspace
```

Each command prints a JSON record of the paths it created. Inside a
workspace, plain `jj` and `git` just work — there is nothing else to
learn.

Agent-facing workflow and machine conventions (store and workspace
locations, per-agent clone naming, handing a workspace to a sub-agent)
live in the `clone-store` skill (`.agents/skills/clone-store/SKILL.md`).
On devboxes, `/ws` is provisioned by systemd-tmpfiles as the eventual
workspace mount root — host-side read-only, so entries appear only via
rho's mount layer; until that lands, workspaces go in `~/src/ws/`.

## Constraints, then design

Two constraints drive everything:

1. **Storage is O(repo + clones)**, never O(repo × clones). The store
   may grow as clones push things — that's O(repo) growth and fine.
2. **Clone creation stays well under 100ms**, with a small constant
   factor; O(repo-size) one-time costs are acceptable if amortized.

From these, share the minimum: only the two O(repo)-sized artifacts are
shared, and each through a mechanism that natively understands
borrowing. Everything else — refs, remotes, config, op store, working
copies — is per-clone private state, exactly as in a clone on its own
machine. Nothing in jj or git is patched, guarded, or banned.

### Shared artifact 1: git object bytes (alternates)

Every clone has its own private bare git repo whose
`objects/info/alternates` points at the store's object database. This is
git's first-class borrowing mechanism:

- Fetch negotiation counts alternate objects as local — a clone's fetch
  transfers only what the store doesn't have.
- `git gc` in a clone repacks and prunes **only its own** odb; repack
  `-l` even dedups a clone's private objects against the store.
- Push, log, diff, everything else just works: objects are objects.

### Shared artifact 2: the jj commit index (hardlinked segments)

jj's default index is a set of immutable, content-addressed segment
files. Two facts make them shareable:

- An index may always be a **superset** of an operation's visible set,
  so a newborn clone can borrow the template's full index wholesale.
- Reindexing replaces a repo's *links* to segments, never the segment
  bytes, so one clone reindexing cannot disturb another.

Clone creation hardlinks the template's segment files and associates the
clone's initial operation with the template's index via one copied
op-link file.

## The store

```
<root>/
  clone-store         # completion marker: "jj clone store v2"
  git/                # bare mirror of the remote; the shared odb
  template/           # lazily built jj repo over git/, for seeding
    repo/             # jj repo (store/git_target -> ../../../git)
    ref-state         # fingerprint of git/'s refs at last refresh
  template.lock, fetch.lock
  clones/<id>/
    git/              # private bare repo; alternates -> store odb
    repo/             # stock jj repo (store/git_target -> ../../git)
```

The store's `git/` is a plain bare mirror: `remote.origin` fetches
`+refs/heads/*:refs/remotes/origin/*` and `+refs/tags/*:refs/tags/*`.
It holds only mirrored state — no `refs/heads` of its own, nothing
writes to it except `git fetch`.

**The one invariant: the store never prunes.** Clones borrow its
objects, so a pruned store object would corrupt clones. Auto-gc is
disabled at init (`gc.auto=0`, `gc.pruneExpire=never`,
`maintenance.auto=false`), and store fetch never deletes objects (ref
pruning is fine; object bytes stay). The store is append-only; growth is
O(repo) by constraint 1.

`fetch()` is a flock plus plain `git fetch --prune --no-tags`, followed
by `git pack-refs --all` — no jj involvement at all. Packing matters for
the clone path: packed refs carry precomputed peeled tag targets, so
listing the store's refs never has to read a tag object.

### The template: an amortized cache

Seeding clones needs a jj index built over the store's git. Building it
is the one O(repo) jj cost, so it is lazy and amortized:

- **Store init does not build it.** Init is a git fetch, nothing more.
- The **first clone creation** builds it (staged, renamed into place)
  and pays seconds on a big repo — once per store.
- Later clone creations refresh it with a **delta import**, and skip
  even that when the store's ref state (a fingerprint of the full ref
  listing) hasn't changed since the last refresh — the common case,
  since freshness is consumed exactly where it's produced.

Template staleness is never a correctness problem; a crashed refresh
just means more work for the next one.

## Clone birth (the fast path)

Steady state, creating a clone is O(refs) file writes and data copies —
no network, no object walks, no subprocess spawns, ~40ms of library work
on a repo with 1,700 tags:

1. The store's refs (with peeled tag targets) and remote URL are read
   in-process with gix — packed-ref peel hints mean zero object reads.
2. The clone's bare git dir is written directly: the same files `git
   init --bare` plus `remote add --no-tags origin <real remote url>`
   would produce, plus `/.jj/` in `info/exclude`, relative alternates to
   the store odb, and `packed-refs` verbatim from the store's ref
   listing — the same `refs/remotes/origin/*` and `refs/tags/*` a real
   `git clone` leaves.
3. Stock jj repo over that git repo; the template's index segments are
   hardlinked in and its op link copied.
4. The template's **view** — heads, remote bookmarks, remote tags, local
   tags, git_refs — is copied wholesale as plain data in one
   transaction. Working-copy and git-HEAD state is cleared: that is
   per-workspace, never shared. (Importing from git instead would cost a
   gix object read per ref — annotated tags get peeled — which blows the
   budget on tag-heavy repos.)

The copied `git_refs` match the written `packed-refs` exactly (both come
from the same ref listing), so the clone's own later fetches import
incrementally on top.

Creation is staged under a dot-prefixed sibling directory and renamed
into place: the final path only ever holds complete clones, an
interrupted creation leaves the id free, and the wreckage is inert.

A clone is born at the store's last-fetched state — `main@origin`, tags,
the works — indistinguishable from a fresh clone on its own machine,
except that it owns no object bytes.

## Workspaces

A workspace is a **real colocated git checkout**: `git worktree add`
against the clone's private git repo (created `--no-checkout`), plus a
jj workspace attached to the clone's repo. Its pointers (`.git`,
`.jj/repo`, git's worktree back-pointer) are **relative**: a store and
its workspaces form one relocatable tree, and no pointer ever leaves
it. A mount namespace can therefore expose the tree at any root —
workspaces at `/ws/<name>` beside stores at `/ws/.stores/<repo>` —
and everything resolves, as long as mounts preserve each workspace's
position relative to its store. Nothing else needs to be mounted for
git and jj to fully work; `SANDBOX.md` builds on exactly this. jj materializes the files and keeps HEAD and the git index
in sync, because that's what stock jj does in a colocated repo. Every
git tool works — `status`, `describe --tags`, `log`, editors' git
integrations — because this *is* a git repository checkout. Multiple
workspaces of one clone share the clone's view and op log (a family, in
jj's normal multi-workspace sense).

Workspace creation is staged like everything else: built in a
dot-prefixed sibling with every embedded path pre-written for the
final location, then renamed into place. The final path only ever
holds complete workspaces;
an interrupted creation leaves the path free and the same command
retries cleanly, including when the interrupted attempt had already
committed its add-workspace operation.

## Collaboration

Clones coordinate through the remote, like coworkers on different
machines: push from your clone, fetch in theirs. The store is not a
rendezvous point; it holds only mirrored remote state. A clone's private
commits live in its own odb until pushed.

## What this design deliberately does not have

Earlier iterations shared the git repo itself between clones, which
required vendored guard patches in jj (banned commands, marker files),
a shared "furniture" git directory, store-level gc that unioned every
clone's keep refs, and careful reasoning about which git config was
shared. Sharing only bytes — via alternates and hardlinks, both built
for exactly this — deletes all of it:

- No jj patches, no banned commands, no marker semantics. Stock jj.
- No shared mutable state: a clone's `git remote`, config, refs, op log
  are its own. `jj op undo`, reindex, gc — all per-clone, all safe.
- Store gc policy is one line: the store never prunes.
- Blast radius of any clone-local disaster is that clone.

## Costs accepted, by constraint

| Cost | When | Why it's fine |
|---|---|---|
| Full fetch of the repo | store init, once | O(repo), one-time, off the clone path |
| Template index build | first clone creation | amortized over all clones |
| Template delta refresh | clone creation after a store fetch | rare; delta-sized |
| Per-ref work at clone birth | every clone | O(refs) data copies, no object reads (~40ms at 1,700 tags) |
| Store growth | as history grows | O(repo), append-only |

Measured on rho's own history (932 commits, 1,684 annotated tags,
~470MB), through the `jj store` CLI in release mode: store init ~26s
(the fetch), first clone 2.2s (template build), steady-state clone
**~60ms end-to-end** (~19ms of which is jj process startup; ~40ms is
clone work), workspace materialization ~640ms (working-copy checkout; a
later phase's concern).
