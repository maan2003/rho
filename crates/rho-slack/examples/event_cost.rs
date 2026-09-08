//! What one arriving message costs, part by part, on a mirror the size of a
//! real workspace's history.
//!
//! `list_cost` measures the model alone. This measures the chain the socket
//! actually runs — the mirror write, then the model update — on a mirror of
//! a given number of messages, so the parts can be compared with each other
//! rather than each with zero. The two view-side parts (the rows a drawer
//! patches, and the frame) need a window and are measured where the window
//! is.
//!
//! ```text
//! cargo run --release --example event_cost -- 250000
//! ```
//!
//! The rule under test: an event costs the rows it touches plus the depth of
//! a tree, and never a pass over the mirror or the list. A number here that
//! grows with the argument is a violation; a number that does not is the
//! floor, whatever its size.

use std::time::Instant;

use rho_slack::config::WorkspaceName;
use rho_slack::mirror::{Mirror, Scope};
use rho_slack::model::Model;
use rho_slack::types::{ChannelId, Conversation, ConversationKind, Message, Ts, User, UserId};

/// Conversations in the workspace. Held while the message count varies, so
/// a bigger argument means deeper conversations rather than more of them:
/// the range scan a write lands in is what the size is supposed to stress.
const CONVERSATIONS: usize = 200;

const WORKSPACE: &str = "acme";

fn main() -> anyhow::Result<()> {
    let messages = std::env::args()
        .nth(1)
        .and_then(|size| size.parse().ok())
        .unwrap_or(250_000usize);
    let each = messages / CONVERSATIONS;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("slack.redb");
    let mirror = Mirror::open(&path)?;

    let mut model = Model::new(WorkspaceName(WORKSPACE.to_owned()));
    model.set_self(UserId("ME".to_owned()));
    model.add_users((0..CONVERSATIONS).map(|at| User {
        id: UserId(format!("U{at}")),
        name: format!("person {at}"),
        handle: format!("person{at}"),
    }));
    model.add_conversations((0..CONVERSATIONS).map(|at| Conversation {
        id: ChannelId(format!("C{at}")),
        kind: ConversationKind::Channel,
        name: format!("channel-{at}"),
        user: None,
        members: Vec::new(),
    }));

    // Seeded in one transaction per conversation, which is not how the socket
    // writes and is not what is being timed: this is only the history a timed
    // write has to land in the middle of.
    let seeded_at = Instant::now();
    for at in 0..CONVERSATIONS {
        let channel = ChannelId(format!("C{at}"));
        let scope = Scope::conversation(WORKSPACE, &channel);
        let history = (0..each)
            .map(|step| message(&channel, 1_600_000_000 + (step * 60) as i64))
            .collect::<Vec<_>>();
        mirror.insert_messages(&scope, &history);
        for held in &history {
            model.note_counts(held);
        }
    }
    let held = CONVERSATIONS * each;
    println!("mirror                    {held} messages over {CONVERSATIONS} conversations");
    println!(
        "  on disk                 {:.1} MiB, seeded in {:?}",
        std::fs::metadata(&path)?.len() as f64 / (1024.0 * 1024.0),
        seeded_at.elapsed()
    );

    let runs = 200u32;
    let channel = ChannelId("C0".to_owned());
    let conversation = Scope::conversation(WORKSPACE, &channel);
    let arriving =
        |step: usize| message(&channel, 1_900_000_000 + (step % runs as usize) as i64 * 60);

    // The read `mirror_live` does before every write, to tell a message
    // landing in a conversation the mirror holds nothing for from one landing
    // on top of history. It is a range scan bounded at one row.
    let looked_at = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(mirror.newest_chunk(&conversation, 1));
    }
    let looked = looked_at.elapsed() / runs;

    // One message into the conversation scope: a write transaction with one
    // row in it, committed.
    let wrote_at = Instant::now();
    for step in 0..runs as usize {
        mirror.insert_messages(&conversation, std::slice::from_ref(&arriving(step)));
    }
    let wrote = wrote_at.elapsed() / runs;

    // The whole arrival as `route` runs it now: the thread scope this
    // message roots and the channel, in one transaction. The scope is built
    // inside the loop because a live one is built per message.
    let live_at = Instant::now();
    for step in 0..runs as usize {
        let said = arriving(step);
        let thread = Scope::thread(WORKSPACE, &channel, &said.ts);
        mirror.insert_live(&[&thread, &conversation], &said);
    }
    let live = live_at.elapsed() / runs;

    // What the model does with the same message: the row put back in its
    // place in the list, and the card rule.
    let counts_at = Instant::now();
    for step in 0..runs as usize {
        model.note_counts(&arriving(step));
    }
    let counts = counts_at.elapsed() / runs;
    model.forget_row_edits();

    let noted_at = Instant::now();
    for step in 0..runs as usize {
        std::hint::black_box(model.note_message(&arriving(step), 1_900_100_000));
    }
    let noted = noted_at.elapsed() / runs;
    model.forget_row_edits();

    println!("one arriving message, top level, into a conversation of {each}");
    println!("  mirror, newest_chunk(1) {looked:?}   (×2: the thread scope and the channel)");
    println!("  mirror, insert + commit {wrote:?}   (one scope, one commit)");
    println!("  mirror, one arrival     {live:?}   (both scopes, one commit)");
    println!("  model, note_counts      {counts:?}");
    println!("  model, note_message     {noted:?}");
    println!(
        "  mirror, two commits     {:?}   (what an arrival cost before)",
        (looked + wrote) * 2
    );
    println!("  model total             {:?}", counts + noted);
    Ok(())
}

fn message(channel: &ChannelId, seconds: i64) -> Message {
    Message {
        ts: Ts(format!("{seconds}.000000")),
        thread_ts: None,
        channel: channel.clone(),
        user: Some(UserId("U1".to_owned())),
        bot_name: None,
        blocks: Vec::new(),
        text: "the quick brown fox jumps over the lazy dog again today".to_owned(),
        attachments: Vec::new(),
        files: Vec::new(),
        subtype: None,
        reply_count: 0,
        latest_reply: None,
        edited: false,
        reactions: Vec::new(),
    }
}
