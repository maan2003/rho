# SPEC-agent-address: An agent's address

## Record justification

The plain form is written by `rho_chat::AgentRef` for a thread's
participants and an agent's posts, the parts under it by
`rho_agents_view::system::AgentAddress`, and every form is kept in the
user's ledger and history and read back by every GUI device and build that
shares them, so no one artifact owns it.

## Address

An agent is `agent:<id>`, where `<id>` is the agent's encoded id
(`eng-ht08`). The id is the host's and unique across hosts, so the address
carries no host; the agents system finds the host from the agent.

What hangs under the agent is a part after the id:

- `agent:<id>/term/<n>` is terminal `<n>` of the agent's workset, a
  number.
- `agent:<id>/shell` is the agent's one host-owned shell. Opening it
  starts the shell when none runs and attaches to it; closing the surface
  only detaches, and the shell keeps running until it is closed on the
  host.
- `agent:<id>/desktop/<name>` is the desktop called `<name>` in the
  agent's workset, as the host's desktops stream lists it. A name never
  holds `/`; a listed desktop whose name would has no address and is not
  offered.

An id never holds `/`, so the id ends at the first `/`. Any other text
after the id is not an address.
