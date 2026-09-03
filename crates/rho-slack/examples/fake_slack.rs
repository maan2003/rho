//! A standalone fake Slack, for the isolated GUI QA run.
//!
//! Starts the same in-process stand-in the transport tests use, seeds a
//! workspace with a mention waiting in it, prints the API base the GUI should
//! be pointed at, and stays up until killed. Nothing here talks to slack.com.

use rho_slack::fake::Fake;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fake = Fake::start().await?;
    // Seeded relative to today, so the transcript's day break and clock times
    // read the way they would in a live workspace rather than a year ago.
    let midnight = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        now - now.rem_euclid(86_400)
    };
    let at = |hour: i64, minute: i64| format!("{}.0", midnight + hour * 3600 + minute * 60);
    fake.add_user("U1", "ada");
    fake.add_user("U2", "kai");
    fake.add_channel("C1", "design");
    fake.add_channel("C2", "random");
    fake.add_dm("D1", "U1");
    fake.set_count("C1", true, 1, &at(8, 0));
    fake.set_count("D1", true, 0, &at(7, 10));
    fake.add_message(
        "C1",
        serde_json::json!({
            "ts": at(7, 43),
            "user": "U2",
            "text": "shipping the new deal curve today",
        }),
    );
    fake.add_message(
        "C1",
        serde_json::json!({
            "ts": at(8, 0),
            "user": "U1",
            "text": "<@ME> can you look at the deploy before the release?",
        }),
    );
    fake.add_message(
        "D1",
        serde_json::json!({"ts": at(7, 10), "user": "U1", "text": "morning!"}),
    );
    fake.add_feed_mention("C1", &at(8, 0));

    println!("RHO_SLACK_API_BASE={}", fake.api_base());
    println!("ws={}", fake.ws_url());
    println!("ready");
    // The seeded mention is already in the feed; the process only has to
    // outlive the GUI it is answering.
    std::future::pending::<()>().await;
    Ok(())
}
