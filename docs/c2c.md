# C2C: the clanker to clanker protocol

Status: draft. Nothing implements it yet.

## Purpose

Agents run inside harnesses: programs that host them, on machines that are
often behind NAT and often offline. C2C (clanker to clanker) lets an agent
on one harness send a message to an agent on another, whoever built either
harness. It is mail for agents: addresses, messages, and a bounce. It defines nothing
about what a harness is, how it runs agents, or what an agent does with a
message.

## Addresses

Each harness holds one [iroh](https://iroh.computer) endpoint. An address is

    <endpoint id>/<local id>

The endpoint id is the harness's public key, so the transport proves which
harness a message came from. The local id names a recipient inside that
harness, and only that harness gives it meaning: an agent, usually. It is at
most 64 bytes of `a-z`, `0-9`, `.`, `_` and `-`.

An address is public. Knowing it grants nothing, so it can be pasted into a
chat or a log. There is no directory and no discovery: an address reaches a
sender the way an email address does, through a person or through an earlier
message.

## Permission

An agent accepts an address, and gives it a name:

    agent A: I am <endpoint 1>/<local 1>
    agent B: I am <endpoint 2>/<local 2>
    agent A: accept(<endpoint 2>/<local 2>, "B's owner's agent")
    agent B: accept(<endpoint 1>/<local 1>, "A's owner's agent")

Accepting is the whole permission model.

- A harness delivers a message only when its recipient has accepted the
  sender's address. Everything else is refused.
- Accepting covers both directions for that one agent: it sends only to
  addresses it has accepted, so a reply always has somewhere to go.
- A refusal says nothing more. An unknown local id and a recipient that has
  not accepted the sender look the same from outside.
- A refused message leaves no trace the recipient can see. A stranger cannot
  wake an agent, or put text in front of it, by sending.
- The name stays in the accepting harness and never crosses the wire. An
  agent can trust it because its own side wrote it.

Who may call `accept` for an agent, and how a person reviews or removes what
was accepted, is the harness's business.

## Messages

A message carries the recipient's local id, the sender's local id, an id the
sender chose, and a body of text. The receiving harness answers `delivered`
or `refused`.

- `delivered` means the receiving harness has stored the message, not that
  the agent has read it.
- The sending harness keeps a message until it is delivered, and tries
  again while the other harness is unreachable. A receiver drops a message
  whose sender and id it has already stored.
- `refused` is not final: the recipient may accept later. A sender may keep
  trying for a while before it reports a bounce to its agent.
- Messages from one address to another arrive in the order they were sent.

On the wire a message is one CBOR map on a stream of its own, and the answer
comes back on that stream. A receiver ignores fields it does not know. The
ALPN is `c2c/1`; a change old peers cannot read takes a new number.

## Trust

- The endpoint is the unit of trust. The handshake proves the sending
  harness; the sender's local id is that harness's claim. Accepting an
  address therefore trusts its whole harness not to lie about which of its
  agents is speaking.
- A message carries no authority. It is input from another party, like a
  web page: the agent that reads it still answers only to its own user.
  Nothing in the protocol lets a sender ask for more than to be read.
- The transport encrypts between harnesses. The two harnesses read
  everything.
- The protocol does not defend against a flood from a harness an agent has
  accepted.

## Not part of this

Groups, discovery, attachments, and byte streams between agents (a tunnel to
a service an agent runs) are left out. A stream protocol would be a second
ALPN beside this one and would reuse the same addresses and the same accept.

The names of the map's fields and size limits are not chosen yet.
