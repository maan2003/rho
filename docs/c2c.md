# C2C: the clanker to clanker protocol

Status: draft.

## Purpose

A clanker is a program that works for an owner and decides for itself what
to do next: it reads what reaches it, acts, and writes back, with no person
steering each step. In practice it is a language model in a loop. A harness
is the program that hosts clankers, on a machine that is often behind NAT
and often offline.

C2C (clanker to clanker) lets a clanker on one harness send a message to a
clanker on another, whoever built either harness. It is mail for clankers:
addresses, messages, and a bounce. It defines nothing more about what a
harness is, how it runs clankers, or what a clanker does with a message.

## Addresses

Each harness holds one [iroh](https://iroh.computer) endpoint. An address is

    <endpoint id>/<local id>

The endpoint id is the harness's public key, so the transport proves which
harness a message came from. In an address it is written as 64 lowercase
hexadecimal digits. The local id names a recipient inside that harness, and
only that harness gives it meaning: a clanker, usually. It is 1 to 64 bytes
of `a-z`, `0-9`, `.`, `_` and `-`.

An address is public. Knowing it grants nothing, so it can be pasted into a
chat or a log. There is no directory and no discovery: an address reaches a
sender the way an email address does, through a person or through an earlier
message.

## Permission

A clanker accepts an address, and gives it a name:

    clanker A: I am <endpoint 1>/<local 1>
    clanker B: I am <endpoint 2>/<local 2>
    clanker A: accept(<endpoint 2>/<local 2>, "B's owner's clanker")
    clanker B: accept(<endpoint 1>/<local 1>, "A's owner's clanker")

Accepting is the whole permission model.

- A harness delivers a message only when its recipient has accepted the
  sender's address. Everything else is refused.
- Accepting covers both directions for that one clanker: it sends only to
  addresses it has accepted, so a reply always has somewhere to go.
- A refusal says nothing more. An unknown local id and a recipient that has
  not accepted the sender look the same from outside.
- A refused message leaves no trace the recipient can see. A stranger cannot
  wake a clanker, or put text in front of it, by sending.
- The name stays in the accepting harness and never crosses the wire. A
  clanker can trust it because its own side wrote it.

Who may call `accept` for a clanker, and how a person reviews or removes what
was accepted, is the harness's business.

## Messages

A message carries the recipient's local id, the sender's local id, an id the
sender chose, and a body of text. The receiving harness answers `delivered`
or `refused`.

- `delivered` means the receiving harness has stored the message, not that
  the clanker has read it.
- The sending harness keeps a message until it is delivered, and tries
  again while the other harness is unreachable.
- `refused` is not final: the recipient may accept later. A sender may keep
  trying for a while before it reports a bounce to its clanker.
- Messages from one address to another arrive in the order they were sent:
  a harness sends the next one only when the one before is delivered or
  given up on.
- A message whose answer was lost is sent again under the same id, and it
  is then the newest from its address. A receiver that stored it answers
  `delivered` again and does not store it twice.

### On the wire

The ALPN is `c2c/1`; a change old peers cannot read takes a new number.

The sending harness opens a bidirectional stream for each message, writes
one CBOR map, and finishes its side of the stream:

| Key    | Value                                                           |
| ------ | --------------------------------------------------------------- |
| `to`   | Text: the recipient's local id.                                 |
| `from` | Text: the sender's local id.                                    |
| `id`   | Unsigned integer below 2^64, never used again for this address. |
| `body` | Text, not blank, of at most 65536 bytes of UTF-8.               |

The receiving harness answers on the same stream with one CBOR map, and
finishes its side:

| Key      | Value                           |
| -------- | ------------------------------- |
| `answer` | Text: `delivered` or `refused`. |

A receiver ignores keys it does not know. It answers `refused` to anything
else it cannot read as a message. A sender treats a missing answer, or one it
does not know, as a harness it could not reach.

## Trust

- The endpoint is the unit of trust. The handshake proves the sending
  harness; the sender's local id is that harness's claim. Accepting an
  address therefore trusts its whole harness not to lie about which of its
  clankers is speaking.
- A message carries no authority. It is input from another party, like a
  web page: the clanker that reads it still answers only to its own owner.
  Nothing in the protocol lets a sender ask for more than to be read.
- The transport encrypts between harnesses. The two harnesses read
  everything.
- The protocol does not defend against a flood from a harness a clanker has
  accepted.

## Not part of this

Groups, discovery, attachments, and byte streams between clankers (a tunnel to
a service a clanker runs) are left out. A stream protocol would be a second
ALPN beside this one and would reuse the same addresses and the same accept.
