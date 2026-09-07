//! What one reaction costs: the menu the reader opens, the toggle they
//! press, and the two writes that outlive the keystroke.
//!
//! ```text
//! cargo run --release --example react_cost -- 20000
//! ```
//!
//! The claim the landing note makes is that none of it grows with the
//! workspace or with the conversation: a reaction names one message, and
//! what it touches is that message, the reader's short list of recent
//! emoji, and the two rows of the mirror those live in. This measures each
//! of them at a workspace the fixture cannot reach.

use std::time::Instant;

use rho_slack::config::WorkspaceName;
use rho_slack::mirror::{Mirror, Scope};
use rho_slack::model::Model;
use rho_slack::types::{ChannelId, Conversation, ConversationKind, Message, Reaction, Ts, UserId};

fn main() -> anyhow::Result<()> {
    let size = std::env::args()
        .nth(1)
        .and_then(|size| size.parse().ok())
        .unwrap_or(20_000usize);
    let history = 20_000usize;

    let mut model = Model::new(WorkspaceName("acme".to_owned()));
    model.set_self(UserId("ME".to_owned()));
    model.add_conversations((0..size).map(|at| Conversation {
        id: ChannelId(format!("C{at}")),
        kind: ConversationKind::Channel,
        name: format!("channel-{at}"),
        user: None,
        members: Vec::new(),
    }));
    println!("conversations             {size}");
    println!("messages in the mirror    {history}");

    let runs = 200;

    // The reader's recent emoji: what the menu's second group is, and the
    // only list a toggle rewrites. It is capped, so this is the same
    // number in an empty workspace and in this one.
    let recents_at = Instant::now();
    for step in 0..runs {
        std::hint::black_box(model.note_reaction_used(NAMES[step as usize % NAMES.len()]));
    }
    println!(
        "one toggle, the recents   {:?}   ({} remembered)",
        recents_at.elapsed() / runs,
        model.reacted_with().len()
    );

    // The mirror. A toggle writes the message it landed on and the recents
    // list, and nothing else; the conversation it is in holds `history`
    // messages, so this is the write against a full one.
    let state = tempfile::tempdir()?;
    let mirror = Mirror::open(state.path().join("slack.redb"))?;
    let channel = ChannelId("C0".to_owned());
    let scope = Scope::conversation("acme", &channel);
    let held = (0..history)
        .map(|at| message(&channel, 1_700_000_000 + at as i64))
        .collect::<Vec<_>>();
    let seeded_at = Instant::now();
    mirror.insert_messages(&scope, &held);
    println!(
        "the history, once          {:?}   ({history} messages)",
        seeded_at.elapsed()
    );

    let mut reacted = held[history / 2].clone();
    reacted.reactions = vec![Reaction {
        name: "tada".to_owned(),
        count: 1,
        users: vec![UserId("UD".to_owned())],
    }];
    let write_at = Instant::now();
    for step in 0..runs {
        // On and off again, so the number is a keystroke and its undo and
        // not one lucky direction.
        reacted.reactions[0].users = match step % 2 {
            0 => vec![UserId("UD".to_owned()), UserId("ME".to_owned())],
            _ => vec![UserId("UD".to_owned())],
        };
        mirror.insert_messages(&scope, std::slice::from_ref(&reacted));
    }
    println!(
        "one toggle, the message   {:?}   (one row of {history})",
        write_at.elapsed() / runs
    );

    let names = model.reacted_with().to_vec();
    let list_at = Instant::now();
    for _ in 0..runs {
        mirror.set_reacted_with("acme", &names);
    }
    println!(
        "one toggle, the list      {:?}   ({} names)",
        list_at.elapsed() / runs,
        names.len()
    );

    let read_at = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(mirror.reacted_with("acme"));
    }
    println!("a start, the list back    {:?}", read_at.elapsed() / runs);
    Ok(())
}

/// Emoji names in the order a reader reaches for them, long enough to run
/// the recents list past its cap.
const NAMES: [&str; 12] = [
    "thumbsup",
    "white_check_mark",
    "eyes",
    "tada",
    "heart",
    "pray",
    "rocket",
    "fire",
    "clap",
    "sob",
    "thinking_face",
    "wave",
];

fn message(channel: &ChannelId, seconds: i64) -> Message {
    Message {
        ts: Ts(format!("{seconds}.000000")),
        thread_ts: None,
        channel: channel.clone(),
        user: Some(UserId("UD".to_owned())),
        bot_name: None,
        blocks: Vec::new(),
        text: "shipping it".to_owned(),
        attachments: Vec::new(),
        files: Vec::new(),
        subtype: None,
        reply_count: 0,
        latest_reply: None,
        edited: false,
        reactions: Vec::new(),
    }
}
