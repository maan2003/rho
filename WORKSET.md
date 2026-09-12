# Worksets and the agent filesystem view

A workset is the unit Rho gives an agent: one plain directory, presented
at `/src` inside the agent's private mount namespace. The daemon does not
interpret what is in it. The agent clones repositories into it with
ordinary `jj git clone`, adds jj workspaces with `jj workspace add`, and
keeps whatever else it wants there; the directory is the truth and there
is no separate record of its contents.

`rho-workset` owns the state root, `~/.local/state/rho`:

```
~/.local/state/rho/
  stores/            # clone-store root (CLONES.md), URL-keyed
  store.sock         # the store server's socket
  worksets/<id>/src  # one directory per workset
```

`Worksets::open` creates the root and starts `jj store serve` on the
socket. That server is the only writer of `stores/`: it initializes a
store on first request, refetches every store in the background so new
clones are born on the remote's current state, and serves the same
store to concurrent requests under one lock. Everything else — the
daemon's own `Workset::clone_repo`, an agent's `jj git clone` and
`jj git fetch` — is a client that reads a store and never touches the
network. `Worksets::discard_workset` deletes the workset directory;
stores are shared and never removed.

Several agents can work in one workset: a child agent joins its parent's
workset and gets its own jj workspace inside it, made by the parent.
Every agent carries its own working directory below `/src`; loading
`AGENTS.md`-style context is a function of that directory, not of a
"primary" repository.

`Workset::enter(Mode)` turns the directory into one of the runtime views
below. The lower-level layout builders remain public for direct
inspection and development tooling (`rho-workset-dev`).

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
  optional skeleton. The host home is not mounted at all.
- `/dev`: the standard character devices bound in, plus a private
  devpts.
- `/src`: the workset directory, read-write. The command starts here.
- The clone-store root, read-only, and the store socket, at the same
  absolute paths they have on the host. Clones record the store by
  absolute path (git alternates), so the path must not change between
  the daemon's frame and the agent's.

The environment is an explicit allowlist (PATH, TERM, plus
HOME/USER/LOGNAME) and `JJ_STORE` / `JJ_STORE_SOCKET` pointing jj at the
store server; inherited fds are closed on exec.

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
path and refuses anything outside `/src`.

## Exposed mode

Some work genuinely needs the real system. Exposed mode is the full
host view as the user — environment, `$HOME`, every path unchanged —
plus the workset directory mounted at `/ws` over the host's existing
`/ws` stub and the store root made read-only. View mode presents the
workset at `/src`; exposed mode temporarily keeps `/ws` until the
deployed host stub migrates. Exposed access is granted per agent by the
user.

The stub is the one host prerequisite this implies: an unprivileged
mount namespace can only mount over a directory that already exists,
and `/` belongs to root. So the host keeps a permanently empty `/ws`
(`d /ws 0500 root root` via systemd-tmpfiles) purely as mountpoint
real estate — deliberately opaque, so nothing can use or pollute it
unmounted.

## Deliberately not here

- No workset table: the directory is the record. Opening a workset is
  checking that its directory exists.
- No forking of checkouts between agents: children join the parent's
  workset; separate work happens in jj workspaces the parent creates.
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
  `/ws` stub are the whole list.
