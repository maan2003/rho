# The agent view: Rho's own distro

`WORKSET.md` describes what the agent's filesystem view is today and how
`rho-workset` builds it. This note records where the view is going and,
more importantly, why: the requirements behind it and the principles
that follow from them. When a change to the view is proposed, it should
be checked against this list.

## The one idea

View mode is a container whose entire userspace Rho generates. The host
contributes exactly four things: the kernel (with `/proc` and the
network), `/nix/store`, the nix daemon socket, and the workset
directory. Everything else an agent sees — `/etc`, `/usr`, `/bin`,
`$HOME`, the environment — is written by Rho at namespace build time
from Rho's own description of what an agent gets.

This is the same relationship a container image has to its host, and
the same one NixOS has to its `configuration.nix`: the system is a
function of a description, not an accumulation of edits. It is not a
security boundary (see `WORKSET.md`); it is a distribution.

## Requirements and why

1. **Identical everywhere, generated, disposable.** Two agents on two
   machines get the same view, and an agent's view does not depend on
   which user launched the daemon. Nothing is copied from the user's
   home: no dotfiles, no home-manager tree, no skeleton, no link
   scripts. All configuration is a generated file under `/etc` or a
   variable in the environment, and `$HOME` starts empty on tmpfs.
   *Why:* an agent's behaviour must be reproducible and debuggable by
   reading the generator, not by diffing two users' laptops. It also
   keeps the user's own config free to be as personal as they like.

2. **The userland is a declared program list.** Rho names the tools an
   agent gets (coreutils, bash, Rho's patched git, direnv, nix, ripgrep,
   fd, just, python3, uv, node, …) and presents them as one directory
   mounted at `/usr`, with `/bin` pointing at it and `PATH=/usr/bin`. A
   nix build of Rho produces that directory as a `buildEnv`; a
   cargo-built daemon assembles the same set at start by resolving each
   name on the host and symlinking the store paths into a tmpfs.
   *Why:* today PATH is the daemon's login shell PATH filtered to store
   entries, which is a heuristic over the user's profile. A list is
   explicit, the same for everyone, and gives `/bin/sh` and
   `/usr/bin/env` — which build scripts, `just` and every shebang
   need — for free.

3. **The project environment comes from the repository, evaluated by
   direnv under Rho's configuration.** `.envrc` is the contract; direnv
   plus nix-direnv evaluate it, and the terminal, the shell sidecar and
   tool execution all run through it. Rho points `DIRENV_CONFIG` at a
   generated directory with its own `direnv.toml` and `direnvrc`:
   - The `/src` prefix is whitelisted; there is no `direnv allow` step.
     *Why:* an agent already runs the repository's build scripts, so a
     trust gate on `.envrc` protects nothing and only adds a failure
     mode.
   - The layout directory (what direnv calls `.direnv`) lives outside
     the checkout, in the workset's state directory, which is bound
     into the view at the same absolute path it has on the host.
     *Why:* nix-direnv registers garbage-collection roots by absolute
     path. Inside the checkout that path would be `/src/…`, which does
     not exist on the host, so the roots would dangle and every GC would
     delete the dev shell. At a host-valid path the roots resolve, and
     they die with the workset when it is discarded — a better lifetime
     than a `.direnv` that outlives its checkout. It also means two
     worktrees of one repository do not share one cache directory.
   - The `use_flake` wrapper that adds the shared cargo cache and
     `RHO_DIRENV_PATH_BEFORE` is part of that `direnvrc`.

4. **Nix works, through the daemon.** The daemon socket is bound,
   `NIX_REMOTE=daemon` is set, and `/etc/nix/nix.conf` is generated with
   flakes enabled (and whatever registry Rho wants pinned). The host's
   nix.conf is not copied.
   *Why:* flakes are how projects here declare toolchains, so `use
   flake` must work. With a read-only store the daemon is the only
   writer; without `NIX_REMOTE` nix sees a writable `/nix/var` on the
   tmpfs, picks a local store and fails. On NixOS `/etc/nix/nix.conf`
   is a symlink into `/etc/static`, so binding it gives a dangling link;
   generating it is both simpler and ours to control.

5. **Expensive state persists at fixed paths; everything else is
   disposable.** A shared cache directory under the Rho state root is
   mounted as `$XDG_CACHE_HOME` (nix evaluation and fetcher caches,
   cargo registry and shared target directory, uv, npm, pip). The
   per-workset state directory holds the direnv layout. The home itself
   is never persisted.
   *Why:* a cold flake evaluation with an empty nix cache takes minutes;
   a cold cargo build takes longer. Those caches are designed for
   concurrent use and are safe to share across agents. Keeping the
   cargo target directory out of the tree also keeps checkouts small,
   which matters to nix's `path:` fetcher (see 8).

6. **Identity is environment; behaviour is `/etc`.** The daemon owns
   the user's name and email as a setting and exports `GIT_AUTHOR_*`
   and `GIT_COMMITTER_*`. Git's behavioural settings (no pager, no
   signing, `init.defaultBranch`) are a generated `/etc/gitconfig`
   named by `GIT_CONFIG_SYSTEM`.
   *Why:* every commit an agent makes must carry authorship, and no
   agent should need a config file in its home to get it. The user's
   own git config is not in the view, so nothing personal leaks in.

7. **Locale and terminal are fixed.** `LANG=C.UTF-8` (built into glibc,
   so no locale archive), `TERM` and `TZ` passed through, `COLORTERM`
   set. Shell initialisation is `/etc/bashrc` and `/etc/profile`, which
   the nixpkgs bash reads; the direnv hook and prompt live there. Fish,
   tmux and zoxide are not part of the distro.

8. **Every checkout is a plain git repository, and `git` is git.** The
   `git` on the agent's PATH is Rho's git: stock git with one patch
   (`CLONES.md`) under which `clone` and `fetch` of a remote URL read
   the daemon's mirror store through git alternates and leave an
   ordinary repository with `origin` at the real remote. Everything
   that fetches, from `pull` to `subtree` to submodules, goes through
   that one path; every other command is untouched. Further checkouts
   are `git worktree add`, which agents run for themselves when they
   want a child in its own checkout. There is no second VCS in the view
   and no daemon-side notion of a change: the model works with git
   alone.
   *Why:* one tool the model knows well beats two it confuses. Nix treats a directory without `.git`
   as a `path:` flake and copies the whole tree, ignored files included,
   into the store on every evaluation; with `.git` it fetches only
   tracked files. `git status`, `gh`, cargo's vergen-style build
   scripts and countless project scripts assume the same.

9. **Anything the host must resolve has the same path in both frames.**
   The clone-store root and socket already appear at their host paths
   inside the view; the per-workset state directory (direnv layout, GC
   roots) joins them. Everything else lives at the view's own paths and
   never at a host path.
   *Why:* git alternates and nix GC roots record absolute paths and are
   read by processes on the host side (the nix daemon, the store server).

10. **The view never depends on host configuration, and exposed mode is
    untouched.** Exposed mode is the host's userspace with the workset
    at `/src`; it is where a user's personal environment applies. Nothing
    in view mode reads the host's `/etc`, the user's home or the user's
    profile, so a machine's setup cannot leak into or break the view.

## Deliberately not in the view

- Dotfiles and home-manager output of any kind; a home skeleton.
- The `direnv allow` database; the login-shell PATH filter; the
  locale archive; a copy of the host's `/etc/nix`.
- ssh keys, tailscale, gh, amp, codex and other credentials. Claude Code
  state is the one exception and is mounted explicitly by
  `Namespace::set_claude_home`; a skills directory can join that stack.
- Interactive shells other than bash.

## Layout

| Path | Contents |
| --- | --- |
| `/usr` (`/bin` → `/usr/bin`) | the declared userland, read-only |
| `/nix/store` | host store, read-only |
| `/nix/var/nix/daemon-socket` | host nix daemon socket |
| `/etc` | generated: passwd, group, hosts, resolv.conf, nsswitch, ssl, localtime, `nix/nix.conf`, `gitconfig`, `rho/direnv/`, `bashrc`, `profile` |
| `/home/agent` | empty tmpfs; `~/.cache` is the shared persistent cache; `~/.claude` is the Claude home stack |
| `/src` | the workset, read-write |
| `<state>/stores`, `<state>/bin`, `<state>/store.sock` | at host paths: mirrors and Rho's git read-only, the keeper's socket |
| `<state>/worksets/<id>/state` | at its host path, read-write: direnv layout and GC roots |
| `/proc`, `/dev`, `/tmp` | as today |

## Environment

`PATH=/usr/bin`; `HOME`, `USER`, `LOGNAME`; `TERM`, `TZ`, `COLORTERM`;
`LANG=C.UTF-8`; `XDG_CACHE_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`;
`NIX_REMOTE=daemon`; `DIRENV_CONFIG=/etc/rho/direnv`;
`GIT_CONFIG_SYSTEM=/etc/gitconfig`; `GIT_AUTHOR_*`, `GIT_COMMITTER_*`;
`RHO_GIT_STORE_SOCKET`, `RHO_GIT`; `INSIDE_AGENT=1`;
`CARGO_HOME` and `CARGO_BUILD_TARGET_DIR` under the shared cache.
Variables the caller sets on a command survive, as today.

## Where it lives

The distro is its own crate, `rho-agent-distro`, and the boundary is
image versus runtime:

- `rho-agent-distro` builds an **image** in a caller-provided directory on
  tmpfs: a directory tree plus an environment manifest. It resolves the program list into `usr/`,
  writes every generated `/etc` file (nix.conf, gitconfig, the direnv
  configuration and `direnvrc`, bashrc and profile) and lists the
  variables. It knows nothing about namespaces or mounts, so it is
  tested with plain file assertions, and a nix build of Rho can run it
  as a derivation step to produce the static part of the image.
- `rho-workset` is the **runtime**: it takes an image and mounts it,
  then adds what only the running daemon knows (passwd with the real
  uid, resolv.conf, the user's identity, state-root paths) and what is
  per agent (the workset at `/src`, the store, the Claude home, the
  working directory). The `/etc` generation and PATH filtering in its
  `layout.rs` today move to the distro crate.

The image builder is implemented in `crates/rho-agent-distro`. A future Nix
derivation can put the declared programs on `PATH` and invoke its binary in
the build step with a staging directory plus the nix-direnv and CA-certificate
package paths. At runtime the daemon calls the same builder with a freshly
mounted tmpfs directory.

Three things change at three different times — the image at build
time, the daemon-level pieces at daemon start, the agent-level pieces
per agent — and keeping them in separate layers is what keeps the image
reproducible.

## Later, enabled by this layout

- Evaluate `.envrc` in the background right after a clone, so the first
  agent command does not pay for the evaluation.
- Pin the flake registry Rho was built with, so `nix run nixpkgs#…`
  is offline-friendly and deterministic.
