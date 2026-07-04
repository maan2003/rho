---
name: temp-migration
description: Use when changing a rho local database format and deciding whether to add, keep, or remove a short-lived migration.
---

# Temporary database migrations

Rho local databases intentionally avoid long-term backwards compatibility. When a local DB format changes, use a short-lived migration that exists only long enough for active developer databases to move forward, then remove it in a follow-up commit after migration has run.

This skill applies to repo-local persisted formats such as `rho-agent`'s redb/senax records.

## Principles

- Prefer schema changes that are naturally compatible and need no migration.
- If data must be rewritten, add a migration for exactly one format hop.
- Bump the format id whenever a migration must run.
- Use random-looking 8-character hex strings for format ids, not monotonic numbers. This makes branch conflicts obvious.
- Keep migration code contained near the table/record definitions it migrates.
- Keep temporary migration code short-lived: usually 1-2 commits total. Ask the user/developer to run the migrated build once, confirm the DB opens, then remove the migration.
- Do not keep long-term compatibility code for old private/dev DB formats.

## Current rho-agent pattern

`crates/rho-agent/src/db.rs` stores one format string in the `FORMAT` table:

```rust
const FORMAT: TableDefinition<(), String> = TableDefinition::new("format");
const CURRENT_AGENT_DB_FORMAT: &str = "c7b31a9e";

struct AgentDbMigration {
    from: &'static str,
    to: &'static str,
    migrate: fn(&mut WriteTxn),
}

const AGENT_DB_MIGRATIONS: &[AgentDbMigration] = &[];
```

`init_agent_tables()` opens tables and calls `migrate_agent_db_format(self)`. The migration loop follows `from -> to` links until it reaches `CURRENT_AGENT_DB_FORMAT`. If there is no supported path, it panics with user-facing guidance to update one version at a time or remove the local DB if saved agents are not needed.

## When no migration is needed

No migration is needed when both old and new rows decode safely.

Examples:

- Removing an optional field where old rows with the extra field still decode.
- Adding an optional field where old rows decode as `None`.
- Changing runtime-only behavior without changing persisted bytes.

Still verify against a real or copied DB when possible:

```sh
cargo run -q -p rho-cli -- debug migrate
cargo run -q -p rho-cli -- debug agents
cargo check -p rho-agent -p rho-daemon -p rho-cli
```

If old rows fail with `MissingRequiredField`, you need either a migration or a temporary compatible decode shape.

## No-op format bump

Use a no-op migration when you need to force all DBs through a new format id, but no data rewrite is required.

```rust
const CURRENT_AGENT_DB_FORMAT: &str = "9a4c1e20";

const AGENT_DB_MIGRATIONS: &[AgentDbMigration] = &[
    AgentDbMigration {
        from: "c7b31a9e",
        to: "9a4c1e20",
        migrate: |_| {},
    },
];
```

Ask the user/developer to run the daemon/CLI once with this commit. After it successfully opens the DB and writes the new format id, remove the migration in the next commit:

```rust
const CURRENT_AGENT_DB_FORMAT: &str = "9a4c1e20";
const AGENT_DB_MIGRATIONS: &[AgentDbMigration] = &[];
```

## Real data rewrite example

Use typed tables and typed values. Avoid raw redb table hacks unless there is no typed path.

Example shape for adding a required field that old records cannot decode into directly:

```rust
const CURRENT_AGENT_DB_FORMAT: &str = "6f34d2ab";

const AGENT_DB_MIGRATIONS: &[AgentDbMigration] = &[
    AgentDbMigration {
        from: "9a4c1e20",
        to: "6f34d2ab",
        migrate: migrate_agent_mode,
    },
];

fn migrate_agent_mode(write: &mut WriteTxn) {
    let mut agents = write.open_table(AGENTS);
    let records = agents
        .iter()
        .map(|(id, record)| (id.value(), record.value().into_owned()))
        .collect::<Vec<_>>();

    for (agent_id, mut record) in records {
        record.mode = AgentMode::deep_default();
        agents.insert(&agent_id, SenValue::borrowed(&record));
    }
}
```

If the new `AgentRecord` cannot decode old bytes because a required field is missing, prefer a temporary custom `Decode` implementation for `AgentRecord` that accepts both old and new shapes. This keeps callers and table definitions using the real type:

```rust
#[derive(Clone, Debug, Encode)]
pub struct AgentRecord {
    pub display_name: Option<String>,
    pub workspace: WorkspaceInfo,
    pub status: Status,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub current_lineage: AgentLineageId,
    pub parent_agent: Option<AgentId>,
    pub mode: AgentMode,
    pub runtime: AgentRuntime,
}

impl Decode for AgentRecord {
    fn decode(data: &mut &[u8]) -> senax_encoder::Result<Self> {
        // Decode the struct shape explicitly. If `mode` is absent, default it
        // for legacy rows. Keep this implementation temporary and remove it
        // with the migration.
        todo!("decode current AgentRecord, accepting legacy rows without mode")
    }
}

fn migrate_agent_mode(write: &mut WriteTxn) {
    let mut agents = write.open_table(AGENTS);
    let records = agents
        .iter()
        .map(|(id, record)| (id.value(), record.value().into_owned()))
        .collect::<Vec<_>>();

    for (agent_id, record) in records {
        // Re-inserting writes the current encoded shape.
        agents.insert(&agent_id, SenValue::borrowed(&record));
    }
}
```

Keep the custom decode private/temporary and delete it with the migration. Only fall back to a separate legacy type when a direct `Decode` implementation would be substantially messier.

## Removal workflow

1. Add the new format id and migration.
2. Tell the user/developer to run the daemon/CLI once so their local DB opens and commits the new format id.
3. Wait for confirmation that the migrated build ran successfully.
4. Remove the migration function and any temporary legacy types in the next commit.
5. Keep `CURRENT_*_DB_FORMAT` at the new id.
6. Commit the removal separately if practical. In normal development, the migration should live for only 1-2 commits.

Useful checks:

```sh
cargo check -p rho-agent -p rho-daemon -p rho-cli
cargo run -q -p rho-cli -- debug migrate
cargo run -q -p rho-cli -- debug agents
```

`rho debug migrate` is the safe dry-run path: it copies the user's DB to a
tempfile, runs pending migrations on the copy, and then decodes the migrated
agent records. Use it before asking the user to run the real daemon/CLI.

## Common mistakes

- Changing record shape but forgetting to change `CURRENT_*_DB_FORMAT` when a migration must run.
- Adding a migration but not opening the DB before removing it.
- Keeping a temporary migration indefinitely.
- Using technical panic text; prefer actionable guidance for users.
- Writing a raw table migration when a temporary typed decode struct would work.
