//! Feeding the fake Slack from a copy of the user's own mirror.
//!
//! The fake's own seed is a fixture: five people, four conversations, every
//! rendering construct the UX checklist names, once. It is the right thing to
//! test rendering against and the wrong thing to test a client against, because
//! the client's problem is the flood — hundreds of conversations, thousands of
//! messages, threads the user replied to from a phone. That state exists
//! already, in the Slack tables of the user's `rho-client.redb`, and a
//! snapshot carries it.
//!
//! So this reads a mirror and hands the fake what it holds: the roster, the
//! conversations with their kinds, the history of each, the threads under it,
//! and Slack's own read cursor. Nothing in `fake.rs` changes — the loader lives
//! here and uses the same `add_*` calls the fixture does. What the mirror
//! cannot supply, the fake keeps: the fake signs rho in as `ME`, so the user's
//! own id is remapped onto that everywhere it appears, and "me" stays "me".
//!
//! The mirror handed in is copied before it is opened. The rig's GUI has the
//! same file open — it is that GUI's mirror — and two processes cannot open one
//! redb.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::Args;
use rho_slack::fake::Fake;
use rho_slack::mirror::{Mirror, Scope};
use rho_slack::types::{Conversation, ConversationKind, Message, User};
use serde_json::{Value, json};

/// The id the fake signs rho in as. Everything the mirror says about the user
/// is rewritten onto it.
const SELF_ID: &str = "ME";

#[derive(Args)]
pub struct FakeSlackArgs {
    /// The mirror to feed from. Copied before it is opened; the file named
    /// here is never opened, and never written.
    #[arg(long)]
    mirror: PathBuf,

    /// Which workspace in the mirror. Defaults to the only one, and lists
    /// them when there is more than one.
    #[arg(long)]
    workspace: Option<String>,

    /// How many of the newest messages to seed per conversation. What is left
    /// behind is reported, not silently dropped.
    #[arg(long, default_value_t = 500)]
    depth: usize,

    /// Where to put the working copy of the mirror.
    #[arg(long)]
    scratch: Option<PathBuf>,
}

pub fn run(args: FakeSlackArgs) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(args))
}

async fn serve(args: FakeSlackArgs) -> Result<()> {
    let scratch = match args.scratch {
        Some(path) => path,
        None => args.mirror.with_extension("seed.redb"),
    };
    copy(&args.mirror, &scratch)?;
    let mirror = Mirror::open(&scratch)
        .with_context(|| format!("open the mirror copy at {}", scratch.display()))?;

    let workspace = match args.workspace {
        Some(name) => name,
        None => {
            // A rig comes up unattended, so several workspaces is not an
            // error: take the one with the most to say, and print the choice
            // along with what it was chosen over.
            let names = mirror.workspaces();
            // By how much each has to say, not by conversation count: a rig's
            // own workspace accumulates conversations of its own, and the
            // user's real one is the one with the history in it.
            let Some(chosen) = names
                .iter()
                .max_by_key(|name| {
                    mirror
                        .conversations(name)
                        .iter()
                        .map(|conversation| {
                            mirror
                                .all_messages(&Scope::conversation(name, &conversation.id))
                                .len()
                        })
                        .sum::<usize>()
                })
                .cloned()
            else {
                bail!("{} holds no workspace", args.mirror.display());
            };
            if names.len() > 1 {
                println!("workspaces={names:?} (pass --workspace to choose)");
            }
            chosen
        }
    };

    let fake = Fake::start().await?;
    let seeded = seed(&fake, &mirror, &workspace, args.depth);

    // The same lines the fixture prints, in the same order, so anything that
    // reads one can read the other.
    println!("RHO_SLACK_API_BASE={}", fake.api_base());
    println!("ws={}", fake.ws_url());
    println!("control={}", fake.control_url());
    println!("workspace={workspace}");
    println!(
        "seeded={} conversations, {} messages, {} people",
        seeded.conversations, seeded.messages, seeded.users
    );
    if seeded.truncated > 0 {
        println!(
            "truncated={} conversations past --depth {}",
            seeded.truncated, args.depth
        );
    }
    println!("ready");

    std::future::pending::<()>().await;
    Ok(())
}

/// What the mirror turned out to hold, so a run says what it was given.
#[derive(Default)]
pub struct Seeded {
    pub users: usize,
    pub conversations: usize,
    pub messages: usize,
    /// Conversations whose history was longer than `depth`.
    pub truncated: usize,
}

fn seed(fake: &Fake, mirror: &Mirror, workspace: &str, depth: usize) -> Seeded {
    let mut seeded = Seeded::default();

    // The user's own id is the fake's own id from here on.
    let me = mirror.self_id(workspace).map(|id| id.0);
    let map = |id: &str| -> String {
        match &me {
            Some(mine) if mine == id => SELF_ID.to_owned(),
            _ => id.to_owned(),
        }
    };

    let users = mirror.users(workspace);
    for user in &users {
        let User { id, name, handle } = user;
        fake.add_user_named(&map(id.as_str()), handle, name);
    }
    seeded.users = users.len();

    for conversation in mirror.conversations(workspace) {
        let Conversation {
            id,
            kind,
            name,
            user,
            members,
        } = &conversation;
        match kind {
            ConversationKind::Channel => fake.add_channel(id.as_str(), name),
            ConversationKind::Group => {
                let members: Vec<String> =
                    members.iter().map(|member| map(member.as_str())).collect();
                let borrowed: Vec<&str> = members.iter().map(String::as_str).collect();
                fake.add_group(id.as_str(), name, &borrowed);
            }
            ConversationKind::DirectMessage => {
                // A DM the mirror has no partner for is still a DM; naming it
                // after itself keeps it in the list rather than losing it.
                let other = user
                    .as_ref()
                    .map_or_else(|| id.as_str().to_owned(), |user| map(user.as_str()));
                fake.add_dm(id.as_str(), &other);
            }
        }
        seeded.conversations += 1;

        let scope = Scope::conversation(workspace, id);
        let mut history = mirror.all_messages(&scope);
        if history.len() > depth {
            seeded.truncated += 1;
            history.drain(..history.len() - depth);
        }

        // Thread replies live in their own runs in the mirror and in the
        // conversation's history in Slack's API, which is what the fake
        // serves `conversations.replies` out of.
        let mut replies = Vec::new();
        for message in &history {
            if message.reply_count == 0 {
                continue;
            }
            let thread = Scope::thread(workspace, id, &message.ts);
            replies.extend(
                mirror
                    .all_messages(&thread)
                    .into_iter()
                    .filter(|reply| reply.ts != message.ts),
            );
        }
        history.extend(replies);
        history.sort_by(|left, right| left.ts.0.cmp(&right.ts.0));

        for message in &history {
            fake.add_message(id.as_str(), payload(message, &map));
            seeded.messages += 1;
        }

        // Slack's own read cursor, which is the truth for what is unread.
        if let Some(read) = mirror.last_read(&scope) {
            fake.set_last_read(id.as_str(), &read.0);
            let unread = history
                .iter()
                .filter(|message| message.ts.0 > read.0 && message.thread_ts.is_none())
                .count();
            if let Some(latest) = history.last() {
                fake.set_count(id.as_str(), unread > 0, 0, &latest.ts.0);
            }
            // Slack counts messages for a DM and a group DM, and only says
            // "has unreads" for a channel.
            if matches!(
                kind,
                ConversationKind::DirectMessage | ConversationKind::Group
            ) {
                fake.set_unread_count(id.as_str(), unread as u32);
            }
        }
    }

    seeded
}

/// A mirrored message as the API would have delivered it. The fields are the
/// ones `rho_slack::api::parse_message` reads, so what the client gets back is
/// what the mirror holds.
fn payload(message: &Message, map: &dyn Fn(&str) -> String) -> Value {
    let mut value = json!({"ts": message.ts.0, "text": message.text});
    let object = value.as_object_mut().expect("a map");
    if let Some(user) = &message.user {
        object.insert("user".to_owned(), json!(map(user.as_str())));
    }
    if let Some(name) = &message.bot_name {
        object.insert("username".to_owned(), json!(name));
    }
    if let Some(thread) = &message.thread_ts {
        object.insert("thread_ts".to_owned(), json!(thread.0));
    }
    if let Some(subtype) = &message.subtype {
        object.insert("subtype".to_owned(), json!(subtype));
    }
    if !message.blocks.is_empty() {
        object.insert("blocks".to_owned(), json!(message.blocks));
    }
    if message.reply_count > 0 {
        object.insert("reply_count".to_owned(), json!(message.reply_count));
    }
    if let Some(latest) = &message.latest_reply {
        object.insert("latest_reply".to_owned(), json!(latest.0));
    }
    if message.edited {
        object.insert("edited".to_owned(), json!({"ts": message.ts.0}));
    }
    if !message.reactions.is_empty() {
        object.insert(
            "reactions".to_owned(),
            json!(
                message
                    .reactions
                    .iter()
                    .map(|reaction| json!({
                        "name": reaction.name,
                        "count": reaction.count,
                        "users": reaction
                            .users
                            .iter()
                            .map(|user| map(user.as_str()))
                            .collect::<Vec<_>>(),
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    if !message.attachments.is_empty() {
        object.insert(
            "attachments".to_owned(),
            json!(
                message
                    .attachments
                    .iter()
                    .map(|attachment| {
                        let mut value = serde_json::Map::new();
                        for (key, held) in [
                            ("title", &attachment.title),
                            ("text", &attachment.text),
                            ("fallback", &attachment.fallback),
                            ("pretext", &attachment.pretext),
                            ("title_link", &attachment.url),
                            ("service_name", &attachment.service),
                        ] {
                            if let Some(held) = held {
                                value.insert(key.to_owned(), json!(held));
                            }
                        }
                        if attachment.is_unfurl {
                            value.insert("is_msg_unfurl".to_owned(), json!(true));
                        }
                        let fields: Vec<Value> = attachment
                            .fields
                            .iter()
                            .map(|(title, held)| json!({"title": title, "value": held}))
                            .collect();
                        if !fields.is_empty() {
                            value.insert("fields".to_owned(), json!(fields));
                        }
                        Value::Object(value)
                    })
                    .collect::<Vec<_>>()
            ),
        );
    }
    if !message.files.is_empty() {
        object.insert(
            "files".to_owned(),
            json!(
                message
                    .files
                    .iter()
                    .map(|file| json!({
                        "id": file.id,
                        "title": file.title,
                        "filetype": file.filetype,
                        "size": file.size,
                        "url_private": file.url,
                        "original_w": file.original_w,
                        "original_h": file.original_h,
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    value
}

/// Copy the mirror, and the sidecar redb writes beside it if there is one.
fn copy(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        bail!("no mirror at {}", from.display());
    }
    std::fs::copy(from, to)
        .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
    Ok(())
}
