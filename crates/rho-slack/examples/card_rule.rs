//! What the card rule costs and what it hands over, measured on a copy of a
//! real mirror.
//!
//! Run it against a copy, never the live file:
//!
//! ```text
//! cp --reflink=auto ~/.local/state/rho/slack.redb /tmp/copy/slack.redb
//! cargo run --example card_rule -- /tmp/copy/slack.redb acme
//! ```
//!
//! It reports the two things a landing note has to say. What the reader
//! sees: how many cards the old rule handed over, how many the new one
//! does, and the top ten with the words each card carries. And what it
//! costs: a start, one message, one mark, and one draw.

use std::time::Instant;

use anyhow::Context as _;
use rho_slack::config::WorkspaceName;
use rho_slack::mirror::{Mirror, Scope};
use rho_slack::model::{Model, Unit};
use rho_slack::session::{derive_units, restore_units, seed_read_cursors};
use rho_slack::types::{ChannelId, Ts};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: card_rule <copy of slack.redb> [workspace]");
        std::process::exit(2);
    });
    // The workspace is the file's own to say, so a reader handed a copy
    // does not have to be told whose it is.
    let named = args.next();
    anyhow::ensure!(
        !path.starts_with(&format!(
            "{}/.local/state/rho",
            std::env::var("HOME").unwrap_or_default()
        )),
        "that is the live mirror; run this on a copy"
    );

    let mirror = Mirror::open(&path)?;
    let held = mirror.workspaces();
    let workspace = match named {
        Some(named) => named,
        None => held
            .first()
            .cloned()
            .context("the mirror names no workspace; pass one")?,
    };
    if held.len() > 1 {
        println!("workspaces held  {}", held.join(", "));
    }
    let name = WorkspaceName(workspace.clone());

    // The old start: every message in the mirror read, to work out facts
    // that were already worked out once.
    let derived_at = Instant::now();
    let mut old = model(&mirror, &name);
    derive_units(&mut old, &mirror);
    let derived = derived_at.elapsed();
    let messages: usize = mirror
        .conversations(&workspace)
        .iter()
        .map(|conversation| {
            mirror
                .all_messages(&Scope::conversation(&workspace, &conversation.id))
                .len()
        })
        .sum();

    // The new start: one row per unit, no message read. The units have to
    // be on disk for that, so they are written here the once, which is what
    // a mirror from before the table does on its first open.
    for unit in old.tracked() {
        if let Some(facts) = old.unit(&unit) {
            mirror.put_unit(&workspace, &unit, facts);
        }
    }
    let restored_at = Instant::now();
    let mut new = model(&mirror, &name);
    restore_units(&mut new, &mirror);
    let restored = restored_at.elapsed();

    // The old rule: a unit is a card while anyone else has written past the
    // desk's cursor, and on a desk that has said nothing that is every unit
    // anyone else has written in at all.
    let before = old
        .tracked()
        .into_iter()
        .filter(|unit| {
            old.unit(unit)
                .is_some_and(|facts| facts.newest_from_other.is_some())
        })
        .count();
    let cards = new.cards(now_ms());

    println!("mirror         {path}");
    println!("workspace      {workspace}");
    println!(
        "held           {} conversations, {messages} messages, {} units",
        mirror.conversations(&workspace).len(),
        new.tracked().len()
    );
    println!();
    println!("cards, old rule   {before}");
    println!("cards, new rule   {}", cards.len());
    println!();
    println!("the top ten, and what each one says:");
    for card in cards.iter().take(10) {
        let reason = rho_slack::model::reason_text(
            card.attention.expect("a card is a card because it asks"),
            &card.conversation,
        );
        println!("  {:>6.1}d  {reason}", card.wait_days);
    }
    if cards.is_empty() {
        println!("  (nothing is asking)");
    }
    println!();
    println!("start, reading messages   {derived:?}   ({messages} messages)");
    println!(
        "start, reading units      {restored:?}   ({} units)",
        new.tracked().len()
    );
    println!();

    // One event, and one draw. Timed over a run so the number is not one
    // scheduling accident.
    let channel = mirror
        .conversations(&workspace)
        .first()
        .map(|conversation| conversation.id.clone())
        .unwrap_or(ChannelId("C1".into()));
    let sample = 2_000;
    let marked_at = Instant::now();
    for step in 0..sample {
        new.mark_read(&channel, &Ts(format!("{}.000000", 2_000_000_000 + step)));
    }
    println!(
        "one mark                  {:?}",
        marked_at.elapsed() / sample as u32
    );

    let drawn_at = Instant::now();
    for _ in 0..sample {
        std::hint::black_box(new.cards(now_ms()));
    }
    println!(
        "one draw of the cards     {:?}   ({} cards)",
        drawn_at.elapsed() / sample as u32,
        cards.len()
    );

    let listed_at = Instant::now();
    for _ in 0..sample {
        std::hint::black_box(new.conversation_rows());
    }
    println!(
        "one draw of the list      {:?}   ({} rows, sorted per draw: change 2b)",
        listed_at.elapsed() / sample as u32,
        new.conversation_rows().len()
    );
    let _ = Unit::conversation(&channel);
    Ok(())
}

fn model(mirror: &Mirror, name: &WorkspaceName) -> Model {
    let mut model = Model::new(name.clone());
    if let Some(id) = mirror.self_id(&name.0) {
        model.set_self(id);
    }
    model.add_users(mirror.users(&name.0));
    model.add_conversations(mirror.conversations(&name.0));
    seed_read_cursors(&mut model, mirror);
    model
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
