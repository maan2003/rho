//! File-based Desk editing: checkout the document to a file, edit it with
//! ordinary tools, and apply the result back as a minimal CRDT edit.
//!
//! `apply` diffs the edited file against the checkout-time snapshot and
//! generates one native text operation from that fork point, so untouched
//! text keeps its CRDT identity and edits made by others in the meantime
//! merge instead of being reverted.

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use imara_diff::{Algorithm, Diff, InternedInput};
use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};
use rho_ui_proto::{ClientMessage, ServerMessage};
use senax_encoder::{Decode, Encode};

use crate::connect_or_start_daemon;

#[derive(Clone, clap::Args)]
pub(crate) struct DeskArgs {
    #[arg(long = "socket-path")]
    socket_path: Option<PathBuf>,
    #[command(subcommand)]
    command: DeskCommand,
}

#[derive(Clone, clap::Subcommand)]
enum DeskCommand {
    /// Print the current Desk document.
    Cat,
    /// Write the Desk document to FILE, plus a FILE.base sidecar recording
    /// the snapshot that `apply` will diff against.
    Checkout {
        file: PathBuf,
        /// Attribute the edits to this agent handle (defaults to
        /// $RHO_AGENT_ID when set, so agents self-attribute).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Diff FILE against its checkout base and send the change to the daemon
    /// as one CRDT edit.
    Apply { file: PathBuf },
}

/// The fork point recorded at checkout: `apply` edits this exact state, and
/// the daemon merges the resulting operation with anything newer.
#[derive(Encode, Decode)]
struct CheckoutBase {
    snapshot: DeskSnapshot,
    replica_id: u16,
}

pub(crate) async fn run(args: DeskArgs) -> Result<()> {
    let socket_path = match args.socket_path {
        Some(path) => path,
        None => rho_daemon::default_socket_path()?,
    };
    match args.command {
        DeskCommand::Cat => cat(&socket_path).await,
        DeskCommand::Checkout { file, agent } => checkout(&socket_path, &file, agent).await,
        DeskCommand::Apply { file } => apply(&socket_path, &file).await,
    }
}

async fn cat(socket_path: &Path) -> Result<()> {
    let mut client = connect_or_start_daemon(socket_path).await?;
    client.send(&ClientMessage::DeskGet).await?;
    loop {
        match client.recv().await? {
            ServerMessage::DeskDocument { text } => {
                print!("{text}");
                return Ok(());
            }
            ServerMessage::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
}

async fn checkout(socket_path: &Path, file: &Path, agent: Option<String>) -> Result<()> {
    let agent = agent.or_else(|| std::env::var("RHO_AGENT_ID").ok());
    let mut client = connect_or_start_daemon(socket_path).await?;
    let subscribe = match &agent {
        Some(handle) => {
            // The daemon classifies a stream by its first client frame and
            // greets with Ready only after it arrives, so the client must
            // speak before waiting. Ready then carries the id domain needed
            // to resolve the handle into a full agent id.
            client.send(&ClientMessage::Ping).await?;
            let (machine_seed, agent_counter) = loop {
                match client.recv().await? {
                    ServerMessage::Ready {
                        machine_seed,
                        agent_counter,
                        ..
                    } => break (machine_seed, agent_counter),
                    ServerMessage::Error { message } => bail!("{message}"),
                    _ => {}
                }
            };
            let agent_id = crate::resolve_agent_id(handle, machine_seed, agent_counter)?;
            ClientMessage::DeskSubscribeAgent { agent_id }
        }
        None => ClientMessage::DeskSubscribe,
    };
    client.send(&subscribe).await?;
    loop {
        match client.recv().await? {
            ServerMessage::DeskSnapshot {
                snapshot,
                replica_id,
            } => {
                let text = snapshot.document_text().map_err(anyhow::Error::msg)?;
                std::fs::write(file, &text).with_context(|| format!("write {}", file.display()))?;
                let base_path = base_path(file);
                write_base(
                    &base_path,
                    &CheckoutBase {
                        snapshot,
                        replica_id,
                    },
                )?;
                println!(
                    "checked out the Desk to {} (base: {})",
                    file.display(),
                    base_path.display()
                );
                println!("edit the file, then run: rho desk apply {}", file.display());
                return Ok(());
            }
            ServerMessage::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
}

async fn apply(socket_path: &Path, file: &Path) -> Result<()> {
    let new_text =
        std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let base_path = base_path(file);
    let mut base = read_base(&base_path)?;
    let base_text = base.snapshot.document_text().map_err(anyhow::Error::msg)?;
    let edits = diff_edits(&base_text, &new_text);
    if edits.is_empty() {
        println!("no changes");
        return Ok(());
    }
    let mut buffer = base
        .snapshot
        .buffer(base.replica_id)
        .map_err(anyhow::Error::msg)?;
    let operation = DeskOperation::from_text(&buffer.edit(edits));
    let mut client = connect_or_start_daemon(socket_path).await?;
    client
        .send(&ClientMessage::DeskTextApply {
            operation: operation.clone(),
            transaction: None,
        })
        .await?;
    loop {
        match client.recv().await? {
            ServerMessage::DeskTextApplied { record }
                if record.operation.timestamp() == operation.timestamp() =>
            {
                // The sidecar advances to include this edit, so further
                // edit/apply rounds fork from the state just applied.
                base.snapshot.operations.push(operation);
                base.snapshot.text = new_text;
                write_base(&base_path, &base)?;
                println!("applied desk edit (sequence {})", record.sequence);
                return Ok(());
            }
            ServerMessage::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
}

fn base_path(file: &Path) -> PathBuf {
    let mut path = file.as_os_str().to_owned();
    path.push(".base");
    PathBuf::from(path)
}

fn write_base(path: &Path, base: &CheckoutBase) -> Result<()> {
    let bytes = senax_encoder::encode(base).map_err(|error| anyhow::anyhow!("{error:?}"))?;
    std::fs::write(path, &bytes).with_context(|| format!("write {}", path.display()))
}

fn read_base(path: &Path) -> Result<CheckoutBase> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "read {} (run `rho desk checkout` before `apply`)",
            path.display()
        )
    })?;
    senax_encoder::decode(&mut bytes.as_slice()).map_err(|error| anyhow::anyhow!("{error:?}"))
}

/// Minimal edits turning `old` into `new`: a line diff, with each hunk
/// tightened to its changed characters so untouched text keeps its CRDT
/// identity (anchors, cursors, and concurrent edits survive).
fn diff_edits(old: &str, new: &str) -> Vec<(Range<usize>, String)> {
    let mut input: InternedInput<&str> = InternedInput::default();
    input.update_before(old.split_inclusive('\n'));
    input.update_after(new.split_inclusive('\n'));
    let old_offsets = line_offsets(old);
    let new_offsets = line_offsets(new);
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut edits = Vec::new();
    for hunk in diff.hunks() {
        let old_range =
            old_offsets[hunk.before.start as usize]..old_offsets[hunk.before.end as usize];
        let new_range =
            new_offsets[hunk.after.start as usize]..new_offsets[hunk.after.end as usize];
        let (old_range, replacement) = tighten(old, new, old_range, new_range);
        if old_range.is_empty() && replacement.is_empty() {
            continue;
        }
        edits.push((old_range, replacement.to_owned()));
    }
    edits
}

/// Byte offsets of each line start, ending with the text's length.
fn line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    offsets.extend(text.split_inclusive('\n').scan(0, |offset, line| {
        *offset += line.len();
        Some(*offset)
    }));
    offsets
}

/// Trims the common prefix and suffix off a replacement hunk.
fn tighten<'a>(
    old: &str,
    new: &'a str,
    mut old_range: Range<usize>,
    mut new_range: Range<usize>,
) -> (Range<usize>, &'a str) {
    let old_slice = &old[old_range.clone()];
    let new_slice = &new[new_range.clone()];
    let prefix = old_slice
        .char_indices()
        .zip(new_slice.chars())
        .take_while(|((_, old_char), new_char)| old_char == new_char)
        .last()
        .map(|((index, character), _)| index + character.len_utf8())
        .unwrap_or(0);
    let suffix: usize = old_slice[prefix..]
        .chars()
        .rev()
        .zip(new_slice[prefix..].chars().rev())
        .take_while(|(old_char, new_char)| old_char == new_char)
        .map(|(character, _)| character.len_utf8())
        .sum();
    old_range.start += prefix;
    old_range.end -= suffix;
    new_range.start += prefix;
    new_range.end -= suffix;
    (old_range, &new[new_range])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_edits(old: &str, edits: &[(Range<usize>, String)]) -> String {
        let mut result = old.to_owned();
        for (range, replacement) in edits.iter().rev() {
            result.replace_range(range.clone(), replacement);
        }
        result
    }

    #[track_caller]
    fn roundtrip(old: &str, new: &str) -> Vec<(Range<usize>, String)> {
        let edits = diff_edits(old, new);
        assert_eq!(apply_edits(old, &edits), new, "{old:?} -> {new:?}");
        for window in edits.windows(2) {
            assert!(window[0].0.end <= window[1].0.start, "edits out of order");
        }
        edits
    }

    #[test]
    fn diffs_reconstruct_the_new_text() {
        roundtrip("", "* One\n* Two\n");
        roundtrip("* One\n* Two\n", "");
        roundtrip("* One\nbody\n* Two\n", "* One\nbody\nmore\n* Two\n");
        roundtrip("* One\n* Two\n* Three\n", "* One\n* Three\n");
        roundtrip("* Onè ✸\n", "* Onè ✿ now\n");
        roundtrip("no trailing newline", "no trailing newline, still");
        roundtrip("a\nb\nc\nd\n", "d\nc\nb\na\n");
    }

    #[test]
    fn touched_lines_keep_their_unchanged_characters() {
        // A one-word change must not replace the whole line: the line's
        // other characters keep their CRDT identity.
        let edits = roundtrip("* fix parser DONE :eng-x:\n", "* fix lexer DONE :eng-x:\n");
        assert_eq!(edits, vec![(6..10, "lex".to_owned())]);
        // An appended heading is a pure insertion.
        let edits = roundtrip("* One\n", "* One\n* Two :eng-y:\n");
        assert_eq!(edits, vec![(6..6, "* Two :eng-y:\n".to_owned())]);
    }

    #[test]
    fn apply_merges_with_edits_made_after_the_checkout() {
        let mut server =
            text::Buffer::new(text::ReplicaId::new(1), text::BufferId::new(1).unwrap(), "");
        let history = vec![DeskOperation::from_text(
            &server.edit([(0..0, "* One\nbody\n")]),
        )];
        let base = CheckoutBase {
            snapshot: DeskSnapshot {
                text: server.text(),
                operations: history,
                transactions: Vec::new(),
                replicas: Vec::new(),
            },
            replica_id: 5,
        };

        // Someone keeps editing after the checkout...
        let end = server.text().len();
        server.edit([(end..end, "* Two\n")]);

        // ...while the agent edits its stale file and applies.
        let base_text = base.snapshot.document_text().unwrap();
        let new_text = "* One\nbody edited\n";
        let mut buffer = base.snapshot.buffer(base.replica_id).unwrap();
        let operation = buffer.edit(diff_edits(&base_text, new_text));

        // The concurrent edit merges instead of being reverted.
        server.apply_ops([operation]);
        assert_eq!(server.text(), "* One\nbody edited\n* Two\n");
    }

    #[test]
    fn edits_replay_through_a_text_buffer() {
        let old = "* rho\n** Desk rework\nbody line\n* Archive\n";
        let new = "* rho\n** Desk rework DONE\nbody line\n** migrated task :eng-old1:\n* Archive\n";
        let mut buffer = text::Buffer::new(
            text::ReplicaId::new(7),
            text::BufferId::new(1).unwrap(),
            old,
        );
        buffer.edit(diff_edits(old, new));
        assert_eq!(buffer.text(), new);
    }
}
