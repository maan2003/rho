# The agent filesystem view

How a Rho agent's filesystem is laid out: a private mount namespace
whose root is built fresh for each agent. The clone store that
provides the repositories in it is described in `CLONES.md`.

**This is a layout, not a sandbox.** Everything runs as the invoking
user in an unprivileged user namespace; no security boundary is
claimed or implied. What the view buys is hygiene: agents get an
identical, minimal, disposable environment, they don't see each
other's working trees, and nothing they do can drift or clutter the
host filesystem — the whole root evaporates with the namespace.

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

## Exposed mode

Some work genuinely needs the real system. Exposed mode is the full
host view as the user — environment, `$HOME`, every path unchanged —
plus the same `/src` working-set tree, mounted in a namespace of its
own over the host's `/src` stub. Both modes therefore present
identical `/src` paths, so nothing about an agent's repositories or
instructions differs between them. Granted per agent by the user.

The stub is the one host prerequisite this implies: an unprivileged
mount namespace can only mount over a directory that already exists,
and `/` belongs to root. So the host keeps a permanently empty `/src`
(`d /src 0500 root root` via systemd-tmpfiles) purely as mountpoint
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
  `/src` stub are the whole list.
