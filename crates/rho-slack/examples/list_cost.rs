//! What the conversation list costs to draw, at a size the fixture cannot
//! reach.
//!
//! The mirror we can measure against holds five conversations, and five
//! rows say nothing about the difference between sorting the list on every
//! draw and keeping it in order. So this builds a workspace of a given size
//! and times the three things that matter: one draw of the whole list, one
//! draw of a screenful, and one message arriving.
//!
//! ```text
//! cargo run --release --example list_cost -- 5000
//! ```
//!
//! The `sorted` function here is what the list used to do on every draw,
//! kept so the two numbers are measured the same way in the same run.

use std::time::Instant;

use rho_slack::config::WorkspaceName;
use rho_slack::model::{ConversationRow, Model};
use rho_slack::types::{ChannelId, Conversation, ConversationKind, Message, Ts, User, UserId};

fn main() {
    let size = std::env::args()
        .nth(1)
        .and_then(|size| size.parse().ok())
        .unwrap_or(5_000usize);

    let mut model = Model::new(WorkspaceName("acme".to_owned()));
    model.set_self(UserId("ME".to_owned()));
    model.add_users((0..size / 2).map(|at| User {
        id: UserId(format!("U{at}")),
        name: format!("person {at}"),
        handle: format!("person{at}"),
    }));
    model.add_conversations((0..size).map(|at| match at % 2 {
        0 => Conversation {
            id: ChannelId(format!("C{at}")),
            kind: ConversationKind::Channel,
            name: format!("channel-{at}"),
            user: None,
            members: Vec::new(),
        },
        _ => Conversation {
            id: ChannelId(format!("C{at}")),
            kind: ConversationKind::DirectMessage,
            name: format!("C{at}"),
            user: Some(UserId(format!("U{}", at / 2))),
            members: Vec::new(),
        },
    }));
    // Traffic spread over the whole workspace, so the order is not one run
    // of equal keys that any comparator gets right by accident.
    for at in 0..size {
        model.note_counts(&message(&format!("C{at}"), 1_700_000_000 + at as i64));
    }

    let runs = 200;
    println!("conversations             {size}");

    // What a start pays to put the listing on screen the first time.
    let built_at = Instant::now();
    let built = model.conversation_rows();
    println!(
        "the first draw            {:?}   ({} rows)",
        built_at.elapsed(),
        built.len()
    );

    let sorted_at = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(sorted(&model));
    }
    println!("one draw, sorted per draw {:?}", sorted_at.elapsed() / runs);

    let kept_at = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(model.conversation_rows());
    }
    println!("one draw, kept in order   {:?}", kept_at.elapsed() / runs);

    let window_at = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(model.conversation_window(0, 50));
    }
    println!(
        "one draw, a screenful     {:?}   (50 rows of {})",
        window_at.elapsed() / runs,
        model.conversation_count()
    );

    // The events the landing note names, each timed on its own. A badge
    // changing where the row already is; a row coming from the bottom of
    // the list to the top; and the roster landing, which renames every
    // direct message and is the one event that legitimately touches every
    // row.
    let in_place = {
        // A drawer has just drawn, so the log starts empty, as it does on
        // screen. Left to fill, it reaches its cap and the model stops
        // logging, which would flatter every number after it.
        model.forget_row_edits();
        let at = Instant::now();
        for step in 0..runs {
            // Same conversation, newer each time: it is already at the top,
            // so its row does not move and only its line changes.
            model.note_counts(&message("C0", 1_900_000_000 + step as i64));
        }
        at.elapsed() / runs
    };
    println!("one badge, row unmoved    {in_place:?}");

    let bottom_to_top = {
        model.forget_row_edits();
        let at = Instant::now();
        for step in 0..runs {
            // The conversation nothing has happened in for longest, which
            // is the far end of the list from where it lands.
            model.note_counts(&message(
                &format!("C{}", size - 1 - (step as usize % 8)),
                1_950_000_000 + step as i64,
            ));
        }
        at.elapsed() / runs
    };
    println!("one row, bottom to top    {bottom_to_top:?}");

    model.forget_row_edits();
    let roster_at = Instant::now();
    model.add_users((0..size / 2).map(|at| User {
        id: UserId(format!("U{at}")),
        name: format!("someone {at}"),
        handle: format!("someone{at}"),
    }));
    println!(
        "the roster landing        {:?}   (every row, once a session)",
        roster_at.elapsed()
    );

    // One message. The row it lands in moves and no other row is touched.
    // Kept under the log's cap so the model is logging throughout, which
    // is what it does with a drawer on screen.
    let events = 4_000;
    model.forget_row_edits();
    let event_at = Instant::now();
    for at in 0..events {
        model.note_counts(&message(
            &format!("C{}", at % size),
            1_800_000_000 + at as i64,
        ));
    }
    println!(
        "one message arriving      {:?}",
        event_at.elapsed() / events as u32
    );
}

/// What the list used to do on every draw: build every row, then sort them.
fn sorted(model: &Model) -> Vec<ConversationRow> {
    let mut rows = model.conversation_rows();
    rows.sort_by(|left, right| {
        left.muted
            .cmp(&right.muted)
            .then_with(|| right.unread.cmp(&left.unread))
            .then_with(|| right.mention_count.cmp(&left.mention_count))
            .then_with(|| {
                let latest = |row: &ConversationRow| {
                    row.latest
                        .as_ref()
                        .map(Ts::epoch_seconds)
                        .unwrap_or_default()
                };
                latest(right).total_cmp(&latest(left))
            })
            .then_with(|| left.label.cmp(&right.label))
    });
    rows
}

fn message(channel: &str, seconds: i64) -> Message {
    Message {
        ts: Ts(format!("{seconds}.000000")),
        thread_ts: None,
        channel: ChannelId(channel.to_owned()),
        user: Some(UserId("U1".to_owned())),
        bot_name: None,
        blocks: Vec::new(),
        text: "traffic".to_owned(),
        attachments: Vec::new(),
        files: Vec::new(),
        subtype: None,
        reply_count: 0,
        latest_reply: None,
        edited: false,
        reactions: Vec::new(),
    }
}
