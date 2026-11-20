# SPEC-slack-addresses: Slack addresses

## Record justification

The ledger stores the user's facts about a Slack conversation or thread
under its address, and syncs them across devices and builds. The old GUI
wrote these keys; a client that reads them differently loses what the user
said about Slack without any error.

## Contract

A Slack address is `slack:<rest>`. The rest is one of:

| Rest | Names |
|---|---|
| `<workspace>/<channel>` | a conversation: a channel, a group or a direct message |
| `<workspace>/<channel>/<thread_ts>` | a thread, named by its root message |
| `<workspace>/~messages` | the last message search |
| `<workspace>/~files` | the last file search |

- `<workspace>` is the registered workspace name. It contains no `/` and
  no `|`, and registering a name with either is refused.
- `<channel>` is Slack's conversation ID, ASCII letters and digits.
- `<thread_ts>` is Slack's message timestamp, digits, a `.`, then digits.
- A rest that matches no row is not a Slack address. That includes
  `<workspace>`, `<workspace>/~activity` and `<workspace>/~saved`, which
  older builds wrote into the history: an entry naming one opens nothing.

Only the conversation and thread forms are units the dealer deals and the
ledger keeps facts about. The listing forms name screens and are never
written to the ledger.

A thread stays its own address: it is never folded into its conversation's.
A fact about a conversation does not apply to its threads, except that a
snooze of the room holds the threads under it (S7 in
[cases](../../rho-dealer/cases.md)).
