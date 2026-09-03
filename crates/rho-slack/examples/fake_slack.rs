//! A standalone fake Slack, for the isolated GUI QA run.
//!
//! Starts the same in-process stand-in the transport tests use, seeds a
//! workspace that reproduces the reference conversation from the UX
//! checklist, prints the API base the GUI should be pointed at, and stays up
//! until killed. Nothing here talks to slack.com.
//!
//! The seed is the spec's fixture: every construct the checklist asks rho to
//! render appears at least once, so a screenshot of this workspace is enough
//! to judge an item.

use rho_slack::fake::Fake;
use serde_json::json;

/// The reference group DM, named the way Slack names one.
const GROUP: &str = "G1";
const GROUP_NAME: &str = "mpdm-david--manmeet--keith-1";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fake = Fake::start().await?;
    // Seeded relative to today, so the transcript's day breaks and clock
    // times read the way they would in a live workspace.
    let midnight = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        now - now.rem_euclid(86_400)
    };
    // `at(days_ago, hour, minute)`: the history spans a week with gaps.
    let at = |days: i64, hour: i64, minute: i64| {
        format!(
            "{}.000000",
            midnight - days * 86_400 + hour * 3600 + minute * 60
        )
    };

    // Display names differ from handles, which is the normal case and the
    // one that catches a client rendering the handle.
    fake.add_user_named("ME", "manmeet", "Manmeet");
    fake.add_user_named("UD", "david", "David");
    fake.add_user_named("UK", "keith", "Keith");
    fake.add_user_named("UA", "ada", "Ada Lovelace");
    fake.add_user_named("UB", "kai", "Kai");

    fake.add_group(GROUP, GROUP_NAME, &["ME", "UD", "UK"]);
    fake.add_channel("C1", "design");
    fake.add_channel("C2", "random");
    fake.add_private_channel("P1", "founders");
    fake.add_dm("D1", "UD");

    // One custom emoji, which stays a shortcode, next to standard ones that
    // become glyphs.
    fake.add_emoji(
        "forrest_gump_wave",
        "https://emoji.slack-edge.com/T1/forrest_gump_wave.png",
    );

    seed_reference_group(&fake, &at);
    seed_design(&fake, &at);
    seed_random_backlog(&fake, midnight);

    fake.add_message(
        "D1",
        json!({"ts": at(0, 9, 12), "user": "UD", "text": "did you see the deploy?"}),
    );

    // The unread rule belongs mid-history: everything from Keith's morning
    // message on is unread.
    fake.set_last_read(GROUP, &at(1, 16, 41));
    fake.set_count(GROUP, true, 1, &at(0, 10, 2));
    fake.set_count("C1", true, 1, &at(0, 8, 0));
    fake.set_count("D1", true, 0, &at(0, 9, 12));
    fake.add_feed_mention("C1", &at(0, 8, 0));

    println!("RHO_SLACK_API_BASE={}", fake.api_base());
    println!("ws={}", fake.ws_url());
    println!("control={}", fake.control_url());
    println!("group={GROUP}");
    println!("ready");

    // `--live` is the hands-off case: a workspace someone else is typing in,
    // so a screenshot taken a minute apart shows whether an open
    // conversation is following the socket at all.
    if std::env::args().any(|argument| argument == "--live") {
        live_script(&fake).await;
    }
    // The seeded mention is already in the feed; the process only has to
    // outlive the GUI it is answering.
    std::future::pending::<()>().await;
    Ok(())
}

/// The conversation the reference screenshot was taken from: every rendering
/// construct the checklist names, in one buffer.
fn seed_reference_group(fake: &Fake, at: &dyn Fn(i64, i64, i64) -> String) {
    let bot_icon = fake.bot_icon_url();
    let files = format!(
        "{}/files/image.png",
        fake.api_base().trim_end_matches("/api")
    );
    let say = |ts: String, user: &str, text: &str| {
        fake.add_message(GROUP, json!({"ts": ts, "user": user, "text": text}))
    };

    // Six days back, so the buffer has day separators with gaps between them.
    say(
        at(6, 11, 3),
        "UD",
        "kicking this off :thumbsup: :forrest_gump_wave:",
    );
    say(
        at(6, 11, 4),
        "UK",
        "*bold*, _italic_, ~struck~, `inline code`",
    );
    fake.add_message(
        GROUP,
        json!({
            "ts": at(6, 11, 6),
            "user": "UK",
            "text": "```\nfn main() {\n    println!(\"hi\");\n}\n```",
        }),
    );
    say(
        at(6, 11, 9),
        "UD",
        "> the quote line\nand a list:\n- first\n- second\n1. numbered",
    );
    say(
        at(6, 11, 12),
        "UD",
        "docs are at <https://example.com/spec|the spec>, see <#C1|design>",
    );
    say(
        at(6, 11, 14),
        "UK",
        "<!here> and <!subteam^S1|@design-team> and <!date^1756800000^{date_short} at {time}|Sep 2 at 08:00>",
    );

    // A parent with replies, so a reply count can hang under it.
    let parent = at(4, 14, 20);
    fake.add_message(
        GROUP,
        json!({
            "ts": parent,
            "user": "UD",
            "text": "can we settle the release date? <@ME>",
            "reply_count": 3,
            "reply_users_count": 2,
            "latest_reply": at(4, 14, 41),
            "reply_users": ["UK", "ME"],
            "reactions": [
                {"name": "thumbsup", "users": ["UK", "ME"], "count": 2},
                {"name": "tada", "users": ["UD"], "count": 1},
            ],
        }),
    );
    fake.add_message(
        GROUP,
        json!({
            "ts": at(4, 14, 25),
            "thread_ts": parent,
            "user": "UK",
            "text": "friday works for me :sweat_smile:",
        }),
    );
    fake.add_message(
        GROUP,
        json!({
            "ts": at(4, 14, 33),
            "thread_ts": parent,
            "user": "ME",
            "text": "friday it is",
            "edited": {"user": "ME", "ts": at(4, 14, 34)},
        }),
    );
    // A reply that was also sent to the conversation.
    fake.add_message(
        GROUP,
        json!({
            "ts": at(4, 14, 41),
            "thread_ts": parent,
            "user": "UK",
            "subtype": "thread_broadcast",
            "text": "settled: friday",
        }),
    );

    fake.add_message(
        GROUP,
        json!({
            "ts": at(3, 9, 2),
            "user": "UK",
            "subtype": "channel_join",
            "text": "<@UK> has joined the channel",
        }),
    );
    // A file attachment.
    fake.add_message(
        GROUP,
        json!({
            "ts": at(3, 9, 40),
            "user": "UD",
            "text": "here is the mock",
            "files": [{
                "id": "F1",
                "name": "image.png",
                "title": "image.png",
                "mimetype": "image/png",
                "filetype": "png",
                "size": 225_280,
                "url_private": files.clone(),
            }],
        }),
    );

    // A bot post with blocks and a legacy attachment unfurl.
    fake.add_message(
        GROUP,
        json!({
            "ts": at(1, 16, 20),
            "bot_id": "B1",
            "username": "deploybot",
            "bot_profile": {
                "id": "B1",
                "name": "deploybot",
                "icons": {"image_48": bot_icon.clone()},
            },
            "text": "deploy finished",
            "blocks": [{
                "type": "section",
                "text": {"type": "mrkdwn", "text": "*deploy finished* in 4m12s"},
            }],
            "attachments": [{
                "title": "build #412",
                "pretext": "pipeline",
                "text": "all checks passed",
                "fallback": "build #412 passed",
                "fields": [
                    {"title": "branch", "value": "main", "short": true},
                    {"title": "duration", "value": "4m12s", "short": true},
                ],
            }],
        }),
    );
    // A link unfurl, which must collapse rather than paint a preview.
    fake.add_message(
        GROUP,
        json!({
            "ts": at(1, 16, 41),
            "user": "UD",
            "text": "<https://example.com/post|worth a read>",
            "attachments": [{
                "is_msg_unfurl": true,
                "title": "Worth a read",
                "text": "A long preview body that should never reach the buffer.",
                "fallback": "Worth a read",
            }],
        }),
    );

    // Everything from here is unread: `last_read` sits on the message above.
    say(at(0, 10, 0), "UK", "morning! :wave:");
    say(
        at(0, 10, 2),
        "UD",
        "<@ME> can you take the release notes today?",
    );
}

/// The channel that shows thread isolation: replies are interleaved today,
/// so 1.6 has a before and an after in the same buffer.
fn seed_design(fake: &Fake, at: &dyn Fn(i64, i64, i64) -> String) {
    fake.add_message(
        "C1",
        json!({"ts": at(2, 7, 43), "user": "UB", "text": "shipping the new deal curve today"}),
    );

    let parent = at(2, 9, 10);
    fake.add_message(
        "C1",
        json!({
            "ts": parent,
            "user": "UA",
            "text": "the curve needs a name",
            "reply_count": 3,
            "reply_users_count": 2,
            "latest_reply": at(2, 9, 31),
            "reply_users": ["UB", "ME"],
        }),
    );
    for (minute, user, text) in [
        (18, "UB", "\"deal curve\" is fine"),
        (24, "ME", "let us keep it"),
    ] {
        fake.add_message(
            "C1",
            json!({
                "ts": at(2, 9, minute),
                "thread_ts": parent,
                "user": user,
                "text": text,
            }),
        );
    }
    fake.add_message(
        "C1",
        json!({
            "ts": at(2, 9, 31),
            "thread_ts": parent,
            "user": "UB",
            "subtype": "thread_broadcast",
            "text": "named: deal curve",
        }),
    );

    fake.add_message(
        "C1",
        json!({"ts": at(0, 7, 43), "user": "UB", "text": "rolling it out this morning"}),
    );
    fake.add_message(
        "C1",
        json!({
            "ts": at(0, 8, 0),
            "user": "UA",
            "text": "<@ME> can you look at the deploy before the release?",
        }),
    );
}

/// A conversation long enough that paging is exercised: 240 messages across
/// four days, which is more than one page at any limit rho uses.
fn seed_random_backlog(fake: &Fake, midnight: i64) {
    for index in 0..240i64 {
        // Four days back to two days back, ten minutes apart, so the buffer
        // crosses day boundaries while paging.
        let ts = midnight - 4 * 86_400 + index * 600;
        let user = if index % 2 == 0 { "UA" } else { "UB" };
        fake.add_message(
            "C2",
            json!({
                "ts": format!("{ts}.000000"),
                "user": user,
                "text": format!("backlog message {}", index + 1),
            }),
        );
    }
}

/// The scripted workspace: a message every few seconds, then the events that
/// change a message already on screen. The QA run watches an open
/// conversation while this runs; anything that does not appear is a bug in
/// the client, because the fake pushed the frame and moved the history.
async fn live_script(fake: &Fake) {
    const BEAT: std::time::Duration = std::time::Duration::from_secs(4);
    let mut round = 0u32;
    loop {
        tokio::time::sleep(BEAT).await;
        round += 1;
        let ts = fake.live_message(GROUP, "UD", &format!("live message {round}"));
        println!("live: message {round} at {ts}");

        tokio::time::sleep(BEAT).await;
        fake.live_reply(GROUP, &ts, "UK", &format!("live reply to {round}"));
        println!("live: reply to {round}");

        tokio::time::sleep(BEAT).await;
        fake.live_reaction(GROUP, &ts, "UK", "eyes");
        println!("live: reaction on {round}");

        tokio::time::sleep(BEAT).await;
        fake.live_edit(
            GROUP,
            &ts,
            &format!("live message {round}, with a second thought"),
        );
        println!("live: edit of {round}");

        tokio::time::sleep(BEAT).await;
        fake.live_delete(GROUP, &ts);
        println!("live: delete of {round}");

        tokio::time::sleep(BEAT).await;
        fake.live_message(GROUP, "UD", &format!("<@ME> round {round} is done"));
        println!("live: mention for {round}");
    }
}
