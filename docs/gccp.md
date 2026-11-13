# GCCP: the global clanker communication protocol

Status: draft.

The file-transfer extension below is a proposal. Harnesses do not necessarily
implement it.

A clanker is a program that works for an owner and decides for itself what
to do next: it reads what reaches it, acts, and writes back, with no person
steering each step. In practice it is a language model in a loop. A harness
is the program that hosts clankers, on a machine that is often behind NAT
and often offline.

GCCP (global clanker communication protocol) lets a clanker on one harness
send a message to a clanker on another, whoever built either harness. It is
mail for clankers: an address, a message, and a result that says whether
the message was taken.

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

The endpoint id is the endpoint's public key, written in z-base-32 as
pkarr writes it: 52 symbols of `ybndrfg8ejkmcpqxot1uwisza345h769`, most
significant bit first, with the last 4 bits zero. iroh's `to_z32` writes
it so. A harness writes and reads an endpoint id in this form wherever it
is text, so that an address is short to paste. On the wire an endpoint is
never text: the transport names it.

The local id names a recipient at that endpoint, and only the harness there
gives it meaning: a clanker, usually. It is 1 to 64 bytes of `a-z`, `0-9`,
`.`, `_` and `-`.

A harness may hold more than one endpoint. An address names one of them.

The protocol has no directory and no discovery. A clanker is expected to
get an address through a channel that exists already: its owner gives it,
or it is in a chat, a document or an earlier message.

### Transport

Harnesses talk over iroh connections with the ALPN `gccp/1`. A later
version may use a higher number.

The transport authenticates both endpoints and encrypts between them. So a
receiving harness knows which endpoint a message came from, a sending
harness knows which one it reached, and the two harnesses read everything.
The protocol proves nothing more: the sender's local id is the sending
harness's claim.

### Streams

Everything is carried on bidirectional streams. The harness that opens a
stream first writes two bytes: the kind of the stream, an unsigned 16-bit
integer, most significant byte first.

| Kind | Stream                    |
| ---- | ------------------------- |
| 1    | A message and its result. |
| 2    | A file offer, its decision, bytes, and a result. |

The kind says what the rest of the stream carries in both directions, and
how it ends. So the header holds no length: that belongs to each kind.

The most significant bit is reserved and must be 0. A later version may use
it to say that more header follows. The other kinds are for later versions
of this document, which can add them without a new ALPN.

A harness that gets a kind it does not know must reset that stream, with 1
as iroh's stream error code, and must keep the connection: other streams on
it carry kinds that it does know. The harness that opened the stream then
knows that the other one does not speak that kind.

### Messages

A message stream has kind 1. After the kind, the sending harness writes one
CBOR map and finishes its side of the stream:

| Key    | Value                                                           |
| ------ | --------------------------------------------------------------- |
| `to`   | Text: the recipient's local id.                                 |
| `from` | Text: the sender's local id.                                    |
| `id`   | Unsigned integer below 2^64, new for each message.              |
| `body` | Text, not blank, of at most 65536 bytes of UTF-8.               |

The address a message is from is the sending endpoint's id and `from`. The
address it is to is the receiving endpoint's id and `to`.

An `id` has to differ only from the others that the same address sent
lately, so a random number will do.

A clanker reads every message it takes, and every message costs it
attention and context. So each harness chooses the largest body it takes,
up to 65536 bytes. It should take bodies of at least 1024 bytes. A body of
at most 1024 bytes then fits the size limit of each harness, though a
harness can still refuse it for other reasons. A clanker with more to say
splits it, or sends where to find it.

A receiver must ignore keys it does not know.

### Results

Every message has a result, which says what the receiving harness did with
it. It is not a reply from the recipient: a clanker that wants to reply
sends a message of its own.

The receiving harness writes the result on the same stream as one CBOR map,
and finishes its side:

| Key       | Value                                                 |
| --------- | ----------------------------------------------------- |
| `success` | Boolean: `true` for stored, `false` for refused.      |
| `reason`  | Optional text, only with `false`: why it was refused. |

- `true` means the receiving harness has stored the message for its
  recipient. It does not mean that the recipient has read it.
- `false` means the receiving harness refused the message: it did not take
  it. A harness may refuse any message. A harness must return `false` for
  what it cannot read as a message.
- With no result, or one it cannot read, the sending harness cannot tell
  whether the message was stored.
- A sending harness must ignore keys it does not know in a result, as a
  receiver does in a message.

A `reason` tells the sending clanker what to do differently, so it is
given only where something can be done:

| Reason          | Meaning                                                         |
| --------------- | --------------------------------------------------------------- |
| `msg-too-large` | The body is larger than this harness takes. It never fits here. |
| `queue-full`    | The recipient has too much waiting. A later try may fit.        |

- A reason is 1 to 64 bytes of `a-z`, `0-9` and `-`, so a result stays
  short. Old harnesses read at most 256 bytes of a result.
- A later version may add reasons. A sending harness must not act on a
  reason it does not know, or on a missing one, other than as a refusal.
  It may still show an unknown reason to its clanker as text.
- A harness may leave the reason out of any refusal. It must leave it out
  where the reason tells the sender more than it may learn: for example,
  that a local id exists.

### Sending again

A harness may send a message again, for example when the result was lost.
The `id` keeps the message from being stored twice.

- A message sent again must carry the same `id`.
- A harness that remembers the `id` from that address must return `true`
  and must not store the message again.
- A harness chooses how many ids it remembers, so a late copy can be stored
  twice.

### Files

A file transfer uses kind 2 on a new stream. It is independent of message
streams: sending a chat message about a file is optional. Kind 1 and the
ALPN do not change. A harness that does not implement files rejects kind 2
as an unknown kind, not by closing the connection.

The sender offers a file and waits for the receiver's decision. If the
receiver accepts, the sender uploads the bytes. The receiver stores its own
copy in its artifact store and makes a notice available to the recipient. Importing
that artifact later does not contact the sender. The copy can survive a
sender restart, but remains subject to the receiver's retention policy.

#### Offer

After the kind, the sender writes one CBOR map, without finishing its side:

| Key      | Value                                                       |
| -------- | ----------------------------------------------------------- |
| `to`     | Text: the recipient's local id.                              |
| `from`   | Text: the sender's local id.                                 |
| `id`     | Unsigned integer below 2^64, new for each file transfer.      |
| `size`   | Unsigned integer below 2^64: the number of file bytes.        |
| `hash`   | Byte string of exactly 32 bytes: SHA-256 of the file bytes.   |
| `name`   | Optional nonempty text, at most 255 bytes of UTF-8.           |
| `mime`   | Optional nonempty text, at most 255 bytes of UTF-8.           |

Addresses have the same meaning as in a message. File ids are separate
from message ids. An empty file is allowed. The name and MIME type are
untrusted hints, not a path to write to or proof of a file's contents.

The encoded offer must fit in 4096 bytes, including unknown fields. The
receiver must ignore unknown keys, reject missing required fields, invalid
known fields and duplicate keys, and enforce the bound while decoding. It must
not wait for the sender to finish its side to recognize the end of the
CBOR map: the sender is waiting for a decision.

#### Decision

The receiver applies its policy before accepting file bytes. For files,
the sender and receiver must each authorize the other address for the
local clanker; how they establish those allowlists is local policy. The
receiver must refuse an unknown recipient or an unauthorized sender
without a reason, and without showing the offer to a clanker.

For an authorized sender, policy may consider size, available storage,
rate limits, or whether the recipient wants the file. The receiver may ask
the recipient or owner for approval, but approval is not a wire operation and
is not required. Accepting an address does not require accepting its files.

The receiver writes one CBOR result map with `success` and optional
`reason`, using the types and reason rules from [Results](#results).
Each file decision and final result must fit in 256 encoded bytes; unknown
keys are ignored and duplicate keys are invalid.

- `false` refuses the offer. The receiver finishes its side; the sender
  finishes without sending file bytes. There is no final result.
- `true` permits this stream's upload, but does not acknowledge storage.
  The receiver leaves its side open for the final result.

An invalid or oversized offer gets `false` when the stream still permits
a response. A sender must not send file bytes before a valid `true`
decision. A receiver must not store or deliver prematurely sent bytes and
may reset the stream. Transport buffering alone is not acceptance.

#### Bytes and final result

After a `true` decision, the sender writes exactly `size` raw bytes, not
another CBOR value, then finishes its side. The receiver streams the bytes
into temporary storage and verifies both the size and SHA-256 hash. It
must observe the end of the sender's side to rule out trailing bytes.
Zero bytes still require that end of stream.

The receiver then writes a final result in the same format and finishes
its side:

- `true` means the complete, verified file is committed to the receiver's
  artifact store and a notice is stored for its recipient. It does not
  mean the recipient has read the notice or imported the artifact.
- `false` means this attempt did not commit a file and notice. Partial
  or invalid data must not become a recipient-visible artifact.

The notice must identify the sender address and transfer id, the SHA-256
hash and size, and any offered name and MIME type. It must let the
recipient import the local artifact; a store that uses different native
identifiers supplies its local reference as well. The notice format and
import interface are local to the harness. Neither a chat message nor a
new network request from the recipient is needed.

The receiver must not return final `true` before both the artifact and
notice are stored. If it cannot complete storage, it returns `false`
when possible. A stream reset, missing result, malformed result, or lost
connection leaves the sender uncertain whether the file was stored.

File refusals can use `queue-full` or these additional reasons:

| Reason             | Meaning                                           |
| ------------------ | ------------------------------------------------- |
| `file-too-large`   | The file exceeds this receiver's size limit.       |
| `storage-full`     | The receiver cannot store the file now.            |
| `file-invalid`     | The bytes do not match the offered size or hash.   |
| `id-conflict`      | A remembered transfer id has a different offer.    |

As with messages, reasons may be omitted, and an unknown reason is only a
refusal. A failed retry does not revoke an earlier successful transfer.

#### Retry, limits, and retention

A retry opens a new kind-2 stream with the same offer, including `id`,
hash and metadata. The receiver makes a new decision and, if it accepts,
the sender repeats the entire upload; there is no resume, separate status
query, or fetching from the sender. A different file or changed metadata needs a new id.

The receiver keys completed transfers by sender address, recipient address
and file id. While it remembers a completed transfer, it must refuse a
changed offer with `id-conflict`, or, for the same offer, verify the
repeated upload and return final `true` without creating another notice.
Unknown offer keys do not participate in this comparison. Concurrent
attempts for the same key must not create duplicate notices while the
completion is remembered; a receiver may refuse an overlapping attempt
with `queue-full`. A receiver can still refuse any retry under its policy.

As with messages, remembering ids is bounded local policy, not an
exactly-once guarantee. A late retry or one after a receiver restart can
produce another notice. A receiver must not acknowledge an expired copy
as stored unless it has stored the uploaded bytes again.

The protocol promises no minimum file size limit or retention time. Receivers
choose limits on file size, total storage, offer rate, concurrent uploads,
and time waiting for approval or bytes. Either side may cancel or time out
a stream; acceptance reserves no indefinite right to upload. The receiver
must clean up abandoned temporary data and must enforce its limits even
after acceptance. To refuse mid-upload, the receiver may send final `false`
and stop reading; the sender must watch for that result while writing and stop
its upload on refusal or reset.

A final `true` acknowledges storage, not permanent availability. Retention
and auto-deletion are receiver policy; later import can report expiry.
Deleting a file need not delete the remembered transfer id or its notice.

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

Accepting is the permission model.

- The harness delivers a message only when its recipient has accepted the
  sender's address. It refuses everything else.
- Accepting covers both directions for that one clanker: it sends only to
  addresses it has accepted, so a reply always has somewhere to go.
- An unknown local id and a recipient that has not accepted the sender get
  the same refusal, with no reason, so a stranger learns nothing from it.
  Only a sender that the recipient accepted gets a reason.
- A refused message leaves no trace the recipient can see. A stranger cannot
  wake a clanker, or put text in front of it, by sending.
- The name stays in the accepting harness and never crosses the wire. A
  clanker can trust it because its own side wrote it.

An address is then public. Knowing it grants nothing, so the channel that
carries it need not be secret: it can be pasted into a chat or a log. The
channel must still carry the right address, because a clanker that accepts
a wrong one talks to someone else.

Accepting an address trusts its whole harness not to lie about which of its
clankers is speaking, because the endpoint is all that the transport proves.

The harness decides who may call `accept` for a clanker, and gives the
owner a way to see and take back what was accepted. It can ask for more: for
example, that the owner approves another harness before a clanker may accept
an address on it.

### Try a few times, then tell the clanker

- After a refusal the sending harness does not try again.
- When the other harness cannot be reached, or gives no result, the sending
  harness tries again a few times over a few minutes: for example after one
  minute, and then after two more. Then it gives up.
- The harness tells its clanker that the message did not go through, and
  whether the other harness refused it or was not reached. It passes on the
  reason of a refusal, when there is one. After no result, the message may
  still have arrived.
- The clanker decides if and when to send again. The harness keeps no queue
  for hours or days: the clanker knows whether the message still matters.
- The receiving harness remembers the ids of the last few hundred messages
  it stored. It keeps them in memory only: every sender gives up within
  minutes, so an older id never comes again. After a restart it remembers
  none, and a message sent again across the restart is stored twice.

There is no call and no session. A connection is only how bytes travel:
either harness may close one at any time, and a clanker is never told that
a connection opened or closed.

### Limit what an endpoint can cost

The protocol does not defend against a flood. A harness can limit the
connections, streams and messages it takes from one endpoint, and refuse or
close what is over the limit.

### Pace what reaches a clanker

Each time a clanker is woken it changes context, and messages that come one
by one wake it again and again. So the receiving harness hands a clanker
its messages in batches, and a batch counts as one input:

- It hands over the first batch at once, and each later one at least 15
  seconds after the last. The owner can change the 15 seconds.
- It takes bodies of at most 1024 bytes, and refuses larger ones with
  `msg-too-large`.
- Between batches it holds at most 5 messages, and at most 1024 bytes of
  bodies, for one clanker. A message that would go over either is refused
  with `queue-full`.
- A held message is stored, so its result is `true`. Each message in a
  batch keeps its sender and its `id`.

### Show a message as input, not as an order

A message carries no authority. It is input from another party, like a web
page: the clanker that reads it still answers only to its own owner. The
harness shows the message with the name its clanker gave the sender and
with the sender's address, apart from what the owner says.

## Not part of this

The protocol leaves out groups, discovery, attachments embedded in messages,
and byte streams between clankers (a tunnel to a service a clanker runs). A
tunnel would be another kind of stream beside messages and file transfers,
and would reuse the same addresses.
