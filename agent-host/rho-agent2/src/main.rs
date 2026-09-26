//! Chat with one agent in a terminal: each line you type is a message, and
//! what the agent sends is printed. `--trace` also prints its cells and the
//! reports it is woken with.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rho_agent2::chat::{ChatEvent, ChatKind};
use rho_agent2::log::{Block, Log, Party};
use rho_agent2::{Agent, Config, Inbound, Trace};
use rho_inference2::Model;
use rho_inference2::openai::{CHATGPT_BASE_URL, OpenAi};
use tokio::io::AsyncBufReadExt;

#[derive(Parser)]
#[command(
    name = "rho-agent2",
    about = "Chat with a rho-agent2 agent in the terminal"
)]
struct Args {
    /// The agent's log; created if missing, resumed if not.
    #[arg(long)]
    log: PathBuf,
    /// Where the agent's commands run.
    #[arg(long, default_value = ".")]
    workdir: PathBuf,
    /// The agent's id, as other agents name it.
    #[arg(long, default_value = "agent")]
    id: String,
    #[arg(long, default_value = "gpt-6-sol")]
    model: String,
    #[arg(long, default_value = "medium")]
    effort: String,
    /// The OAuth credentials file in rho's auth directory.
    #[arg(long, default_value = "default")]
    auth: String,
    #[arg(long, default_value = CHATGPT_BASE_URL)]
    base_url: String,
    /// Also print the agent's cells and reports.
    #[arg(long)]
    trace: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let workdir = camino::Utf8PathBuf::try_from(std::fs::canonicalize(&args.workdir)?)?;
    let shell = rho_tool_shell::ShellTools::in_directory(
        Duration::from_secs(20),
        workdir,
        rho_fs_view::PathOverrides::default(),
    );
    let model = Arc::new(Model::OpenAi(OpenAi {
        base_url: args.base_url,
        model: args.model,
        effort: args.effort,
        auth: args.auth,
    }));
    let (agent, handle) = Agent::new(Config {
        id: args.id.clone(),
        log: Log::open(&args.log)?,
        model,
        shell,
        instructions: rho_agent2::prompt::INSTRUCTIONS.into(),
    })?;
    for event in agent.chat() {
        print_event(&args.id, &event);
    }
    let mut chat = handle.chat();
    let me = args.id.clone();
    tokio::spawn(async move {
        while let Ok(event) = chat.recv().await {
            print_event(&me, &event);
        }
    });
    if args.trace {
        let mut trace = handle.trace();
        tokio::spawn(async move {
            while let Ok(trace) = trace.recv().await {
                match trace {
                    Trace::Woken { why, report } => {
                        eprintln!("\x1b[2m── woken: {why:?}\n{report}\x1b[0m")
                    }
                    Trace::Step { code, prose } => {
                        if !prose.is_empty() {
                            eprintln!("\x1b[2m── prose (undelivered): {prose}\x1b[0m");
                        }
                        if let Some(code) = code {
                            eprintln!("\x1b[2m── cell\n{code}\x1b[0m");
                        }
                    }
                }
            }
        });
    }
    let running = tokio::spawn(agent.run());
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        handle.send(Inbound {
            from: Party::Human,
            body: vec![Block::Text(line)],
        })?;
    }
    drop(handle);
    running.await?
}

fn print_event(me: &str, event: &ChatEvent) {
    match &event.kind {
        ChatKind::Message { from, to, body, .. } => {
            let text = body
                .iter()
                .filter_map(|block| match block {
                    Block::Text(text) => Some(text.as_str()),
                    Block::Quote { .. } => None,
                })
                .collect::<String>();
            match (from, to) {
                (Party::Human, _) => {}
                (Party::Agent(from), Party::Human) => println!("{from}> {text}"),
                (Party::Agent(from), Party::Agent(to)) if from == me => {
                    println!("{from} → {to}> {text}")
                }
                (Party::Agent(from), _) => println!("{from} → {me}> {text}"),
            }
        }
        ChatKind::Status(status) => println!("[{me}: {status}]"),
    }
}
