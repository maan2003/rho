# rho

rho is where one person works with many AI agents and the rest of their
inbound world, like Slack. The GUI is the product.

## Ideas

- **An editor, not an app.** rho is an editor the way Emacs is one, with
  Vim keys. Every screen is a buffer and keys do everything. There are
  no modals: the minibuffer asks, the echo line answers, and transients
  offer choices. New UI is built from editor primitives (`rho-window`).
- **Dealing.** Attention is the scarce resource. Rather than a feed, the
  dealer ranks everything that wants the user as cards and hands them
  the next one. The user answers each card with a verdict.
- **Notes are the user's memory.** The machine never writes note text.

## Shape

- `rho-gui` stands alone. It keeps its own database, runs Slack itself,
  and syncs the desk. It is usually remote, on a laptop or a phone.
- Agent hosts are machines the GUI attaches to. Everything only they
  build lives in `agent-host/`, which has its own `AGENTS.md`.
- `crates/` holds the GUI and what both sides share. The GUI reaches a
  host only through protocols: each client crate owns its protocol in a
  `protocol` module, and everything else in it sits behind the `client`
  feature, which only the GUI turns on.

## Security

- Everything the GUI shows is untrusted: Slack, transcripts, model
  output. Nothing displayed acts without the user.
- It holds the Slack token and the key it authenticates to hosts with.

## Working here

- Docs say why; the code says what. Rules that must hold are Linked
  Specs next to the code they govern (`linked-specs` skill).
- `ARCHITECTURE.md` and `SECURITY.md` are being broken down into specs
  and crate docs. Treat them as history, not as rules.
- Vendored subtrees (`vendor/*`, `crates/senax-encoder`) are first-class
  code: fix things where they belong (`git-subtree` skill). Plain `rg`
  skips `vendor/`; name the path to search it.
