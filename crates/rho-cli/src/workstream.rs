//! `rho workstream` (alias `ws`): inspect and edit workstreams from a
//! terminal, over the same UI socket the GUI speaks.
//!
//! Every invocation snapshots the daemon first (`Subscribe` → `Ready`) so
//! names and short agent ids resolve exactly like they display, then sends
//! the mutation and waits for the daemon's refreshed `Ready` (success) or
//! `Error` (failure) on this connection.

use std::io::{Read as _, Write as _};

use anyhow::{Context as _, bail};
use rho_ui_proto::{
    AgentId, AgentIdDomain, ClientMessage, ServerMessage, UiAgentSummary, UiAttention,
    UiWorkstream, WorkstreamId, WorkstreamTarget, client::Client,
};

use crate::{WorkstreamArgs, WorkstreamCommand, default_socket_path};

pub(crate) async fn run(args: WorkstreamArgs) -> anyhow::Result<()> {
    let socket_path = match args.socket_path {
        Some(path) => path,
        None => default_socket_path()?,
    };
    let mut client = Client::connect(&socket_path)
        .await
        .context("connect to rho daemon")?;
    client.send(&ClientMessage::Subscribe).await?;
    let snapshot = loop {
        if let ServerMessage::Ready {
            workstreams,
            agents,
            view_config,
            machine_seed,
            agent_counter,
            ..
        } = client.recv().await?
        {
            break Snapshot {
                workstreams,
                agents,
                view_config,
                machine_seed,
                agent_counter,
            };
        }
    };

    let request = match args.command {
        WorkstreamCommand::List => {
            for workstream in &snapshot.workstreams {
                println!("{}", snapshot.stream_line(workstream));
            }
            return Ok(());
        }
        WorkstreamCommand::Show { workstream } => {
            let workstream_id = snapshot.resolve_stream(&workstream)?;
            let workstream = snapshot
                .workstreams
                .iter()
                .find(|entry| entry.workstream_id == workstream_id)
                .expect("resolved id comes from the snapshot");
            println!("{}", snapshot.stream_line(workstream));
            for agent in snapshot.members(workstream_id) {
                println!("  {}", snapshot.agent_line(agent));
            }
            return Ok(());
        }
        WorkstreamCommand::Rename { workstream, name } => ClientMessage::WorkstreamRename {
            workstream_id: snapshot.resolve_stream(&workstream)?,
            name,
        },
        WorkstreamCommand::Label { workstream, label } => ClientMessage::WorkstreamLabel {
            workstream_id: snapshot.resolve_stream(&workstream)?,
            label,
            add: true,
        },
        WorkstreamCommand::Unlabel { workstream, label } => ClientMessage::WorkstreamLabel {
            workstream_id: snapshot.resolve_stream(&workstream)?,
            label,
            add: false,
        },
        WorkstreamCommand::Move { agent, workstream } => ClientMessage::AgentMove {
            agent_id: snapshot.resolve_agent(&agent)?,
            // An unknown name is a creation target, so "spin off a new
            // workstream around this agent" is the same gesture as moving.
            target: match snapshot.resolve_stream(&workstream) {
                Ok(workstream_id) => WorkstreamTarget::Existing(workstream_id),
                Err(_) => WorkstreamTarget::Named(workstream),
            },
        },
        WorkstreamCommand::ViewGet => {
            std::io::stdout().write_all(&snapshot.view_config)?;
            return Ok(());
        }
        WorkstreamCommand::ViewSet => {
            let mut data = Vec::new();
            std::io::stdin().read_to_end(&mut data)?;
            ClientMessage::ViewConfigSet { data }
        }
    };

    // `ViewConfigSet` is fire-and-forget (`Refresh::None`); every other
    // mutation refreshes this connection with a fresh `Ready` on success.
    let await_ready = !matches!(request, ClientMessage::ViewConfigSet { .. });
    client.send(&request).await?;
    if await_ready {
        loop {
            match client.recv().await? {
                ServerMessage::Ready { .. } => return Ok(()),
                ServerMessage::Error { message } => bail!("{message}"),
                _ => {}
            }
        }
    }
    Ok(())
}

struct Snapshot {
    workstreams: Vec<UiWorkstream>,
    agents: Vec<UiAgentSummary>,
    view_config: Vec<u8>,
    machine_seed: u64,
    agent_counter: u64,
}

impl Snapshot {
    fn members(&self, workstream_id: WorkstreamId) -> impl Iterator<Item = &UiAgentSummary> {
        self.agents
            .iter()
            .filter(move |agent| agent.workstream == workstream_id)
    }

    fn stream_line(&self, workstream: &UiWorkstream) -> String {
        let members = self.members(workstream.workstream_id).count();
        let attention = self
            .members(workstream.workstream_id)
            .map(|agent| agent.attention)
            .max()
            .unwrap_or(UiAttention::Quiet);
        let mut line = format!(
            "ws-{}  {}  ({members} agent{}, {})",
            workstream.workstream_id.0,
            workstream.name,
            if members == 1 { "" } else { "s" },
            attention_name(attention),
        );
        if !workstream.labels.is_empty() {
            line.push_str("  [");
            line.push_str(&workstream.labels.join(", "));
            line.push(']');
        }
        line
    }

    fn agent_line(&self, agent: &UiAgentSummary) -> String {
        let mut line = self.agent_label(agent);
        if let Some(name) = agent.display_name.as_deref().filter(|name| !name.is_empty()) {
            line.push_str("  ");
            line.push_str(name);
        }
        line.push_str("  (");
        line.push_str(attention_name(agent.attention));
        line.push(')');
        if !agent.labels.is_empty() {
            line.push_str("  [");
            line.push_str(&agent.labels.join(", "));
            line.push(']');
        }
        line
    }

    /// The role-prefixed short id, matching what the GUI and skills display.
    fn agent_label(&self, agent: &UiAgentSummary) -> String {
        let prefix_len = prefix_id::uniform_prefix_len(self.agent_counter, 100).max(4);
        format!(
            "{}-{}",
            agent.role.handle_prefix(),
            &agent.agent_id.encoded()[..prefix_len]
        )
    }

    /// Accepts `ws-<n>`, a bare id number, or a workstream's exact name.
    fn resolve_stream(&self, text: &str) -> anyhow::Result<WorkstreamId> {
        let text = text.trim();
        let by_id = text
            .strip_prefix("ws-")
            .unwrap_or(text)
            .parse::<u64>()
            .ok()
            .map(WorkstreamId);
        if let Some(workstream_id) = by_id
            && self
                .workstreams
                .iter()
                .any(|workstream| workstream.workstream_id == workstream_id)
        {
            return Ok(workstream_id);
        }
        let named = self
            .workstreams
            .iter()
            .filter(|workstream| workstream.name == text)
            .collect::<Vec<_>>();
        match named.as_slice() {
            [workstream] => Ok(workstream.workstream_id),
            [] => bail!("no workstream named `{text}`"),
            _ => bail!("multiple workstreams named `{text}`; use ws-<id>"),
        }
    }

    /// Accepts a role-prefixed handle (`eng-16lh`) or a bare id prefix.
    fn resolve_agent(&self, text: &str) -> anyhow::Result<AgentId> {
        let raw = match text.trim().split_once('-') {
            Some((_, raw)) => raw,
            None => text.trim(),
        };
        let domain = AgentIdDomain(self.machine_seed);
        match AgentId::from_prefix(raw, self.agent_counter + 1, &domain)? {
            prefix_id::PrefixResolution::Unique(agent_id) => Ok(agent_id),
            prefix_id::PrefixResolution::Ambiguous { .. } => {
                bail!("agent id `{text}` is ambiguous; give more characters")
            }
            prefix_id::PrefixResolution::NotFound => bail!("no agent with id `{text}`"),
        }
    }
}

fn attention_name(attention: UiAttention) -> &'static str {
    match attention {
        UiAttention::Quiet => "quiet",
        UiAttention::Working => "working",
        UiAttention::Pending => "pending",
        UiAttention::NeedsInput => "needs input",
    }
}
