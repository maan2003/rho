# Agent host

An agent host (`rho-agent-host`) hosts agents: one worker process per
workset. Everything else it does is agent-adjacent: desktops agents run
that the user watches, terminals and shells in an agent's workset, the
git an agent uses and the user approves, voice, and a copy of the desk
for devices to sync through.

The host is the `rho-agent-host` binary. The `rho` command (rho-cli) runs
beside it and is mostly the agents' own: tools they call from their shell,
plus the plumbing around the host.

## The boundary

- The GUI reaches the host only through protocols: `rho_rpc::protocol`
  names them, and each client crate in `crates/` owns its own in a
  `protocol` module. Crates here use those modules alone, never a GUI
  crate or a `client` feature. `just check-split` fails if they do.
- A change to any protocol's messages changes the wire; bump the epoch
  in `rho_rpc::protocol`. Packed structs carry a hash of their field
  names and types, so a rename changes the wire too.
- A table records its stored types by name, without module paths
  (`rho-db::Sen<CellMeta>`), so they can move between modules and
  crates freely. Renaming one needs a migration that retypes its tables.

## Security

- An authenticated client can do anything the user can: it starts
  agents, and agents run code. Client authentication is the boundary.
- Credentials stay on the host. They never reach agent context, logs or
  clients.
- Tool output and web content are untrusted input to the model.
- Agents are not sandboxed; the workset view is hygiene, not isolation.

Resource limits are robustness, not security. Add one only when
exhaustion is obvious and likely.

## Working here

- Linked Specs exist for the `rho-agent` runtime loop in
  `rho-agent/specs/`.
- The end-to-end check is the fake-model proof: build the binaries, then
  `unshare --user --map-root-user --net sh -c 'ip link set lo up &&
  target/debug/rho-qa fake-model-proof --seconds 15 --bin-dir
  target/debug'`.
