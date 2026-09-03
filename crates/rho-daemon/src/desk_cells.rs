//! Cells-V2 persistence and the one-shot native-tree V1 conversion.

#![allow(dead_code)]

use camino::Utf8PathBuf;
use redb::TableDefinition;
use rho_core::AgentId;
use rho_db::{RhoDb, Sen, SenValue, WriteTxn};
use rho_desk::NodeId;
use rho_desk::cells::{
    Cell, CellMutation, DeviceId, Field, Snapshot, Stamp, Store, VerdictEvent, Version,
};
use senax_encoder::{Decode, Encode};

const CELLS: TableDefinition<Sen<CellAddress>, Sen<Cell>> =
    TableDefinition::new("rho_desk_cells_v2");
const VERDICTS: TableDefinition<Sen<VerdictKey>, Sen<VerdictEvent>> =
    TableDefinition::new("rho_desk_verdicts_v1");
const TEXTS: TableDefinition<Sen<NodeId>, Sen<rho_desk::NodeTextSnapshot>> =
    TableDefinition::new("rho_desk_node_text_v2");
const META: TableDefinition<(), Sen<CellMeta>> = TableDefinition::new("rho_desk_cell_meta_v2");
const MIGRATED: TableDefinition<(), Sen<MigrationReport>> =
    TableDefinition::new("rho_desk_tree_migrated_v2");
const MUTATIONS: TableDefinition<Sen<Stamp>, Sen<CellMutation>> =
    TableDefinition::new("rho_desk_mutations_v2");

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct CellAddress {
    node: NodeId,
    field: Field,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct VerdictKey {
    node: NodeId,
    stamp: Stamp,
}

#[derive(Clone, Debug, Encode, Decode)]
struct CellMeta {
    daemon_device: DeviceId,
    frontier: Version,
    device_node_namespaces: Vec<(DeviceId, u16)>,
    next_node_namespace: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub(crate) struct MigrationReport {
    pub(crate) warnings: Vec<String>,
    pub(crate) page_urls_awaiting_backfill: Vec<NodeId>,
    #[senax(default)]
    pub(crate) kind_counts: Vec<(rho_desk::cells::NodeKind, u64)>,
    #[senax(default)]
    pub(crate) rooted_by_chain_rule: u64,
    #[senax(default)]
    pub(crate) dropped_marks: u64,
}

#[derive(Clone)]
pub(crate) struct DeskCellStore {
    db: RhoDb,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MachineBinding {
    Agent {
        agent_id: AgentId,
        host: u64,
    },
    Page {
        page_id: rho_desk::PageId,
        url: String,
    },
    File {
        path: Utf8PathBuf,
    },
    Thread {
        workspace: String,
        channel: String,
        thread_ts: String,
    },
}

/// Whether a bind may move a node that already exists for this binding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindPlacement {
    /// The caller chose the parent, so an existing node moves there.
    Chosen,
    /// The parent is only a default; a node that already exists stays put.
    Default,
}

impl DeskCellStore {
    pub(crate) async fn new(
        db: RhoDb,
        machine_seed: u64,
    ) -> Result<(Self, MigrationReport, bool), String> {
        let mut write = db.write().await;
        let already_migrated = write.open_table(MIGRATED).get(&()).is_some();
        let report = initialize(&mut write, machine_seed)?;
        write.open_table(MUTATIONS);
        write.commit();
        Ok((Self { db }, report, !already_migrated))
    }

    pub(crate) fn sync_since(&self, known: &Version) -> Result<Snapshot, String> {
        let read = self.db.read();
        let meta = read
            .open_table(META)
            .get(&())
            .ok_or("Desk cells V2 metadata is missing")?
            .value()
            .into_owned();
        Store::from_snapshot(meta.daemon_device, read_snapshot(&read)?)
            .map(|store| store.since(known))
    }

    pub(crate) fn frontier(&self) -> Result<Version, String> {
        self.db
            .read()
            .open_table(META)
            .get(&())
            .map(|meta| meta.value().as_ref().frontier.clone())
            .ok_or_else(|| "Desk cells V2 metadata is missing".into())
    }

    pub(crate) fn texts(&self) -> Vec<rho_desk::NodeTextSnapshot> {
        self.db
            .read()
            .open_table(TEXTS)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .collect()
    }

    pub(crate) async fn apply_text(
        &self,
        session_namespace: u16,
        node_id: NodeId,
        operation: rho_desk::TextOperation,
        transaction: Option<rho_desk::TextTransaction>,
    ) -> Result<bool, String> {
        if operation.timestamp().replica_id != session_namespace {
            return Err("Desk text operation does not belong to this connection".into());
        }
        let mut write = self.db.write().await;
        let meta = load_meta_from_write(&mut write)?;
        let store =
            Store::from_snapshot(meta.daemon_device, read_snapshot_from_write(&mut write)?)?;
        let node = store
            .materialize()
            .into_iter()
            .find(|node| node.id == node_id)
            .ok_or("Desk text operation references an unknown or deleted node")?;
        if node.kind != rho_desk::cells::NodeKind::Note {
            return Err("machine-owned Desk nodes are read-only".into());
        }
        let mut text = write
            .open_table(TEXTS)
            .get(SenValue::borrowed(&node_id))
            .map(|value| value.value().into_owned())
            .unwrap_or(rho_desk::NodeTextSnapshot {
                node_id,
                operations: Vec::new(),
                transactions: Vec::new(),
            });
        if let Some(old) = text
            .operations
            .iter()
            .find(|old| old.timestamp() == operation.timestamp())
        {
            let old_transaction = text
                .transactions
                .iter()
                .find(|candidate| candidate.edit_ids.contains(&operation.timestamp()));
            return if old == &operation && old_transaction == transaction.as_ref() {
                Ok(false)
            } else {
                Err("Desk text operation timestamp was reused with different content".into())
            };
        }
        if let Some(transaction) = &transaction
            && (transaction.id.replica_id != operation.timestamp().replica_id
                || !transaction.edit_ids.contains(&operation.timestamp())
                || transaction.edit_ids.len() > 1024)
        {
            return Err("invalid Desk node text transaction".into());
        }
        let buffer_id = text::BufferId::new(1).map_err(|error| error.to_string())?;
        let validation_buffer = text.buffer(text::ReplicaId::REMOTE_SERVER.as_u16(), buffer_id)?;
        validate_text_operation(&text, &validation_buffer, &operation)?;
        text.operations.push(operation);
        if let Some(transaction) = transaction {
            text.transactions.push(transaction);
        }
        let buffer = text.buffer(text::ReplicaId::REMOTE_SERVER.as_u16(), buffer_id)?;
        if buffer.len() > 4 * 1024 * 1024 {
            return Err("Desk node text exceeds 4194304 bytes".into());
        }
        write
            .open_table(TEXTS)
            .insert(SenValue::owned(node_id), SenValue::owned(text));
        write.commit();
        Ok(true)
    }

    pub(crate) async fn node_namespace(&self, device: DeviceId) -> Result<u16, String> {
        let mut write = self.db.write().await;
        let mut meta = write
            .open_table(META)
            .get(&())
            .ok_or("Desk cells V2 metadata is missing")?
            .value()
            .into_owned();
        if let Some((_, namespace)) = meta
            .device_node_namespaces
            .iter()
            .find(|(candidate, _)| *candidate == device)
        {
            return Ok(*namespace);
        }
        let namespace = meta.next_node_namespace;
        meta.next_node_namespace = namespace
            .checked_add(1)
            .ok_or("Desk node namespace exhausted")?;
        meta.device_node_namespaces.push((device, namespace));
        write.open_table(META).insert(&(), SenValue::owned(meta));
        write.commit();
        Ok(namespace)
    }

    pub(crate) async fn apply_mutation(
        &self,
        session_device: DeviceId,
        session_namespace: u16,
        mutation: CellMutation,
    ) -> Result<(), String> {
        validate_mutation_bounds(&mutation)?;
        if mutation.stamp.device != session_device {
            return Err("Desk mutation stamp does not belong to this connection".into());
        }
        let mut write = self.db.write().await;
        let mut meta = write
            .open_table(META)
            .get(&())
            .ok_or("Desk cells V2 metadata is missing")?
            .value()
            .into_owned();
        let accepted = meta.frontier.get(&session_device).copied().unwrap_or(0);
        if mutation.stamp.version <= accepted {
            return match write
                .open_table(MUTATIONS)
                .get(SenValue::borrowed(&mutation.stamp))
            {
                Some(old) if old.value().as_ref() == &mutation => Ok(()),
                _ => Err("Desk mutation version is not newer than its device frontier".into()),
            };
        }
        let observed = meta.frontier.values().copied().max().unwrap_or(0);
        if mutation.stamp.version > observed.saturating_add(1) || observed == u64::MAX {
            return Err("Desk mutation version advances beyond the observable frontier".into());
        }
        let snapshot = read_snapshot_from_write(&mut write)?;
        let mut store = Store::from_snapshot(meta.daemon_device, snapshot)?;
        validate_user_mutation(&store, session_namespace, &mutation)?;
        store.apply_mutation(&mutation)?;
        persist_cells_and_verdicts(&mut write, &store.snapshot())?;
        meta.frontier = store.version().clone();
        write
            .open_table(META)
            .insert(&(), SenValue::borrowed(&meta));
        write
            .open_table(MUTATIONS)
            .insert(SenValue::owned(mutation.stamp), SenValue::owned(mutation));
        write.commit();
        Ok(())
    }

    pub(crate) fn validate_machine_parent(&self, parent: Option<NodeId>) -> Result<(), String> {
        let Some(parent) = parent else {
            return Ok(());
        };
        let read = self.db.read();
        let meta = read
            .open_table(META)
            .get(&())
            .ok_or("Desk cells V2 metadata is missing")?
            .value()
            .into_owned();
        let store = Store::from_snapshot(meta.daemon_device, read_snapshot(&read)?)?;
        let node = store
            .materialize()
            .into_iter()
            .find(|node| node.id == parent)
            .ok_or("Desk binding parent no longer exists")?;
        if node.kind != rho_desk::cells::NodeKind::Note {
            return Err("Desk machine rows must be filed under a user note".into());
        }
        Ok(())
    }

    /// The node a machine-owned thing already has, if any.
    pub(crate) fn machine_node(&self, binding: &MachineBinding) -> Option<NodeId> {
        let read = self.db.read();
        let meta = read.open_table(META).get(&())?.value().into_owned();
        let store = Store::from_snapshot(meta.daemon_device, read_snapshot(&read).ok()?).ok()?;
        store
            .materialize()
            .into_iter()
            .find(|node| binding_identity_matches(node, binding))
            .map(|node| node.id)
    }

    /// Create the node for a machine-owned thing, or return the one it
    /// already has. `parent` is `None` for the root.
    pub(crate) async fn bind_machine(
        &self,
        parent: Option<NodeId>,
        binding: MachineBinding,
        placement: BindPlacement,
    ) -> Result<(NodeId, bool), String> {
        use rho_desk::cells::{CellWrite, NodeKind, State, Timestamp, TimestampPrecision, Value};

        let mut write = self.db.write().await;
        let mut meta = load_meta_from_write(&mut write)?;
        let snapshot = read_snapshot_from_write(&mut write)?;
        let mut store = Store::from_snapshot(meta.daemon_device, snapshot)?;
        let nodes = store.materialize();
        if let Some(parent) = parent {
            let parent_node = nodes
                .iter()
                .find(|node| node.id == parent)
                .ok_or("Desk binding parent no longer exists")?;
            if parent_node.kind != NodeKind::Note {
                return Err("Desk machine rows must be filed under a user note".into());
            }
        }
        // A machine thing has one node wherever it is filed, so an existing
        // node is found by identity across the whole tree, never only under
        // the parent this call happens to name.
        if let Some(existing) = nodes
            .iter()
            .find(|node| binding_identity_matches(node, &binding))
        {
            let mut writes = Vec::new();
            if let MachineBinding::Page { url, .. } = &binding
                && existing.fields.get(&Field::Url)
                    != Some(&rho_desk::cells::Value::Text(url.clone()))
            {
                validate_page_url(url)?;
                writes.push(CellWrite {
                    node: existing.id,
                    field: Field::Url,
                    value: Value::Text(url.clone()),
                });
            }
            // New traffic supersedes the verdict that quieted a thread: the
            // answer was to the older message, so its node opens again.
            if matches!(binding, MachineBinding::Thread { .. }) && existing.state != State::Open {
                writes.push(CellWrite {
                    node: existing.id,
                    field: Field::State,
                    value: Value::State(State::Open),
                });
            }
            if placement == BindPlacement::Chosen && existing.parent != parent {
                writes.push(CellWrite {
                    node: existing.id,
                    field: Field::Parent,
                    value: Value::Parent(parent),
                });
            }
            if writes.is_empty() {
                return Ok((existing.id, false));
            }
            let version = next_daemon_version(&meta)?;
            let mutation = CellMutation {
                stamp: Stamp {
                    device: meta.daemon_device,
                    version,
                },
                writes,
                verdict: None,
            };
            store.apply_mutation(&mutation)?;
            persist_accepted_mutation(&mut write, &mut meta, &store, mutation);
            write.commit();
            return Ok((existing.id, true));
        }
        let version = next_daemon_version(&meta)?;
        let namespace = daemon_node_namespace(&meta)?;
        let node = NodeId {
            replica_id: namespace,
            counter: version,
        };
        let kind = match &binding {
            MachineBinding::Agent { .. } => NodeKind::Agent,
            MachineBinding::Page { .. } => NodeKind::Page,
            MachineBinding::File { .. } => NodeKind::File,
            MachineBinding::Thread { .. } => NodeKind::Thread,
        };
        let now = Timestamp {
            unix_ms: i64::try_from(rho_core::UnixMs::now().0)
                .map_err(|_| "current time exceeds Desk timestamp range")?,
            precision: TimestampPrecision::Millisecond,
        };
        let mut writes = vec![
            CellWrite {
                node,
                field: Field::Kind,
                value: Value::Kind(kind),
            },
            CellWrite {
                node,
                field: Field::Parent,
                value: Value::Parent(parent),
            },
            CellWrite {
                node,
                field: Field::Deleted,
                value: Value::Bool(false),
            },
            CellWrite {
                node,
                field: Field::CreatedAt,
                value: Value::Timestamp(now),
            },
            CellWrite {
                node,
                field: Field::State,
                value: Value::State(State::Open),
            },
            CellWrite {
                node,
                field: Field::DeferUntil,
                value: Value::OptionalTimestamp(None),
            },
            CellWrite {
                node,
                field: Field::Deadline,
                value: Value::OptionalTimestamp(None),
            },
            CellWrite {
                node,
                field: Field::PaceDays,
                value: Value::Days(0),
            },
        ];
        match binding {
            MachineBinding::Agent { agent_id, host } => {
                writes.push(CellWrite {
                    node,
                    field: Field::AgentId,
                    value: Value::AgentId(agent_id),
                });
                writes.push(CellWrite {
                    node,
                    field: Field::Host,
                    value: Value::Host(host),
                });
            }
            MachineBinding::Page { page_id, url } => {
                validate_page_url(&url)?;
                writes.push(CellWrite {
                    node,
                    field: Field::PageRef,
                    value: Value::PageRef(page_id),
                });
                writes.push(CellWrite {
                    node,
                    field: Field::Url,
                    value: Value::Text(url),
                });
            }
            MachineBinding::File { path } => {
                writes.push(CellWrite {
                    node,
                    field: Field::Path,
                    value: Value::Path(path),
                });
            }
            MachineBinding::Thread {
                workspace,
                channel,
                thread_ts,
            } => {
                writes.push(CellWrite {
                    node,
                    field: Field::Workspace,
                    value: Value::Text(workspace),
                });
                writes.push(CellWrite {
                    node,
                    field: Field::Channel,
                    value: Value::Text(channel),
                });
                writes.push(CellWrite {
                    node,
                    field: Field::ThreadTs,
                    value: Value::Text(thread_ts),
                });
            }
        }
        let mutation = CellMutation {
            stamp: Stamp {
                device: meta.daemon_device,
                version,
            },
            writes,
            verdict: None,
        };
        validate_mutation_bounds(&mutation)?;
        store.apply_mutation(&mutation)?;
        persist_accepted_mutation(&mut write, &mut meta, &store, mutation);
        write.commit();
        Ok((node, true))
    }

    pub(crate) async fn unbind_machine(&self, binding: &MachineBinding) -> Result<bool, String> {
        let mut write = self.db.write().await;
        let mut meta = load_meta_from_write(&mut write)?;
        let snapshot = read_snapshot_from_write(&mut write)?;
        let mut store = Store::from_snapshot(meta.daemon_device, snapshot)?;
        let nodes = store
            .materialize()
            .into_iter()
            .filter(|node| binding_identity_matches(node, binding))
            .map(|node| node.id)
            .collect::<Vec<_>>();
        if nodes.is_empty() {
            return Ok(false);
        }
        let version = next_daemon_version(&meta)?;
        let mutation = CellMutation {
            stamp: Stamp {
                device: meta.daemon_device,
                version,
            },
            writes: nodes
                .into_iter()
                .map(|node| rho_desk::cells::CellWrite {
                    node,
                    field: Field::Deleted,
                    value: rho_desk::cells::Value::Bool(true),
                })
                .collect(),
            verdict: None,
        };
        validate_mutation_bounds(&mutation)?;
        store.apply_mutation(&mutation)?;
        persist_accepted_mutation(&mut write, &mut meta, &store, mutation);
        write.commit();
        Ok(true)
    }

    pub(crate) async fn unbind_machine_node(&self, node: NodeId) -> Result<bool, String> {
        let mut write = self.db.write().await;
        let mut meta = load_meta_from_write(&mut write)?;
        let snapshot = read_snapshot_from_write(&mut write)?;
        let mut store = Store::from_snapshot(meta.daemon_device, snapshot)?;
        let Some(existing) = store
            .materialize()
            .into_iter()
            .find(|candidate| candidate.id == node)
        else {
            return Ok(false);
        };
        if existing.kind != rho_desk::cells::NodeKind::Page {
            return Err("Desk page unbind must target a machine-owned page row".into());
        }
        let version = next_daemon_version(&meta)?;
        let mutation = CellMutation {
            stamp: Stamp {
                device: meta.daemon_device,
                version,
            },
            writes: vec![rho_desk::cells::CellWrite {
                node,
                field: Field::Deleted,
                value: rho_desk::cells::Value::Bool(true),
            }],
            verdict: None,
        };
        validate_mutation_bounds(&mutation)?;
        store.apply_mutation(&mutation)?;
        persist_accepted_mutation(&mut write, &mut meta, &store, mutation);
        write.commit();
        Ok(true)
    }
}

fn validate_text_operation(
    text: &rho_desk::NodeTextSnapshot,
    buffer: &text::Buffer,
    operation: &rho_desk::TextOperation,
) -> Result<(), String> {
    use rho_desk::TextOperation;

    let native = operation.to_text()?;
    let timestamp = operation.timestamp();
    if timestamp.value == 0 || timestamp.replica_id == 0 {
        return Err("Desk text operation timestamp is invalid".into());
    }
    let version = match operation {
        TextOperation::Edit { version, .. } | TextOperation::Undo { version, .. } => version,
    };
    if version.len() > 4096
        || version.iter().any(|clock| clock.value == 0)
        || version
            .windows(2)
            .any(|pair| pair[0].replica_id >= pair[1].replica_id)
    {
        return Err("Desk text source version is not canonical or bounded".into());
    }
    let known = text
        .operations
        .iter()
        .map(rho_desk::TextOperation::timestamp)
        .fold(
            std::collections::BTreeMap::<u16, u32>::new(),
            |mut known, clock| {
                known
                    .entry(clock.replica_id)
                    .and_modify(|value| *value = (*value).max(clock.value))
                    .or_insert(clock.value);
                known
            },
        );
    if timestamp.value
        <= known
            .get(&timestamp.replica_id)
            .copied()
            .unwrap_or_default()
    {
        return Err("Desk text operation timestamp does not advance its replica".into());
    }
    if version
        .iter()
        .any(|clock| clock.value > known.get(&clock.replica_id).copied().unwrap_or_default())
    {
        return Err("Desk text source version has not been observed".into());
    }
    let observed = |clock: rho_desk::TreeClock| {
        version
            .binary_search_by_key(&clock.replica_id, |candidate| candidate.replica_id)
            .ok()
            .is_some_and(|index| version[index].value >= clock.value)
    };
    match operation {
        TextOperation::Edit { ranges, .. } => {
            let full_len = text
                .operations
                .iter()
                .filter(|old| observed(old.timestamp()))
                .filter_map(|old| match old {
                    TextOperation::Edit { new_text, .. } => {
                        Some(new_text.iter().map(String::len).sum::<usize>())
                    }
                    TextOperation::Undo { .. } => None,
                })
                .try_fold(0usize, |total, len| total.checked_add(len))
                .ok_or("Desk text history length overflow")? as u64;
            if ranges
                .iter()
                .any(|(start, end)| start > end || *end > full_len)
                || ranges.windows(2).any(|pair| pair[0].1 > pair[1].0)
            {
                return Err("Desk text edit ranges are invalid for its source version".into());
            }
            let text::Operation::Edit(native) = &native else {
                unreachable!("converted edit operation changed variants")
            };
            if !buffer.snapshot().are_valid_full_offsets_for_version(
                native
                    .ranges
                    .iter()
                    .flat_map(|range| [range.start, range.end]),
                &native.version,
            ) {
                return Err("Desk text edit range splits a UTF-8 character".into());
            }
        }
        TextOperation::Undo { counts, .. } => {
            if counts.len() > 65_536
                || counts.iter().any(|(clock, count)| {
                    clock.value == 0
                        || *count == 0
                        || !observed(*clock)
                        || !text.operations.iter().any(|old| old.timestamp() == *clock)
                })
                || counts.windows(2).any(|pair| pair[0].0 >= pair[1].0)
            {
                return Err("Desk text undo counts are invalid or unbounded".into());
            }
        }
    }
    Ok(())
}

fn load_meta_from_write(write: &mut WriteTxn) -> Result<CellMeta, String> {
    write
        .open_table(META)
        .get(&())
        .map(|meta| meta.value().into_owned())
        .ok_or_else(|| "Desk cells V2 metadata is missing".into())
}

fn daemon_node_namespace(meta: &CellMeta) -> Result<u16, String> {
    meta.device_node_namespaces
        .iter()
        .find_map(|(device, namespace)| (*device == meta.daemon_device).then_some(*namespace))
        .ok_or_else(|| "Desk daemon node namespace is missing".into())
}

fn next_daemon_version(meta: &CellMeta) -> Result<u64, String> {
    meta.frontier
        .values()
        .copied()
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| "Desk daemon version exhausted".into())
}

fn persist_accepted_mutation(
    write: &mut WriteTxn,
    meta: &mut CellMeta,
    store: &Store,
    mutation: CellMutation,
) {
    persist_cells_and_verdicts(write, &store.snapshot())
        .expect("validated Desk store snapshot persists");
    meta.frontier = store.version().clone();
    write.open_table(META).insert(&(), SenValue::borrowed(meta));
    write
        .open_table(MUTATIONS)
        .insert(SenValue::owned(mutation.stamp), SenValue::owned(mutation));
}

fn binding_identity_matches(
    node: &rho_desk::cells::MaterializedNode,
    binding: &MachineBinding,
) -> bool {
    use rho_desk::cells::Value;
    match binding {
        MachineBinding::Agent { agent_id, host } => {
            node.kind == rho_desk::cells::NodeKind::Agent
                && node.fields.get(&Field::AgentId) == Some(&Value::AgentId(agent_id.clone()))
                && node.fields.get(&Field::Host) == Some(&Value::Host(*host))
        }
        MachineBinding::Page { page_id, .. } => {
            node.kind == rho_desk::cells::NodeKind::Page
                && node.fields.get(&Field::PageRef) == Some(&Value::PageRef(*page_id))
        }
        MachineBinding::File { path } => {
            node.kind == rho_desk::cells::NodeKind::File
                && node.fields.get(&Field::Path) == Some(&Value::Path(path.clone()))
        }
        MachineBinding::Thread {
            workspace,
            channel,
            thread_ts,
        } => {
            node.kind == rho_desk::cells::NodeKind::Thread
                && node.fields.get(&Field::Workspace) == Some(&Value::Text(workspace.clone()))
                && node.fields.get(&Field::Channel) == Some(&Value::Text(channel.clone()))
                && node.fields.get(&Field::ThreadTs) == Some(&Value::Text(thread_ts.clone()))
        }
    }
}

fn validate_page_url(value: &str) -> Result<(), String> {
    if value.len() > 4096 {
        return Err("Desk page URL exceeds 4096 bytes".into());
    }
    let url = url::Url::parse(value).map_err(|error| format!("invalid Desk page URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Desk page URL must use http or https".into());
    }
    Ok(())
}

fn validate_mutation_bounds(mutation: &CellMutation) -> Result<(), String> {
    for write in &mutation.writes {
        if matches!(&write.field, Field::Tag(tag) if tag.is_empty() || tag.len() > 1024) {
            return Err("Desk tag must contain 1–1024 bytes".into());
        }
        match &write.value {
            rho_desk::cells::Value::Text(value) if value.len() > 64 * 1024 => {
                return Err("Desk cell text exceeds 65536 bytes".into());
            }
            rho_desk::cells::Value::Path(path) if path.as_str().len() > 4096 => {
                return Err("Desk path exceeds 4096 bytes".into());
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_user_mutation(
    store: &Store,
    session_namespace: u16,
    mutation: &CellMutation,
) -> Result<(), String> {
    use rho_desk::cells::{NodeKind, Value, VerdictEvent};

    let created = mutation
        .writes
        .iter()
        .filter_map(|write| (write.field == Field::Kind).then_some(write.node))
        .collect::<std::collections::BTreeSet<_>>();
    let mut verdict_fields = std::collections::BTreeSet::new();
    if let Some((verdict_node, event)) = &mutation.verdict {
        let (verdict, changes) = match event {
            VerdictEvent::Applied {
                verdict, changes, ..
            } => (verdict, changes),
            VerdictEvent::Undone { of } => match store.verdict_event(*verdict_node, *of) {
                Some(VerdictEvent::Applied {
                    verdict, changes, ..
                }) => (verdict, changes),
                _ => return Err("Desk verdict undo does not reference an applied verdict".into()),
            },
        };
        validate_verdict_shape(
            *verdict_node,
            verdict,
            changes,
            mutation,
            &created,
            matches!(event, VerdictEvent::Applied { .. }),
        )?;
        for change in changes {
            if !verdict_fields.insert((change.node, change.field.clone())) {
                return Err("Desk verdict contains duplicate field changes".into());
            }
            let (expected_current, expected_write) = match event {
                VerdictEvent::Applied { .. } => (&change.before, &change.after),
                VerdictEvent::Undone { .. } => (&change.after, &change.before),
            };
            let todo_virtual_source = matches!(event, VerdictEvent::Applied { .. })
                && matches!(verdict, rho_desk::cells::Verdict::Todo { note } if *note == change.node)
                && store.value(change.node, &Field::Kind).is_none()
                && matches!(
                    (&change.field, expected_current),
                    (Field::Deleted, Some(Value::Bool(true)))
                        | (Field::DeferUntil, Some(Value::OptionalTimestamp(None)))
                        | (Field::PaceDays, Some(Value::Days(0)))
                );
            if !todo_virtual_source
                && store.value(change.node, &change.field) != expected_current.as_ref()
            {
                return Err("Desk verdict source value does not match current state".into());
            }
            let Some(expected_write) = expected_write else {
                return Err("Desk verdict changes cannot remove a cell".into());
            };
            if !mutation.writes.iter().any(|write| {
                write.node == change.node
                    && write.field == change.field
                    && &write.value == expected_write
            }) {
                return Err("Desk verdict change is not applied by its mutation".into());
            }
        }
    }
    for write in &mutation.writes {
        match store.value(write.node, &Field::Kind) {
            Some(Value::Kind(NodeKind::Note)) => {
                if matches!(write.field, Field::Kind | Field::CreatedAt) {
                    return Err("Desk node kind and creation time are write-once".into());
                }
                if !user_field(&write.field) {
                    return Err("Desk note mutation contains a machine-owned field".into());
                }
            }
            Some(Value::Kind(_)) => {
                if !matches!(
                    write.field,
                    Field::State | Field::DeferUntil | Field::Parent
                ) || !verdict_fields.contains(&(write.node, write.field.clone()))
                {
                    return Err("machine-owned Desk nodes are read-only outside a verdict".into());
                }
            }
            Some(_) => return Err("Desk node kind cell is malformed".into()),
            None => {
                if !created.contains(&write.node) {
                    return Err("Desk mutation references an unknown or deleted node".into());
                }
                if write.node.replica_id != session_namespace {
                    return Err("new Desk node does not belong to this connection".into());
                }
                if !user_field(&write.field) && write.field != Field::Kind {
                    return Err("new Desk note contains a machine-owned field".into());
                }
                if write.field == Field::Kind && write.value != Value::Kind(NodeKind::Note) {
                    return Err("clients may only create user-owned Desk notes".into());
                }
            }
        }
    }
    for node in &created {
        let has = |field: &Field| {
            mutation
                .writes
                .iter()
                .any(|write| write.node == *node && &write.field == field)
        };
        for field in [
            Field::Kind,
            Field::Parent,
            Field::Deleted,
            Field::CreatedAt,
            Field::State,
            Field::DeferUntil,
            Field::Deadline,
            Field::PaceDays,
        ] {
            if !has(&field) {
                return Err("new Desk note is missing a required common field".into());
            }
        }
    }
    Ok(())
}

fn validate_verdict_shape(
    verdict_node: NodeId,
    verdict: &rho_desk::cells::Verdict,
    changes: &[rho_desk::cells::FieldChange],
    mutation: &CellMutation,
    created: &std::collections::BTreeSet<NodeId>,
    applied: bool,
) -> Result<(), String> {
    use rho_desk::cells::{Value, Verdict};

    // The entry is checked against the shape rho-desk builds for this
    // verdict, so the writer and the checker share one definition. `before`
    // is the writer's to state; everything else has to match.
    let expected = rho_desk::cells::verdict_changes(
        verdict_node,
        verdict,
        changes
            .iter()
            .find(|change| change.node == verdict_node)
            .and_then(|change| change.before.clone()),
        rho_desk::cells::todo_cadence(changes),
    )?;
    let mut shape = changes.to_vec();
    shape.sort_by(|left, right| left.field.cmp(&right.field));
    let mut expected_shape = expected;
    expected_shape.sort_by(|left, right| left.field.cmp(&right.field));
    let valid = shape == expected_shape
        && match verdict {
            // The note a todo creates has to be created by the same mutation
            // and filed under the node the verdict was dealt on.
            Verdict::Todo { note } => {
                !applied
                    || (created.contains(note)
                        && mutation.writes.iter().any(|write| {
                            write.node == *note
                                && write.field == Field::Parent
                                && write.value == Value::Parent(Some(verdict_node))
                        }))
            }
            _ => true,
        };
    if !valid {
        return Err("Desk verdict changes do not match the verdict semantics".into());
    }
    Ok(())
}

fn user_field(field: &Field) -> bool {
    matches!(
        field,
        Field::Parent
            | Field::Deleted
            | Field::CreatedAt
            | Field::State
            | Field::DeferUntil
            | Field::Deadline
            | Field::PaceDays
            | Field::Tag(_)
    )
}

pub(crate) fn initialize(
    write: &mut WriteTxn,
    machine_seed: u64,
) -> Result<MigrationReport, String> {
    let meta_exists = write.open_table(META).get(&()).is_some();
    let report = write
        .open_table(MIGRATED)
        .get(&())
        .map(|value| value.value().into_owned());
    match (meta_exists, report) {
        (true, Some(report)) => {
            upgrade_slice_1a_state(write, machine_seed)?;
            return Ok(report);
        }
        (true, None) => {
            return Err("Desk cells V2 state exists without its migration marker".into());
        }
        (false, Some(_)) => {
            return Err("Desk cells V2 migration marker exists without state".into());
        }
        (false, None) => {}
    }

    let legacy = crate::desk_tree_v1::load_replayed(write)?
        .ok_or("Desk native-tree V1 state is missing during cells migration")?;
    let daemon_device = DeviceId(*uuid::Uuid::new_v4().as_bytes());
    let migrated = migrate_v1(legacy, daemon_device, machine_seed)?;
    let daemon_namespace = migrated.next_node_namespace;
    let next_node_namespace = daemon_namespace
        .checked_add(1)
        .ok_or("Desk node namespace exhausted during migration")?;
    let report = migration_report(&migrated, daemon_device)?;
    persist_snapshot(write, &migrated.snapshot, migrated.texts)?;
    write.open_table(META).insert(
        &(),
        SenValue::owned(CellMeta {
            daemon_device,
            frontier: migrated.snapshot.version,
            device_node_namespaces: vec![(daemon_device, daemon_namespace)],
            next_node_namespace,
        }),
    );
    write
        .open_table(MIGRATED)
        .insert(&(), SenValue::borrowed(&report));
    Ok(report)
}

fn upgrade_slice_1a_state(write: &mut WriteTxn, machine_seed: u64) -> Result<(), String> {
    let mut meta = load_meta_from_write(write)?;
    let has_daemon_namespace = meta
        .device_node_namespaces
        .iter()
        .any(|(device, _)| *device == meta.daemon_device);
    if !has_daemon_namespace {
        let namespace = meta.next_node_namespace;
        meta.next_node_namespace = namespace
            .checked_add(1)
            .ok_or("Desk node namespace exhausted while upgrading cells metadata")?;
        meta.device_node_namespaces
            .push((meta.daemon_device, namespace));
    }
    let snapshot = read_snapshot_from_write(write)?;
    let mut store = Store::from_snapshot(meta.daemon_device, snapshot)?;
    let current = store.snapshot();
    let kind_nodes = current
        .cells
        .iter()
        .filter_map(|cell| {
            matches!(&cell.value, rho_desk::cells::Value::Kind(_)).then_some(cell.node)
        })
        .collect::<std::collections::BTreeSet<_>>();
    let agent_nodes = current
        .cells
        .iter()
        .filter_map(|cell| {
            (cell.field == Field::Kind
                && cell.value == rho_desk::cells::Value::Kind(rho_desk::cells::NodeKind::Agent))
            .then_some(cell.node)
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut writes = current
        .cells
        .into_iter()
        .filter(|cell| {
            agent_nodes.contains(&cell.node)
                && cell.field == Field::Host
                && cell.value == rho_desk::cells::Value::Host(0)
                && machine_seed != 0
        })
        .map(|cell| rho_desk::cells::CellWrite {
            node: cell.node,
            field: Field::Host,
            value: rho_desk::cells::Value::Host(machine_seed),
        })
        .collect::<Vec<_>>();
    for node in kind_nodes {
        for field in [Field::DeferUntil, Field::Deadline] {
            if store.value(node, &field).is_none() {
                writes.push(rho_desk::cells::CellWrite {
                    node,
                    field,
                    value: rho_desk::cells::Value::OptionalTimestamp(None),
                });
            }
        }
    }
    if writes.is_empty() {
        write.open_table(META).insert(&(), SenValue::owned(meta));
        return Ok(());
    }
    for writes in writes.chunks(4096) {
        let mutation = CellMutation {
            stamp: Stamp {
                device: meta.daemon_device,
                version: next_daemon_version(&meta)?,
            },
            writes: writes.to_vec(),
            verdict: None,
        };
        store.apply_mutation(&mutation)?;
        persist_accepted_mutation(write, &mut meta, &store, mutation);
    }
    Ok(())
}

struct ConvertedV1 {
    snapshot: Snapshot,
    texts: Vec<rho_desk::NodeTextSnapshot>,
    warnings: Vec<String>,
    next_node_namespace: u16,
}

fn migration_report(migrated: &ConvertedV1, device: DeviceId) -> Result<MigrationReport, String> {
    let page_urls_awaiting_backfill =
        migrated
            .snapshot
            .cells
            .iter()
            .filter(|cell| {
                cell.field == Field::PageRef
                    && !migrated.snapshot.cells.iter().any(|candidate| {
                        candidate.node == cell.node && candidate.field == Field::Url
                    })
            })
            .map(|cell| cell.node)
            .collect();
    let migrated_store = Store::from_snapshot(device, migrated.snapshot.clone())?;
    let materialized = migrated_store.materialize();
    let mut kind_counts = std::collections::BTreeMap::new();
    let mut rooted_by_chain_rule = 0;
    for node in &materialized {
        *kind_counts.entry(node.kind).or_insert(0) += 1;
        if node.parent.is_none()
            && matches!(
                migrated_store.value(node.id, &Field::Parent),
                Some(rho_desk::cells::Value::Parent(Some(_)))
            )
        {
            rooted_by_chain_rule += 1;
        }
    }
    Ok(MigrationReport {
        warnings: migrated.warnings.clone(),
        page_urls_awaiting_backfill,
        kind_counts: kind_counts.into_iter().collect(),
        rooted_by_chain_rule,
        dropped_marks: 0,
    })
}

fn migrate_v1(
    legacy: crate::desk_tree_v1::Snapshot,
    device: DeviceId,
    machine_seed: u64,
) -> Result<ConvertedV1, String> {
    use rho_desk::cells::{NodeKind, State, Store, Timestamp, TimestampPrecision, Value};

    use crate::desk_tree_v1 as v1;
    let migration_namespace = legacy
        .nodes
        .iter()
        .map(|node| node.id.replica_id)
        .chain(legacy.replicas.iter().map(|replica| replica.replica_id))
        .max()
        .unwrap_or(text::ReplicaId::FIRST_COLLAB_ID.as_u16().saturating_sub(1))
        .checked_add(1)
        .ok_or("Desk node namespace exhausted during migration")?;
    let order = v1::Document::from_snapshot(legacy.clone())?
        .materialize()
        .into_iter()
        .enumerate()
        .map(|(index, node)| (node.id, index as i64))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut store = Store::new(device);
    let mut warnings = Vec::new();
    let mut synthetic_file_count = 0_u64;
    for (fallback_order, old) in legacy.nodes.iter().enumerate() {
        let id = current_id(old.id);
        let kind = match old.kind {
            v1::NodeKind::Agent => NodeKind::Agent,
            v1::NodeKind::Page => NodeKind::Page,
            v1::NodeKind::File => NodeKind::File,
            v1::NodeKind::Heading | v1::NodeKind::Prose | v1::NodeKind::Draft => NodeKind::Note,
        };
        let parent = old
            .placements
            .iter()
            .max_by_key(|placement| placement.timestamp)
            .and_then(|placement| placement.parent)
            .map(current_id);
        store.write(id, Field::Kind, Value::Kind(kind))?;
        store.write(id, Field::Parent, Value::Parent(parent))?;
        store.write(
            id,
            Field::CreatedAt,
            Value::Timestamp(Timestamp {
                unix_ms: order
                    .get(&old.id)
                    .copied()
                    .unwrap_or_else(|| order.len() as i64 + fallback_order as i64),
                precision: TimestampPrecision::Millisecond,
            }),
        )?;
        store.write(id, Field::Deleted, Value::Bool(old.deleted_at.is_some()))?;
        let state = old
            .temporal
            .iter()
            .filter_map(|(kind, stamp, mark)| {
                mark.as_ref()?;
                let value = match kind {
                    v1::TemporalKind::Todo => State::Open,
                    v1::TemporalKind::Done => State::Done,
                    v1::TemporalKind::Discarded => State::Dismissed,
                    _ => return None,
                };
                Some((*stamp, value))
            })
            .max_by_key(|(stamp, _)| *stamp)
            .map_or(State::Open, |(_, state)| state);
        store.write(id, Field::State, Value::State(state))?;
        let pace = old
            .temporal
            .iter()
            .filter(|(kind, _, _)| {
                matches!(
                    kind,
                    v1::TemporalKind::Todo
                        | v1::TemporalKind::Defer
                        | v1::TemporalKind::Reminder
                        | v1::TemporalKind::Deadline
                )
            })
            .filter_map(|(kind, stamp, mark)| {
                mark.map(|mark| {
                    (
                        *stamp,
                        if *kind == v1::TemporalKind::Defer {
                            0
                        } else {
                            mark.pace_days
                        },
                    )
                })
            })
            .max_by_key(|(stamp, _)| *stamp)
            .map_or(0, |(_, pace)| pace);
        store.write(id, Field::PaceDays, Value::Days(pace))?;
        let defer = old
            .temporal
            .iter()
            .filter_map(|(kind, stamp, mark)| {
                matches!(
                    kind,
                    v1::TemporalKind::Todo | v1::TemporalKind::Defer | v1::TemporalKind::Reminder
                )
                .then_some((*stamp, *kind, *mark))
            })
            .filter_map(|(stamp, kind, mark)| mark.map(|mark| (stamp, kind, mark)))
            .max_by_key(|(stamp, _, _)| *stamp);
        let defer_until = defer.map(|(_, _, mark)| timestamp(mark)).transpose()?;
        store.write(id, Field::DeferUntil, Value::OptionalTimestamp(defer_until))?;
        if let Some((_, source, _)) = defer {
            if source == v1::TemporalKind::Reminder {
                warnings.push(format!(
                    "node {:?}: reminder converted to defer-until",
                    old.id
                ));
            }
        }
        let deadline = old
            .temporal
            .iter()
            .filter(|(kind, _, _)| *kind == v1::TemporalKind::Deadline)
            .max_by_key(|(_, stamp, _)| *stamp)
            .and_then(|(_, _, mark)| *mark)
            .map(timestamp)
            .transpose()?;
        store.write(id, Field::Deadline, Value::OptionalTimestamp(deadline))?;
        for (tag, _, present) in &old.tags {
            store.write(id, Field::Tag(tag.clone()), Value::Bool(*present))?;
        }
        let active_bindings = old.bindings.iter().fold(
            std::collections::BTreeMap::new(),
            |mut latest, (kind, stamp, binding)| {
                if latest
                    .get(kind)
                    .is_none_or(|(latest_stamp, _)| stamp > latest_stamp)
                {
                    latest.insert(*kind, (*stamp, binding));
                }
                latest
            },
        );
        for (binding_kind, (_, binding)) in active_bindings {
            let Some(binding) = binding else { continue };
            if binding.kind() != binding_kind {
                return Err(format!(
                    "legacy node {:?} has a binding whose value does not match its kind",
                    old.id
                ));
            }
            match binding {
                v1::Binding::Agent(agent) => {
                    if kind != NodeKind::Agent {
                        return Err(format!(
                            "legacy {:?} node {:?} has an agent binding",
                            old.kind, old.id
                        ));
                    }
                    store.write(id, Field::AgentId, Value::AgentId(agent.clone()))?;
                    store.write(id, Field::Host, Value::Host(machine_seed))?;
                }
                v1::Binding::Page(page) => {
                    if kind != NodeKind::Page {
                        return Err(format!(
                            "legacy {:?} node {:?} has a page binding",
                            old.kind, old.id
                        ));
                    }
                    store.write(id, Field::PageRef, Value::PageRef(rho_desk::PageId(page.0)))?;
                    warnings.push(format!(
                        "node {:?}: page URL awaits GUI registry backfill",
                        old.id
                    ));
                }
                v1::Binding::File(path) => {
                    if kind == NodeKind::File {
                        store.write(id, Field::Path, Value::Path(path.clone()))?;
                    } else if old.kind == v1::NodeKind::Heading {
                        synthetic_file_count += 1;
                        let file = NodeId {
                            replica_id: migration_namespace,
                            counter: synthetic_file_count,
                        };
                        let created_at = match store.value(id, &Field::CreatedAt) {
                            Some(Value::Timestamp(value)) => *value,
                            _ => unreachable!("migration just wrote CreatedAt"),
                        };
                        store.write(file, Field::Kind, Value::Kind(NodeKind::File))?;
                        store.write(file, Field::Parent, Value::Parent(Some(id)))?;
                        store.write(file, Field::CreatedAt, Value::Timestamp(created_at))?;
                        store.write(file, Field::Deleted, Value::Bool(old.deleted_at.is_some()))?;
                        store.write(file, Field::State, Value::State(State::Open))?;
                        store.write(file, Field::DeferUntil, Value::OptionalTimestamp(None))?;
                        store.write(file, Field::Deadline, Value::OptionalTimestamp(None))?;
                        store.write(file, Field::PaceDays, Value::Days(0))?;
                        store.write(file, Field::Path, Value::Path(path.clone()))?;
                    } else {
                        return Err(format!(
                            "legacy {:?} node {:?} has a file binding",
                            old.kind, old.id
                        ));
                    }
                }
            }
        }
    }
    if synthetic_file_count > 0 {
        warnings.push(format!(
            "split {synthetic_file_count} legacy heading file bindings into File children"
        ));
    }
    let texts = legacy
        .texts
        .into_iter()
        .map(crate::desk_tree_v1::text_into_current)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ConvertedV1 {
        snapshot: store.snapshot(),
        texts,
        warnings,
        next_node_namespace: if synthetic_file_count == 0 {
            migration_namespace
        } else {
            migration_namespace
                .checked_add(1)
                .ok_or("Desk node namespace exhausted after creating migration nodes")?
        },
    })
}

fn current_id(id: crate::desk_tree_v1::NodeId) -> NodeId {
    NodeId {
        replica_id: id.replica_id,
        counter: id.counter,
    }
}

fn timestamp(
    mark: crate::desk_tree_v1::TemporalMark,
) -> Result<rho_desk::cells::Timestamp, String> {
    use rho_desk::cells::{Timestamp, TimestampPrecision};
    let at = mark.at().ok_or("invalid legacy Desk temporal mark")?;
    Ok(Timestamp {
        unix_ms: at.and_utc().timestamp_millis(),
        precision: if mark.minute_of_day.is_some() {
            TimestampPrecision::Minute
        } else {
            TimestampPrecision::Day
        },
    })
}

fn read_snapshot(read: &rho_db::ReadTxn) -> Result<Snapshot, String> {
    let version = read
        .open_table(META)
        .get(&())
        .ok_or("Desk cells V2 metadata is missing")?
        .value()
        .as_ref()
        .frontier
        .clone();
    Ok(Snapshot {
        cells: read
            .open_table(CELLS)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .collect(),
        verdicts: read
            .open_table(VERDICTS)
            .iter()
            .map(|(key, value)| {
                let key = key.value().into_owned();
                (key.node, key.stamp, value.value().into_owned())
            })
            .collect(),
        version,
    })
}

fn read_snapshot_from_write(write: &mut WriteTxn) -> Result<Snapshot, String> {
    let version = write
        .open_table(META)
        .get(&())
        .ok_or("Desk cells V2 metadata is missing")?
        .value()
        .as_ref()
        .frontier
        .clone();
    let cells = write
        .open_table(CELLS)
        .iter()
        .map(|(_, value)| value.value().into_owned())
        .collect();
    let verdicts = write
        .open_table(VERDICTS)
        .iter()
        .map(|(key, value)| {
            let key = key.value().into_owned();
            (key.node, key.stamp, value.value().into_owned())
        })
        .collect();
    Ok(Snapshot {
        cells,
        verdicts,
        version,
    })
}

fn persist_cells_and_verdicts(write: &mut WriteTxn, snapshot: &Snapshot) -> Result<(), String> {
    let mut cells = write.open_table(CELLS);
    for cell in &snapshot.cells {
        cells.insert(
            SenValue::owned(CellAddress {
                node: cell.node,
                field: cell.field.clone(),
            }),
            SenValue::borrowed(cell),
        );
    }
    drop(cells);
    let mut verdicts = write.open_table(VERDICTS);
    for (node, stamp, event) in &snapshot.verdicts {
        verdicts.insert(
            SenValue::owned(VerdictKey {
                node: *node,
                stamp: *stamp,
            }),
            SenValue::borrowed(event),
        );
    }
    Ok(())
}

fn persist_snapshot(
    write: &mut WriteTxn,
    snapshot: &Snapshot,
    texts: Vec<rho_desk::NodeTextSnapshot>,
) -> Result<(), String> {
    let mut cells = write.open_table(CELLS);
    for cell in &snapshot.cells {
        cells.insert(
            SenValue::owned(CellAddress {
                node: cell.node,
                field: cell.field.clone(),
            }),
            SenValue::borrowed(cell),
        );
    }
    drop(cells);
    let mut verdicts = write.open_table(VERDICTS);
    for (node, stamp, event) in &snapshot.verdicts {
        verdicts.insert(
            SenValue::owned(VerdictKey {
                node: *node,
                stamp: *stamp,
            }),
            SenValue::borrowed(event),
        );
    }
    drop(verdicts);
    let mut table = write.open_table(TEXTS);
    for text in texts {
        table.insert(SenValue::owned(text.node_id), SenValue::owned(text));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use redb::{TableDefinition, TypeName, Value as RedbValue};
    use rho_db::RhoDb;
    use senax_encoder::{Decoder, Encoder};

    use super::*;

    #[derive(Clone, Debug, Encode, Decode)]
    struct V1MigrationReport {
        warnings: Vec<String>,
        page_urls_awaiting_backfill: Vec<NodeId>,
    }

    #[derive(Debug)]
    struct V1MigrationReportValue;

    impl RedbValue for V1MigrationReportValue {
        type SelfType<'a> = SenValue<'a, V1MigrationReport>;
        type AsBytes<'a> = BytesMut;

        fn fixed_width() -> Option<usize> {
            None
        }

        fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
        where
            Self: 'a,
        {
            let mut data = data;
            SenValue::owned(V1MigrationReport::decode(&mut data).unwrap())
        }

        fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
        where
            Self: 'b,
        {
            let mut bytes = BytesMut::new();
            value.as_ref().encode(&mut bytes).unwrap();
            bytes
        }

        fn type_name() -> TypeName {
            TypeName::new("rho-db::Sen<rho_daemon::desk_cells::MigrationReport>")
        }
    }

    async fn fixture_store() -> DeskCellStore {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rho.redb");
        std::fs::write(&path, include_bytes!("../testdata/desk-tree-v1-real.redb")).unwrap();
        let db = RhoDb::open(path);
        // RhoDb owns the open file after the temporary directory handle drops.
        DeskCellStore::new(db, 42).await.unwrap().0
    }

    #[tokio::test]
    async fn frozen_v1_fixture_migrates_once_into_v2_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rho.redb");
        std::fs::write(&path, include_bytes!("../testdata/desk-tree-v1-real.redb")).unwrap();
        let db = RhoDb::open(path);
        let machine_seed = 42;
        let first = {
            let mut write = db.write().await;
            let report = initialize(&mut write, machine_seed).unwrap();
            write.commit();
            report
        };
        assert_eq!(
            first
                .kind_counts
                .iter()
                .map(|(_, count)| count)
                .sum::<u64>(),
            2
        );
        assert_eq!(first.dropped_marks, 0);
        let read = db.read();
        assert!(read.open_table(META).get(&()).is_some());
        assert!(read.open_table(MIGRATED).get(&()).is_some());
        let cells = read
            .open_table(CELLS)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .collect::<Vec<_>>();
        assert!(cells.iter().any(|cell| {
            cell.field == Field::Kind
                && cell.value == rho_desk::cells::Value::Kind(rho_desk::cells::NodeKind::Note)
        }));
        assert!(cells.iter().any(|cell| {
            cell.field == Field::PaceDays && cell.value == rho_desk::cells::Value::Days(3)
        }));
        assert_eq!(
            cells
                .iter()
                .map(|cell| cell.node)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            2
        );
        assert!(cells.iter().all(|cell| {
            cell.field != Field::Deleted || cell.value == rho_desk::cells::Value::Bool(false)
        }));
        assert!(cells.iter().any(|cell| {
            cell.field == Field::Parent
                && matches!(cell.value, rho_desk::cells::Value::Parent(Some(_)))
        }));
        assert_eq!(read.open_table(TEXTS).iter().count(), 2);
        drop(read);
        let second = {
            let mut write = db.write().await;
            let report = initialize(&mut write, machine_seed).unwrap();
            write.commit();
            report
        };
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn migration_splits_a_heading_file_binding_into_a_typed_child() {
        use crate::desk_tree_v1 as v1;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rho.redb");
        std::fs::write(&path, include_bytes!("../testdata/desk-tree-v1-real.redb")).unwrap();
        let db = RhoDb::open(path);
        let mut write = db.write().await;
        let mut legacy = v1::load_replayed(&mut write).unwrap().unwrap();
        drop(write);
        let heading = legacy
            .nodes
            .iter_mut()
            .find(|node| node.kind == v1::NodeKind::Heading)
            .unwrap();
        let heading_id = current_id(heading.id);
        heading.bindings.push((
            v1::BindingKind::File,
            v1::TreeClock {
                value: u32::MAX,
                replica_id: heading.id.replica_id,
            },
            Some(v1::Binding::File(camino::Utf8PathBuf::from("rho"))),
        ));
        let migration_namespace = legacy
            .nodes
            .iter()
            .map(|node| node.id.replica_id)
            .chain(legacy.replicas.iter().map(|replica| replica.replica_id))
            .max()
            .unwrap()
            + 1;

        let converted = migrate_v1(legacy, DeviceId([4; 16]), 42).unwrap();
        let store = Store::from_snapshot(DeviceId([4; 16]), converted.snapshot).unwrap();
        let file_id = NodeId {
            replica_id: migration_namespace,
            counter: 1,
        };
        assert_eq!(
            store.value(heading_id, &Field::Kind),
            Some(&rho_desk::cells::Value::Kind(
                rho_desk::cells::NodeKind::Note
            ))
        );
        assert_eq!(
            store.value(file_id, &Field::Parent),
            Some(&rho_desk::cells::Value::Parent(Some(heading_id)))
        );
        assert_eq!(
            store.value(file_id, &Field::Path),
            Some(&rho_desk::cells::Value::Path(camino::Utf8PathBuf::from(
                "rho"
            )))
        );
        assert_eq!(converted.next_node_namespace, migration_namespace + 1);
    }

    #[tokio::test]
    async fn migration_report_added_fields_default_when_reopening_slice_1a_state() {
        const OLD_MIGRATED: TableDefinition<(), V1MigrationReportValue> =
            TableDefinition::new("rho_desk_tree_migrated_v2");

        let store = fixture_store().await;
        let machine_seed = 42;
        let parent = store.sync_since(&Version::new()).unwrap().cells[0].node;
        store
            .bind_machine(
                Some(parent),
                MachineBinding::Agent {
                    agent_id: AgentId::from_counter(3, &rho_core::AgentIdDomain(machine_seed))
                        .unwrap(),
                    host: machine_seed,
                },
                BindPlacement::Chosen,
            )
            .await
            .unwrap();
        let mut write = store.db.write().await;
        let mut old_meta = load_meta_from_write(&mut write).unwrap();
        let daemon_namespace = daemon_node_namespace(&old_meta).unwrap();
        old_meta.device_node_namespaces.clear();
        old_meta.next_node_namespace = daemon_namespace;
        write
            .open_table(META)
            .insert(&(), SenValue::owned(old_meta));
        let mut host_cell = write
            .open_table(CELLS)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .find(|cell| cell.field == Field::Host)
            .unwrap();
        host_cell.value = rho_desk::cells::Value::Host(0);
        write.open_table(CELLS).insert(
            SenValue::owned(CellAddress {
                node: host_cell.node,
                field: Field::Host,
            }),
            SenValue::owned(host_cell),
        );
        write.open_table(OLD_MIGRATED).insert(
            &(),
            SenValue::owned(V1MigrationReport {
                warnings: vec!["old".into()],
                page_urls_awaiting_backfill: Vec::new(),
            }),
        );
        write.commit();
        let (_, report, migrated_now) = DeskCellStore::new(store.db.clone(), machine_seed)
            .await
            .unwrap();
        assert!(!migrated_now);
        assert_eq!(report.warnings, ["old"]);
        assert!(report.kind_counts.is_empty());
        assert_eq!(report.rooted_by_chain_rule, 0);
        assert_eq!(report.dropped_marks, 0);
        let read = store.db.read();
        let upgraded = read.open_table(META).get(&()).unwrap().value().into_owned();
        assert_eq!(daemon_node_namespace(&upgraded).unwrap(), daemon_namespace);
        assert!(read.open_table(CELLS).iter().any(|(_, value)| {
            value.value().as_ref().field == Field::Host
                && value.value().as_ref().value == rho_desk::cells::Value::Host(machine_seed)
        }));
    }

    #[tokio::test]
    async fn mutations_are_connection_bound_idempotent_and_sync_since_frontiers() {
        use rho_desk::cells::{CellMutation, CellWrite, Value};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rho.redb");
        std::fs::write(&path, include_bytes!("../testdata/desk-tree-v1-real.redb")).unwrap();
        let db = RhoDb::open(path);
        let (store, _, _) = DeskCellStore::new(db, 42).await.unwrap();
        let initial = store.sync_since(&Version::new()).unwrap();
        let node = initial.cells[0].node;
        let device = DeviceId([9; 16]);
        let namespace = store.node_namespace(device).await.unwrap();
        let version = initial.version.values().copied().max().unwrap_or(0) + 1;
        let mutation = CellMutation {
            stamp: Stamp { device, version },
            writes: vec![CellWrite {
                node,
                field: Field::Tag("synced".into()),
                value: Value::Bool(true),
            }],
            verdict: None,
        };
        store
            .apply_mutation(device, namespace, mutation.clone())
            .await
            .unwrap();
        store
            .apply_mutation(device, namespace, mutation.clone())
            .await
            .unwrap();
        let delta = store.sync_since(&initial.version).unwrap();
        assert_eq!(delta.cells.len(), 1);
        assert_eq!(delta.version.get(&device), Some(&version));
        let mut conflict = mutation.clone();
        conflict.writes[0].value = Value::Bool(false);
        assert!(
            store
                .apply_mutation(device, namespace, conflict)
                .await
                .is_err()
        );
        assert!(
            store
                .apply_mutation(DeviceId([8; 16]), namespace, mutation)
                .await
                .is_err()
        );
        let offline_device = DeviceId([7; 16]);
        let offline_namespace = store.node_namespace(offline_device).await.unwrap();
        store
            .apply_mutation(
                offline_device,
                offline_namespace,
                CellMutation {
                    stamp: Stamp {
                        device: offline_device,
                        version: 1,
                    },
                    writes: vec![CellWrite {
                        node,
                        field: Field::Tag("offline".into()),
                        value: Value::Bool(true),
                    }],
                    verdict: None,
                },
            )
            .await
            .unwrap();
        let global = store.frontier().unwrap().values().copied().max().unwrap();
        assert!(
            store
                .apply_mutation(
                    offline_device,
                    offline_namespace,
                    CellMutation {
                        stamp: Stamp {
                            device: offline_device,
                            version: global + 2,
                        },
                        writes: vec![CellWrite {
                            node,
                            field: Field::Tag("jump".into()),
                            value: Value::Bool(true),
                        }],
                        verdict: None,
                    },
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn new_notes_must_use_the_bound_namespace_and_complete_shape() {
        use rho_desk::cells::{CellWrite, NodeKind, State, Timestamp, TimestampPrecision, Value};

        let store = fixture_store().await;
        let initial = store.sync_since(&Version::new()).unwrap();
        let device = DeviceId([10; 16]);
        let namespace = store.node_namespace(device).await.unwrap();
        let version = initial.version.values().copied().max().unwrap_or(0) + 1;
        let node = NodeId {
            replica_id: namespace + 1,
            counter: 1,
        };
        let writes = vec![
            CellWrite {
                node,
                field: Field::Kind,
                value: Value::Kind(NodeKind::Note),
            },
            CellWrite {
                node,
                field: Field::Parent,
                value: Value::Parent(None),
            },
            CellWrite {
                node,
                field: Field::Deleted,
                value: Value::Bool(false),
            },
            CellWrite {
                node,
                field: Field::CreatedAt,
                value: Value::Timestamp(Timestamp {
                    unix_ms: 1,
                    precision: TimestampPrecision::Millisecond,
                }),
            },
            CellWrite {
                node,
                field: Field::State,
                value: Value::State(State::Open),
            },
            CellWrite {
                node,
                field: Field::DeferUntil,
                value: Value::OptionalTimestamp(None),
            },
            CellWrite {
                node,
                field: Field::Deadline,
                value: Value::OptionalTimestamp(None),
            },
            CellWrite {
                node,
                field: Field::PaceDays,
                value: Value::Days(0),
            },
        ];
        let mutation = CellMutation {
            stamp: Stamp { device, version },
            writes,
            verdict: None,
        };
        assert!(
            store
                .apply_mutation(device, namespace, mutation.clone())
                .await
                .is_err()
        );
        let mut valid = mutation;
        for write in &mut valid.writes {
            write.node.replica_id = namespace;
        }
        store
            .apply_mutation(device, namespace, valid.clone())
            .await
            .unwrap();
        let target = valid.writes[0].node;
        let note = NodeId {
            replica_id: namespace,
            counter: 2,
        };
        let mut todo_writes = valid.writes;
        let todo_at = Timestamp {
            unix_ms: 2,
            precision: TimestampPrecision::Millisecond,
        };
        for write in &mut todo_writes {
            write.node = note;
            if write.field == Field::Parent {
                write.value = Value::Parent(Some(target));
            } else if write.field == Field::DeferUntil {
                write.value = Value::OptionalTimestamp(Some(todo_at));
            } else if write.field == Field::PaceDays {
                write.value = Value::Days(7);
            }
        }
        todo_writes.push(CellWrite {
            node: target,
            field: Field::State,
            value: Value::State(State::Done),
        });
        let applied_stamp = Stamp {
            device,
            version: version + 1,
        };
        store
            .apply_mutation(
                device,
                namespace,
                CellMutation {
                    stamp: applied_stamp,
                    writes: todo_writes,
                    verdict: Some((
                        target,
                        rho_desk::cells::VerdictEvent::Applied {
                            verdict: rho_desk::cells::Verdict::Todo { note },
                            at: applied_stamp,
                            changes: vec![
                                rho_desk::cells::FieldChange {
                                    node: target,
                                    field: Field::State,
                                    before: Some(Value::State(State::Open)),
                                    after: Some(Value::State(State::Done)),
                                },
                                rho_desk::cells::FieldChange {
                                    node: note,
                                    field: Field::Deleted,
                                    before: Some(Value::Bool(true)),
                                    after: Some(Value::Bool(false)),
                                },
                                rho_desk::cells::FieldChange {
                                    node: note,
                                    field: Field::DeferUntil,
                                    before: Some(Value::OptionalTimestamp(None)),
                                    after: Some(Value::OptionalTimestamp(Some(todo_at))),
                                },
                                rho_desk::cells::FieldChange {
                                    node: note,
                                    field: Field::PaceDays,
                                    before: Some(Value::Days(0)),
                                    after: Some(Value::Days(7)),
                                },
                            ],
                        },
                    )),
                },
            )
            .await
            .unwrap();
        store
            .apply_mutation(
                device,
                namespace,
                CellMutation {
                    stamp: Stamp {
                        device,
                        version: version + 2,
                    },
                    writes: vec![
                        CellWrite {
                            node: note,
                            field: Field::Deleted,
                            value: Value::Bool(true),
                        },
                        CellWrite {
                            node: note,
                            field: Field::DeferUntil,
                            value: Value::OptionalTimestamp(None),
                        },
                        CellWrite {
                            node: note,
                            field: Field::PaceDays,
                            value: Value::Days(0),
                        },
                        CellWrite {
                            node: target,
                            field: Field::State,
                            value: Value::State(State::Open),
                        },
                    ],
                    verdict: Some((
                        target,
                        rho_desk::cells::VerdictEvent::Undone { of: applied_stamp },
                    )),
                },
            )
            .await
            .unwrap();
        let snapshot = store.sync_since(&Version::new()).unwrap();
        assert!(
            !Store::from_snapshot(DeviceId([0; 16]), snapshot)
                .unwrap()
                .materialize()
                .iter()
                .any(|node| node.id == note)
        );
    }

    #[tokio::test]
    async fn daemon_machine_bindings_are_complete_idempotent_and_removable() {
        use rho_desk::cells::{
            CellWrite, FieldChange, NodeKind, State, Value, Verdict, VerdictEvent,
        };

        let store = fixture_store().await;
        let parent = store.sync_since(&Version::new()).unwrap().cells[0].node;
        let agent_id = AgentId::from_counter(7, &rho_core::AgentIdDomain(42)).unwrap();
        let binding = MachineBinding::Agent {
            agent_id: agent_id.clone(),
            host: 42,
        };
        let (node, created) = store
            .bind_machine(Some(parent), binding.clone(), BindPlacement::Chosen)
            .await
            .unwrap();
        assert!(created);
        assert_eq!(
            store
                .bind_machine(Some(parent), binding.clone(), BindPlacement::Chosen)
                .await
                .unwrap(),
            (node, false)
        );
        let meta = store.sync_since(&Version::new()).unwrap();
        let materialized = Store::from_snapshot(DeviceId([0; 16]), meta)
            .unwrap()
            .materialize();
        let agent = materialized
            .iter()
            .find(|node| node.kind == NodeKind::Agent)
            .unwrap();
        assert_eq!(agent.parent, Some(parent));
        assert_eq!(
            agent.fields.get(&Field::AgentId),
            Some(&Value::AgentId(agent_id))
        );
        assert_eq!(agent.fields.get(&Field::Host), Some(&Value::Host(42)));
        let agent_node = agent.id;
        let device = DeviceId([12; 16]);
        let namespace = store.node_namespace(device).await.unwrap();
        let version = store.frontier().unwrap().values().copied().max().unwrap() + 1;
        let stamp = Stamp { device, version };
        assert!(
            store
                .apply_mutation(
                    device,
                    namespace,
                    CellMutation {
                        stamp,
                        writes: vec![CellWrite {
                            node: agent_node,
                            field: Field::Parent,
                            value: Value::Parent(None),
                        }],
                        verdict: Some((
                            agent_node,
                            VerdictEvent::Applied {
                                verdict: Verdict::Done,
                                at: stamp,
                                changes: vec![FieldChange {
                                    node: agent_node,
                                    field: Field::Parent,
                                    before: Some(Value::Parent(Some(parent))),
                                    after: Some(Value::Parent(None)),
                                }],
                            },
                        )),
                    },
                )
                .await
                .is_err()
        );
        store
            .apply_mutation(
                device,
                namespace,
                CellMutation {
                    stamp,
                    writes: vec![CellWrite {
                        node: agent_node,
                        field: Field::State,
                        value: Value::State(State::Done),
                    }],
                    verdict: Some((
                        agent_node,
                        VerdictEvent::Applied {
                            verdict: Verdict::Done,
                            at: stamp,
                            changes: vec![FieldChange {
                                node: agent_node,
                                field: Field::State,
                                before: Some(Value::State(State::Open)),
                                after: Some(Value::State(State::Done)),
                            }],
                        },
                    )),
                },
            )
            .await
            .unwrap();
        assert!(
            store
                .apply_mutation(
                    device,
                    namespace,
                    CellMutation {
                        stamp: Stamp {
                            device,
                            version: version + 1,
                        },
                        writes: vec![CellWrite {
                            node: agent_node,
                            field: Field::State,
                            value: Value::State(State::Open),
                        }],
                        verdict: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(store.unbind_machine(&binding).await.unwrap());
        assert!(!store.unbind_machine(&binding).await.unwrap());
    }

    #[tokio::test]
    async fn note_text_is_namespace_bound_and_exact_retries_are_idempotent() {
        let store = fixture_store().await;
        let text = store.texts().into_iter().next().unwrap();
        let device = DeviceId([11; 16]);
        let namespace = store.node_namespace(device).await.unwrap();
        let before = store.texts();
        for ranges in [vec![(0, 1)], vec![(1, 0)]] {
            let malformed = rho_desk::TextOperation::Edit {
                timestamp: rho_desk::TreeClock {
                    value: 1,
                    replica_id: namespace,
                },
                version: Vec::new(),
                ranges,
                new_text: vec!["x".into()],
            };
            assert!(
                store
                    .apply_text(namespace, text.node_id, malformed, None)
                    .await
                    .is_err()
            );
            assert_eq!(store.texts(), before);
        }
        let mut buffer = text
            .buffer(namespace, text::BufferId::new(1).unwrap())
            .unwrap();
        let end = buffer.len();
        let operation = rho_desk::TextOperation::from_text(&buffer.edit([(end..end, "!")]));
        assert!(
            store
                .apply_text(namespace, text.node_id, operation.clone(), None)
                .await
                .unwrap()
        );
        assert!(
            !store
                .apply_text(namespace, text.node_id, operation.clone(), None)
                .await
                .unwrap()
        );
        let stored = store
            .texts()
            .into_iter()
            .find(|candidate| candidate.node_id == text.node_id)
            .unwrap();
        let mut utf_buffer = stored
            .buffer(namespace, text::BufferId::new(2).unwrap())
            .unwrap();
        let utf_end = utf_buffer.len();
        let utf_operation =
            rho_desk::TextOperation::from_text(&utf_buffer.edit([(utf_end..utf_end, "é")]));
        store
            .apply_text(namespace, text.node_id, utf_operation, None)
            .await
            .unwrap();
        let stored = store
            .texts()
            .into_iter()
            .find(|candidate| candidate.node_id == text.node_id)
            .unwrap();
        let mut utf_buffer = stored
            .buffer(namespace, text::BufferId::new(3).unwrap())
            .unwrap();
        let utf_end = utf_buffer.len();
        let mut split_character =
            rho_desk::TextOperation::from_text(&utf_buffer.edit([(utf_end..utf_end, "x")]));
        if let rho_desk::TextOperation::Edit { ranges, .. } = &mut split_character {
            let end = ranges[0].0;
            ranges[0] = (end - 1, end - 1);
        }
        let before_split = store.texts();
        assert!(
            store
                .apply_text(namespace, text.node_id, split_character, None)
                .await
                .is_err()
        );
        assert_eq!(store.texts(), before_split);
        let mut stale = operation.clone();
        match &mut stale {
            rho_desk::TextOperation::Edit { timestamp, .. }
            | rho_desk::TextOperation::Undo { timestamp, .. } => {
                timestamp.value = timestamp.value.saturating_sub(1);
            }
        }
        assert!(
            store
                .apply_text(namespace, text.node_id, stale, None)
                .await
                .is_err()
        );
        assert!(
            store
                .apply_text(namespace + 1, text.node_id, operation, None)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn a_todo_verdict_on_a_thread_node_creates_its_note() {
        use rho_desk::cells::{
            CellWrite, FieldChange, NodeKind, State, Timestamp, TimestampPrecision, Value, Verdict,
            VerdictEvent,
        };

        let store = fixture_store().await;
        let (thread, _) = store
            .bind_machine(
                None,
                MachineBinding::Thread {
                    workspace: "acme".into(),
                    channel: "C1".into(),
                    thread_ts: "1.0".into(),
                },
                BindPlacement::Default,
            )
            .await
            .unwrap();
        let device = DeviceId([12; 16]);
        let namespace = store.node_namespace(device).await.unwrap();
        let version = store.frontier().unwrap().values().copied().max().unwrap() + 1;
        let stamp = Stamp { device, version };
        let note = NodeId {
            replica_id: namespace,
            counter: 4242,
        };
        let at = Timestamp {
            unix_ms: 5,
            precision: TimestampPrecision::Millisecond,
        };
        let write = |field, value| CellWrite {
            node: note,
            field,
            value,
        };
        let result = store
            .apply_mutation(
                device,
                namespace,
                CellMutation {
                    stamp,
                    writes: vec![
                        CellWrite {
                            node: thread,
                            field: Field::State,
                            value: Value::State(State::Done),
                        },
                        write(Field::Kind, Value::Kind(NodeKind::Note)),
                        write(Field::Parent, Value::Parent(Some(thread))),
                        write(Field::Deleted, Value::Bool(false)),
                        write(Field::CreatedAt, Value::Timestamp(at)),
                        write(Field::State, Value::State(State::Open)),
                        write(Field::DeferUntil, Value::OptionalTimestamp(Some(at))),
                        write(Field::Deadline, Value::OptionalTimestamp(None)),
                        write(Field::PaceDays, Value::Days(7)),
                    ],
                    verdict: Some((
                        thread,
                        VerdictEvent::Applied {
                            verdict: Verdict::Todo { note },
                            at: stamp,
                            changes: vec![
                                FieldChange {
                                    node: thread,
                                    field: Field::State,
                                    before: Some(Value::State(State::Open)),
                                    after: Some(Value::State(State::Done)),
                                },
                                FieldChange {
                                    node: note,
                                    field: Field::Deleted,
                                    before: Some(Value::Bool(true)),
                                    after: Some(Value::Bool(false)),
                                },
                                FieldChange {
                                    node: note,
                                    field: Field::DeferUntil,
                                    before: Some(Value::OptionalTimestamp(None)),
                                    after: Some(Value::OptionalTimestamp(Some(at))),
                                },
                                FieldChange {
                                    node: note,
                                    field: Field::PaceDays,
                                    before: Some(Value::Days(0)),
                                    after: Some(Value::Days(7)),
                                },
                            ],
                        },
                    )),
                },
            )
            .await;
        assert_eq!(result, Ok(()));
    }
}
