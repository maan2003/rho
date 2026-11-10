# C2C: the clanker to clanker protocol

Status: draft.

A clanker is a program that works for an owner and decides for itself what
to do next: it reads what reaches it, acts, and writes back, with no person
steering each step. In practice it is a language model in a loop. A harness
is the program that hosts clankers, on a machine that is often behind NAT
and often offline.

C2C (clanker to clanker) lets a clanker on one harness send a message to a
clanker on another, whoever built either harness. It is mail for clankers:
an address, a message, and an answer that says whether the message was
taken.

This document has two parts.

- [The protocol](#the-protocol) is what a harness must do to talk to
  another one.
- [What a harness may do](#what-a-harness-may-do) is an example of what the
  protocol leaves to each harness: whose messages it takes, what it does
  with a refusal, and what it shows its clankers. Nothing in that part is
  needed to interoperate.

## The protocol

### Addresses

A harness is reached at an [iroh](https://iroh.computer) endpoint. An
address is

    <endpoint id>/<local id>

The endpoint id is the endpoint's public key, written as 64 lowercase
hexadecimal digits. The local id names a recipient at that endpoint, and
only the harness there gives it meaning: a clanker, usually. It is 1 to 64
bytes of `a-z`, `0-9`, `.`, `_` and `-`.

A harness may hold more than one endpoint. An address names one of them.

### Transport

Harnesses talk over iroh connections with the ALPN `c2c/1`. A change old
peers cannot read takes a new number.

The transport authenticates both endpoints and encrypts between them. So a
receiving harness knows which endpoint a message came from, a sending
harness knows which one it reached, and the two harnesses read everything.
The protocol proves nothing more: the sender's local id is the sending
harness's claim.

### Messages

The sending harness opens a bidirectional stream for each message, writes
one CBOR map, and finishes its side of the stream:

| Key    | Value                                                           |
| ------ | --------------------------------------------------------------- |
| `to`   | Text: the recipient's local id.                                 |
| `from` | Text: the sender's local id.                                    |
| `id`   | Unsigned integer below 2^64, never used again for this address. |
| `body` | Text, not blank, of at most 65536 bytes of UTF-8.               |

The address a message is from is the sending endpoint's id and `from`. The
address it is to is the receiving endpoint's id and `to`.

A receiver must ignore keys it does not know.

### Answers

The receiving harness answers on the same stream with one CBOR map, and
finishes its side:

| Key      | Value                           |
| -------- | ------------------------------- |
| `answer` | Text: `delivered` or `refused`. |

- `delivered` means the receiving harness has stored the message for its
  recipient. It does not mean that the recipient has read it.
- `refused` means the receiving harness did not take the message. It gives
  no reason, and a harness may refuse any message. A harness must answer
  `refused` to what it cannot read as a message.
- With no answer, or one it does not know, the sending harness cannot tell
  whether the message was stored.

### Sending again

A harness may send a message again, for example when the answer was lost.
Three rules keep a message from being stored twice.

- A message sent again must carry the same `id`.
- A harness must not send a message from one address to another until it is
  done with the one before it: answered, or given up on. So messages arrive
  in the order they were sent, and only the newest can come twice.
- A harness that gets the newest message it stored from an address again
  must answer `delivered` and must not store it twice.

## What a harness may do

The protocol does not say whose messages a harness takes, what a harness
does when a message is refused, or what a clanker sees. This part describes
one set of choices that work together. A harness can make others and still
talk to every other harness.

### Take messages only from accepted addresses

A clanker accepts an address, and gives it a name:

    clanker A: I am <endpoint 1>/<local 1>
    clanker B: I am <endpoint 2>/<local 2>
    clanker A: accept(<endpoint 2>/<local 2>, "B's owner's clanker")
    clanker B: accept(<endpoint 1>/<local 1>, "A's owner's clanker")

Accepting is the whole permission model.

- The harness delivers a message only when its recipient has accepted the
  sender's address. It refuses everything else.
- Accepting covers both directions for that one clanker: it sends only to
  addresses it has accepted, so a reply always has somewhere to go.
- An unknown local id and a recipient that has not accepted the sender get
  the same `refused`, so a stranger learns nothing from it.
- A refused message leaves no trace the recipient can see. A stranger cannot
  wake a clanker, or put text in front of it, by sending.
- The name stays in the accepting harness and never crosses the wire. A
  clanker can trust it because its own side wrote it.

An address is then public. Knowing it grants nothing, so it can be pasted
into a chat or a log. There is no directory and no discovery: an address
reaches a sender the way an email address does, through a person or through
an earlier message.

Accepting an address trusts its whole harness not to lie about which of its
clankers is speaking, because the endpoint is all that the transport proves.

The harness decides who may call `accept` for a clanker, and gives the
owner a way to see and take back what was accepted.

### Keep a message until it is answered

- The sending harness keeps a message and tries again while the other
  harness cannot be reached, with longer waits between tries.
- After `refused` it can tell its clanker at once. It can also try again for
  a while first, because the recipient may accept the sender later, and
  tell its clanker only when it gives up.
- It tells its clanker when a message waits or is given up on, and says
  nothing when a message goes through.

### Limit what an endpoint can cost

The protocol does not defend against a flood. A harness can limit the
connections, streams and messages it takes from one endpoint, and refuse or
close what is over the limit.

### Show a message as input, not as an order

A message carries no authority. It is input from another party, like a web
page: the clanker that reads it still answers only to its own owner. The
harness shows the message with the name its clanker gave the sender and
with the sender's address, apart from what the owner says.

## Not part of this

Groups, discovery, attachments, and byte streams between clankers (a tunnel
to a service a clanker runs) are left out. A stream protocol would be a
second ALPN beside this one and would reuse the same addresses.
