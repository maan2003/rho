# Worksets and the agent filesystem view

A workset is the unit Rho gives an agent: one plain directory, presented
at `/src` inside the agent's private mount namespace. The daemon does not
interpret what is in it. The agent clones repositories into it with
ordinary `git clone`, adds checkouts with `git worktree add`, and keeps
whatever else it wants there; the directory is the truth and there is
no separate record of its contents.

`rho-fs-view` owns the state root, `~/.local/state/rho`:

```
~/.local/state/rho/
  stores/              # mirror store root (CLONES.md), URL-keyed
  store.sock           # the mirror keeper's socket
  cache/               # every agent's ~/.cache (VIEW.md)
  worksets/<id>/src    # one directory per workset
  worksets/<id>/state  # its direnv layout and nix GC roots
```

`Worksets::open` creates the root and starts the mirror keeper
(`rho-git-server`) in-process on the socket. The keeper is the only
writer of `stores/`: it initializes a mirror on first request, refetches
it on later requests (debounced, never in the background), and serves
the same mirror to concurrent requests under one lock. Everything else —
the daemon's own `Workset::clone_repo`, an agent's `git clone` and `git
fetch` through Rho's patched git — is a client that reads a mirror.
That git is part of the agent base (`VIEW.md`), the `buildEnv` whose
path the daemon bakes in at build time (`RHO_AGENT_BASE`, set by the
flake for nix and dev-shell builds). `Worksets::discard_workset`
deletes the workset directory; mirrors are shared and never removed.

Several agents can work in one workset: a child agent joins its parent's
workset in the parent's directory. A parent that wants a child in a
checkout of its own makes one itself first — a git worktree, another
clone, whatever it likes — and tells the child where to work; the
daemon only ever does the initial clone. Every agent's record is a workset id, a working directory as the
agent sees it, and a mode; loading
`AGENTS.md`-style context is a function of that directory (the git
checkout containing it), not of a "primary" repository. The daemon runs
one `Worksets` for its state root and hands the pool a `Workset` per
agent; a directory outside the root can be adopted for one process
(`Worksets::adopt`), which is how tests and `rho eval` work in place.

Agents recorded before worksets (`WorkspaceInfo::Workspace`, a jj
managed workspace) still load: their transcripts read, but they cannot
run. `rho debug migrate-agent <agent>` asks the running daemon to move
one into a workset: a clone of the repository's origin through the mirror
store (the daemon's, since `octo://` remotes need its transport), checked
out (detached) at the old workspace's parent commit with the working
copy's changes staged, recorded as a `WorkdirMigrated` event at the tail
of the agent's log, after which the loaded agent is dropped so its next
load reads the new place. The agent is exposed unless `--mode view` says
otherwise, since a jj workspace on the host was. The old workspace is
left as it is.

`Workset::enter(mode, cwd)` is one agent's `Namespace` over the
directory: its mount namespace is built on the first command (so loading
an agent never fails on a namespace it does not use) and then kept for
the life of the value. `prepare_command` enters it for a child process;
`enter_interpreter_thread` moves a dedicated thread into it for the
in-process Python notebook. There are two modes, view and exposed, and
in both the workset is at `/src`; `rho-fs-view-dev` enters one from the
command line the way the daemon does.
`VIEW.md` records the requirements and principles the view is being
built towards, and why.

**This is a layout, not a sandbox.** Everything runs as the invoking
user in an unprivileged user namespace; no security boundary is
claimed or implied. What the view buys is hygiene: agents get an
identical, minimal, disposable environment; they see only their workset,
not the rest of the host or other worksets; and everything outside it —
the root, `$HOME`, and `/tmp` — is tmpfs that evaporates with the
namespace.

## What's in the view

The root is one fresh tmpfs, generated at launch. Everything on it is
a plain directory or file except a handful of real mounts:

- `/nix/store`, read-only: all software.
- `/proc`: the host's, bound (same pid namespace).
- A generated `/etc`: passwd, DNS, TLS certs — written, not bound.
- `/home/agent`: `$HOME`, empty tmpfs directory seeded from an
  optional skeleton. The host home is not mounted at all; `~/.cache` is
  the shared persistent cache.
- The workset's state directory, read-write at its host path, so the
  nix GC roots direnv registers there resolve on the host.
- `/dev`: the standard character devices bound in, plus a private
  devpts.
- `/src`: the workset directory, read-write. The command starts here.
- The mirror store root, read-only, and the keeper's socket, at the
  same absolute paths they have on the host.
  Clones record the store by absolute path (git alternates), so the
  path must not change between the daemon's frame and the agent's.
- The directory holding the daemon's own executable, read-only at its
  host path, when that is outside `/nix/store`: a cargo-built daemon can
  then launch its sibling sidecars (`rho-shell`, `rho-pager`). A nix
  build adds nothing.

The environment is an explicit allowlist, listed in `VIEW.md`: PATH is
the agent's nix profile then the base, and the rest names the home, the
caches, git's identity and configuration, direnv's configuration,
`NIX_REMOTE=daemon` when the host has a nix daemon and
`RHO_GIT_STORE_SOCKET` pointing git at the keeper. Exposed mode
passes the user's environment through with Rho's git first on PATH.
Variables the caller sets on the command survive in both modes, and
inherited fds are closed on exec.

`Namespace::set_claude_home` mounts an agent's Claude Code home over its
`~/.claude` inside the live namespace: the per-account state directory,
a shared `projects/` directory, the prompt as `CLAUDE.md`, and an optional
`settings.json`, all bind mounts of host paths. In view mode a host-home
relative `config_home` lands under `/home/agent`. Setting a different home
detaches the previous stack first.

`Namespace::read_file_bounded` reads a file below `/src` by visible or
relative path with `openat2(RESOLVE_BENEATH)`, so symlinks that leave the
workset are refused, and returns at most 64 MiB.
`Namespace::prepare_command` takes the working directory as a visible
path, or one relative to the agent's own, and refuses anything outside
`/src`.

## Exposed mode

Some work genuinely needs the real system. Exposed mode is the full
host view as the user — environment, `$HOME`, every path unchanged —
plus the workset directory mounted at `/src` over the host's existing
`/src` stub and the store root made read-only. Paths are the same in
both modes, so an agent's record and prompt do not depend on the mode.
Exposed access is granted per agent by the user; the daemon's
`--workset-mode` flag (`RHO_WORKSET_MODE`) picks the mode new agents
get, `view` by default.

The stub is the one host prerequisite this implies: an unprivileged
mount namespace can only mount over a directory that already exists,
and `/` belongs to root. So the host keeps a permanently empty `/src`
(`d /src 0500 root root` via systemd-tmpfiles) purely as mountpoint
real estate — deliberately opaque, so nothing can use or pollute it
unmounted. Entering exposed mode on a host without it fails with that
message.

There is no mode without a namespace. `rho eval`, `rho-daemon debug
render-prompt` and the tests adopt a host directory as a workset
(`Worksets::adopt`) and enter it in view mode; whatever only reads files
or renders prompts never builds the namespace, and whatever runs
commands does so in a real one (their processes set up the identity user
namespace first, before any thread).

## Deliberately not here

- No workset table: the directory is the record. Opening a workset is
  checking that its directory exists.
- No forking of checkouts between agents: children join the parent's
  workset; separate work happens in worktrees the parent creates.
- No namespace refresh: `/src` is one bind mount, so anything cloned
  into the workset is visible immediately.
- No uid separation, no role users, no setgroups/setuid machinery:
  same user inside and out. Rejected as complexity without an honest
  boundary — a mount namespace shared with the host uid cannot
  contain a determined escape anyway.
- No pid namespace: the host's /proc is the useful one, and nested
  tools (sandboxed browsers, containers, agents' own tools) keep
  working without pid-1 signal plumbing.
- No other host prerequisites: unprivileged user namespaces and the
  `/src` stub (exposed mode only) are the whole list.
