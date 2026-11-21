//! Seed a disposable host for the thread screenshots in
//! product/design/README.md.
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rho_agent::store::{MailWrite, NewMessage, StoreWrite};
use rho_agent_types::{
    Address, AgentId, AgentIdDomain, ContentPart, Mention, MessageId, UnixMs, WorksetId,
    WorksetKind,
};
use rho_agents_client::protocol::worksets::{CreateWorkset, Eval};
use rho_rpc::protocol::RuntimePaths;
use rho_rpc::protocol::client::call;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("seed") => {
            let path = PathBuf::from(args.next().context("database path")?);
            let flash_lead = args.next().map(|s| s.parse()).transpose()?;
            seed(path, flash_lead).await
        }
        Some("flash") => {
            let paths = runtime(&mut args)?;
            let cwd = args.next().context("fixture directory")?;
            let thread = args.next().context("fixture thread ID")?.parse()?;
            let words = args.next().context("flash text")?;
            send(&paths, &cwd, thread, &words, true).await
        }
        Some("send") => {
            let paths = runtime(&mut args)?;
            let cwd = args.next().context("fixture directory")?;
            let thread: u64 = args.next().context("fixture thread ID")?.parse()?;
            let text = args.next().context("mail text")?;
            send(&paths, &cwd, thread, &text, false).await
        }
        Some("edit") => {
            let paths = runtime(&mut args)?;
            let workset = args.next().context("fixture workset ID")?.parse()?;
            let agent = args.next().context("fixture agent ID")?;
            let message: u64 = args.next().context("fixture mail ID")?.parse()?;
            let text = args.next().context("mail text")?;
            eval(
                &paths,
                workset,
                format!(
                    "from rho.tools import Mail\nMail({agent:?}).edit({message}, {})",
                    serde_json::to_string(&text)?
                ),
            )
            .await?;
            Ok(())
        }
        _ => anyhow::bail!(
            "thread_reading_fixture seed DATABASE [FLASH_LEAD_SECONDS] | (send|flash) RUNTIME DIRECTORY THREAD TEXT | edit RUNTIME WORKSET AGENT MAIL TEXT"
        ),
    }
}

fn runtime(args: &mut impl Iterator<Item = String>) -> anyhow::Result<RuntimePaths> {
    RuntimePaths::new(Some(PathBuf::from(args.next().context("runtime path")?)))
}

async fn seed(path: PathBuf, flash_lead: Option<u64>) -> anyhow::Result<()> {
    anyhow::ensure!(!path.exists(), "fixture database must not exist");
    let root = path.parent().context("database directory")?;
    std::fs::create_dir_all(root)?;
    let db = rho_db::RhoDb::open(path);
    rho_agent::db::prepare(&db).await;
    let mut tx = db.write().await;
    tx.init_store_tables();
    let worker =
        AgentId::from_counter(71, &AgentIdDomain(u64::MAX - 1)).context("fixture agent ID")?;
    let old = UnixMs(UnixMs::now().0 - 30 * 24 * 60 * 60_000);
    let today = UnixMs(UnixMs::now().0 - 13 * 60_000);
    let opening = "Check the long clock history.";
    let (project, thread) = {
        let mut post = |at: UnixMs, parent: Option<MessageId>, from, text: String, ping| {
            tx.post(at, parent, message(from, text, ping), None)
        };
        let project = post(old, None, Address::User, "clock".into(), false)?;
        let thread = post(old, Some(project), Address::User, opening.into(), false)?;
        for n in 1..=10 {
            post(
                old,
                Some(thread),
                Address::Agent(worker),
                format!(
                    "March note {n}: retain this reading position.\n\nThe reference clock stays as it was."
                ),
                false,
            )?;
        }
        post(today, Some(thread), Address::Agent(worker),
            "The latest run uses a steady clock.\n\nToday: we only changed the timer, not the retry policy.".into(), false)?;
        post(
            UnixMs(today.0 + 60_000),
            Some(thread),
            Address::User,
            "Go ahead with the steady clock.".into(),
            false,
        )?;
        (project, thread)
    };
    let ask_at = if let Some(lead) = flash_lead {
        anyhow::ensure!(
            (1..=1200).contains(&lead),
            "flash lead must be 1–1200 seconds"
        );
        let now = UnixMs::now();
        let at = UnixMs(now.0 - (1200 - lead) * 1000);
        let second = AgentId::from_counter(72, &AgentIdDomain(u64::MAX - 1))
            .context("second fixture agent ID")?;
        for (author, text) in [
            (worker, "I will run the suite with a separate clock."),
            (worker, "The first twenty checks passed."),
            (second, "I am comparing the two clock inputs."),
            (worker, "Running the suite with a separate clock, 40 of 50."),
        ] {
            tx.post_flash(
                at,
                thread,
                author,
                vec![ContentPart::Text { text: text.into() }],
            )?;
        }
        println!("flash deadline Unix ms: {}", now.0 + lead * 1000);
        now
    } else {
        UnixMs(today.0 + 2 * 60_000)
    };
    tx.post(
        ask_at,
        Some(thread),
        message(
            Address::Agent(worker),
            "Does the steady clock cover both clocks?".into(),
            true,
        ),
        None,
    )?;
    tx.commit();
    drop(db);
    println!("clock project {}, thread {}", project.0.0, thread.0.0);
    Ok(())
}

fn message(from: Address, text: String, ping: bool) -> NewMessage {
    NewMessage {
        from,
        reply_to: None,
        mentions: if ping {
            vec![Mention::User { ping: true }]
        } else {
            vec![]
        },
        content: vec![ContentPart::Text { text }],
    }
}

async fn send(
    paths: &RuntimePaths,
    cwd: &str,
    thread: u64,
    words: &str,
    flash: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(Path::new(cwd).is_dir(), "fixture directory must exist");
    let created = call(
        paths,
        CreateWorkset {
            kind: WorksetKind::Host { cwd: cwd.into() },
            code: r#"new_agent(runtime={"kind": "native", "model": "sol", "effort": "medium"})"#
                .into(),
        },
    )
    .await?;
    let agent = created
        .result
        .agent
        .context("fixture agent creation failed")?;
    let agent = agent.encoded();
    let workset = created.workset;
    let words = serde_json::to_string(words)?;
    let code = if flash {
        format!(
            "from rho.tools import Chat\nprint(int(Chat({agent:?}).send({words}, 'flash', thread={thread})))"
        )
    } else {
        format!(
            "from rho.tools import Mail\nprint(int(Mail({agent:?}).send('user', {words}, thread={thread}, ping=False)))"
        )
    };
    let result = eval(paths, workset, code).await?;
    let message: u64 = result.lines().last().context("fixture mail ID")?.parse()?;
    println!("{workset} {agent} {message}");
    Ok(())
}

async fn eval(paths: &RuntimePaths, workset: WorksetId, code: String) -> anyhow::Result<String> {
    let result = call(paths, Eval { workset, code }).await?;
    anyhow::ensure!(result.error.is_none(), "{:?}", result.error);
    Ok(result.output.trim().to_string())
}
