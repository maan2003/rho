# The client has one database

The client kept its state in five redb files: `agent-mirror.redb`,
`desk-mirror.redb`, `action-journal.redb`, `inbox.redb` and `slack.redb`.
That is five file locks, five page caches and five allocator rebuilds
after an unclean stop, and five chances for a session to be half open. It
is one file now, `rho-client.redb`, the way the daemon's store is one
file.

Every crate keeps its own tables and its own types; only the file is
shared. The names were already owner-prefixed — `gui_mirror_*`,
`gui_agent_*`, `gui_desk_*`, `gui_action_journal_*`, `rho_slack_*`,
`rho_gui_inbox_*` — so nothing collides and nothing had to be renamed.

`main` names the state directory and nothing else. The model thread opens
the database, because after an unclean stop redb rebuilds its allocator
from every page and no frame may wait on that, and hands it to the agent
mirror and the desk replica directly. Everything whose own open would
have happened on the main thread — the action journal — registers with
`rho_db::client::on_open` instead and is handed the file when it opens.
The Slack mirror and the inbox ask for it when they need it, and a
process that has none simply has no cache, which is what a test is and
what each of these has always promised to be.

Two things fall out of it. `capture_carryover` resolved the state
directory itself, through `dirs::state_dir()` in a library, against the
rule that only a binary's `main` does that; it asks for the database now
and the call is gone. And the journal's own lock file is gone with it:
the lock belongs to the file, so `rho-client.lock` covers all of it, and
it is held by the database handle rather than dropped by the call that
took it.

## No migration, and the files that are now dead

Nothing is copied over. Every one of these is a replica or a cache: the
agent mirror and the desk replica rebuild from the daemon, the Slack
mirror refills from Slack, and rho's Slack cursors re-seed from the
store's cells exactly as they did the first time. The journal is the one
thing that is neither, and it is a record of what the user did rather
than something they read; a new one starts at the next launch.

So these files are simply no longer opened. They can be deleted:

    ~/.local/state/rho/agent-mirror.redb
    ~/.local/state/rho/desk-mirror.redb
    ~/.local/state/rho/action-journal.redb
    ~/.local/state/rho/action-journal.lock
    ~/.local/state/rho/inbox.redb
    ~/.local/state/rho/slack.redb

`desk-device` stays where it is: it names this device, it is not a cache,
and the desk replica's resume depends on it being the same after a
restart.

A QA snapshot taken before this and used after it starts with empty
mirrors rather than the user's, since the names in the allow lists moved
with the file. Both lists carry `rho-client.redb` now and nothing else of
the client's.
