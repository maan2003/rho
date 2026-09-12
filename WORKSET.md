# Worksets and filesystem views

A Workset is the unit Rho gives an agent: an ordered collection of named
Checkouts backed by daemon-managed clone stores. `rho-workset` owns the full
lifecycle—rho-db records, clone/fork orchestration, mount mapping, tmpfs layout,
and the live namespace. Shared stores live under `~/src/.rho/stores`; each
Workset's host-frame files live under `~/src/.rho/worksets/<id>/src`, while its
explicit primary Checkout and append-only Checkout order live in rho-db.

`Workset::enter(Mode)` turns that durable host-frame collection into one of the
runtime views below. The lower-level layout builders remain public for direct
inspection and development tooling (`rho-workset-dev`).

How a Rho agent's filesystem is laid out: a private mount namespace
whose root is built fresh for each agent. The clone store that
provides the repositories in it is described in `CLONES.md`.

**This is a layout, not a sandbox.** Everything runs as the invoking
user in an unprivileged user namespace; no security boundary is
claimed or implied. What the view buys is hygiene: agents get an identical, minimal,
disposable environment; they see only the Checkouts their Workset grants, not
the rest of the host or other Worksets; and everything outside those
host-backed Checkouts—the root, `$HOME`, and `/tmp`—is tmpfs that evaporates
with the namespace.

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
- `/src`: the working set, below. The command starts here.

The environment is an explicit allowlist (plus HOME/USER/LOGNAME);
inherited fds are closed on exec.

`Namespace::set_claude_home` mounts an agent's Claude Code home over its
`~/.claude` inside the live namespace: the per-account state directory,
a shared `projects/` directory, the prompt as `CLAUDE.md`, and an optional
`settings.json`, all bind mounts of host paths. In view mode a host-home
relative `config_home` lands under `/home/agent`. Setting a different home
detaches the previous stack first.

`Namespace::read_file_bounded` reads a file from a checkout by visible or
primary-relative path with `openat2(RESOLVE_BENEATH)`, so symlinks that
leave the checkout are refused, and returns at most 64 MiB.

## /src: the working set

Workspaces appear at `/src/<name>`, read-write. Clone stores appear at
`/src/.stores/<repo>`, read-only, with the agent's own clone
(`clones/<id>`) bind-mounted read-write over it — so the store's
never-prune invariant is at least mount-enforced against accidents,
while the agent's own refs, op log, and fetches work normally.

Because clone-store pointers are relative and never leave the tree
(`CLONES.md`), this layout is the entire filesystem contract: a
workspace plus its store, mounted in the same relative positions,
works identically from any mount root. The host locations of the
backing directories are bookkeeping the launcher owns.

A store is initialized on first use and refreshed with `jj store fetch`
before every later clone into it, so a new Checkout always starts from
the remote's current state; forks transfer by sha and need no refresh.
`Worksets::discard_workset` removes a Workset's Checkouts, its clone in
every store and its rho-db record, never the store's `git/` or
`template/`.

## Exposed mode

Some work genuinely needs the real system. Exposed mode is the full
host view as the user — environment, `$HOME`, every path unchanged —
plus the same working-set tree mounted at `/ws` over the host's existing
`/ws` stub. View mode presents checkouts at `/src`; exposed mode temporarily
keeps `/ws` until the deployed host stub migrates. The store plumbing remains
dot-hidden at `/src/.stores` or `/ws/.stores`, with identical relative pointer
depth. Exposed access is granted per agent by the user.

The stub is the one host prerequisite this implies: an unprivileged
mount namespace can only mount over a directory that already exists,
and `/` belongs to root. So the host keeps a permanently empty `/ws`
(`d /ws 0500 root root` via systemd-tmpfiles) purely as mountpoint
real estate — deliberately opaque, so nothing can use or pollute it
unmounted.

## Deliberately not here

- No uid separation, no role users, no setgroups/setuid machinery:
  same user inside and out. Rejected as complexity without an honest
  boundary — a mount namespace shared with the host uid cannot
  contain a determined escape anyway.
- No pid namespace: the host's /proc is the useful one, and nested
  tools (sandboxed browsers, containers, agents' own tools) keep
  working without pid-1 signal plumbing.
- No other host prerequisites: unprivileged user namespaces and the
  `/ws` stub are the whole list.
