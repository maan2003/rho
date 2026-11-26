//! Seed and drive the "heavy workday" fixture behind
//! product/design/capture-matrix.sh and product/design/qa-session.sh: three
//! hosts and one device in the middle of a messy eight-hour day.
//!
//! `seed RIG` writes, before anything runs:
//! - `lab`'s store: projects `tide`, `rho` and `infra` with waiting asks,
//!   results, three stops and a refused start, the long `Flaky clock test`
//!   thread (code, quotes, links, an edit and a fresh flash), a muted and a
//!   snoozed ask, a settled corpus of more than 200 messages, Claude allowance
//!   for two accounts and thirty days of spend;
//! - `gpu-box`'s store, whose log also goes into the device's copy, so the
//!   device holds its cards while it is away (it never runs);
//! - `ci`'s store: the `nightly` project that `ci-day` puts to work;
//! - the device: a paired ledger with fifteen notes, a private reply, the mute,
//!   the snooze and the done verdicts, plus the long thread's draft.
//!
//! `ci-day RIG MODEL_PID` runs on the live `ci` host: an idea runs real
//! commands, its model then goes away (MODEL_PID is killed) and a reply
//! leaves it retrying, which is `working` until it is `stuck`; a second
//! idea is stopped, which is `stopped by you`.
//!
//! `say RIG HOST THREAD TEXT [flash]` posts as a fresh agent on a live host
//! (the `↑ new` notice); `edit` revises what it said; `py RIG HOST CODE`
//! runs Python against a live host's chat and agents.
//!
//! `check-png FILE [PREVIOUS]` and `check-webm FILE` judge the captures:
//! they print what they measured as JSON and fail on a blank or frozen
//! frame, or on a recording without frames.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use rho_agent::db::{AgentWriteTxnExt as _, QuotaModel, QuotaObservationRecord, QuotaProvider};
use rho_agent::log::{AGENT_USAGE_BUCKET_MS, AgentUsageBucket, AgentUsageModel};
use rho_agent::store::{MailRead as _, MailWrite as _, NewMessage, StoreWrite as _};
use rho_agent_types::{
    Address, AgentId, AgentIdDomain, ChatRecord, ChatSeq, ContentPart, EndpointId, MessageId,
    ProjectConfig, ProjectId, ProjectMode, ProjectPlace, RuntimeLabel, Stop, UnixMs, Voice,
    WorksetId, WorksetKind,
};
use rho_agents_client::protocol::worksets::{CreateWorkset, Eval, StopAgent};
use rho_dealer::Addressed as _;
use rho_dealer::facts::{Device, Entry, Fact, Seen};
use rho_rpc::protocol::RuntimePaths;
use rho_rpc::protocol::client::call;
use serde_json::json;

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["seed", rig] => seed(Path::new(rig)),
        ["ci-day", rig, model_pid] => {
            runtime()?.block_on(ci_day(Path::new(rig), model_pid.parse()?))
        }
        ["say", rig, host, thread, text, rest @ ..] => {
            let flash = match rest {
                [] => false,
                ["flash"] => true,
                _ => anyhow::bail!("say takes `flash` or nothing after the text"),
            };
            runtime()?.block_on(say(Path::new(rig), host, thread.parse()?, text, flash))
        }
        ["edit", rig, host, workset, agent, message, text] => runtime()?.block_on(edit(
            Path::new(rig),
            host,
            workset.parse()?,
            agent,
            message.parse()?,
            text,
        )),
        ["check-png", file, rest @ ..] => match rest {
            [] => check_png(Path::new(file), None),
            [previous] => check_png(Path::new(file), Some(Path::new(previous))),
            _ => anyhow::bail!("check-png takes a file and an optional previous file"),
        },
        ["check-webm", file] => check_webm(Path::new(file)),
        ["py", rig, host, code] => runtime()?.block_on(py(Path::new(rig), host, code)),
        _ => anyhow::bail!(
            "heavy_workday seed RIG | ci-day RIG MODEL_PID | say RIG HOST THREAD TEXT [flash] | edit RIG HOST WORKSET AGENT MESSAGE TEXT | py RIG HOST CODE | check-png FILE [PREVIOUS] | check-webm FILE"
        ),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// What `seed` leaves for the scripts and the live commands, in
/// `RIG/fixture.json`.
#[derive(Default)]
struct Ids {
    endpoints: BTreeMap<&'static str, EndpointId>,
    /// Named threads and projects, by a stable key, per host.
    threads: BTreeMap<String, (&'static str, MessageId)>,
}

impl Ids {
    fn thread(&mut self, key: &str, host: &'static str, id: MessageId) {
        self.threads.insert(key.to_owned(), (host, id));
    }

    fn get(&self, key: &str) -> MessageId {
        self.threads[key].1
    }

    fn address(&self, key: &str) -> rho_dealer::Address {
        let (host, message) = self.threads[key];
        rho_chat::ThreadId {
            host: self.endpoints[host],
            message,
        }
        .address()
    }
}

fn seed(rig: &Path) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let rig = rig
        .canonicalize()
        .with_context(|| format!("rig {} must exist", rig.display()))?;
    let now = UnixMs::now();
    let mut ids = Ids::default();
    let runtime = runtime()?;
    let gpu_log = runtime.block_on(async {
        seed_lab(&rig, now, &mut ids).await?;
        seed_ci(&rig, now, &mut ids).await?;
        seed_gpu_box(&rig, now, &mut ids).await
    })?;
    drop(runtime);
    seed_device(&rig, now, &ids, gpu_log)?;
    let threads: serde_json::Map<String, serde_json::Value> = ids
        .threads
        .iter()
        .map(|(key, (host, id))| (key.clone(), json!({"host": host, "id": id.0.0})))
        .collect();
    let endpoints: serde_json::Map<String, serde_json::Value> = ids
        .endpoints
        .iter()
        .map(|(host, endpoint)| ((*host).to_owned(), json!(endpoint.to_string())))
        .collect();
    std::fs::write(
        rig.join("fixture.json"),
        serde_json::to_string_pretty(&json!({
            "seeded_at_ms": now.0,
            "endpoints": endpoints,
            "threads": threads,
        }))?,
    )?;
    eprintln!(
        "heavy_workday: seeded {} in {} ms",
        rig.display(),
        started.elapsed().as_millis()
    );
    Ok(())
}

/// A host's store, made fresh, and the iroh identity it will bind with.
async fn open_host(rig: &Path, host: &str) -> anyhow::Result<(rho_db::RhoDb, EndpointId)> {
    let path = rig.join(host).join("state/rho/rho.redb");
    anyhow::ensure!(!path.exists(), "{} must not exist", path.display());
    std::fs::create_dir_all(path.parent().context("store directory")?)?;
    let db = rho_db::RhoDb::open(path);
    rho_agent::db::prepare(&db).await;
    let endpoint = EndpointId(rho_rpc::server_endpoint_id(&db).await?);
    Ok((db, endpoint))
}

/// Leaves a store closed cleanly, so the host or window that opens it next
/// does not repair it first.
async fn close(db: rho_db::RhoDb) -> anyhow::Result<()> {
    let settled = db.settle().await?;
    drop(db);
    drop(settled);
    Ok(())
}

/// How many messages a host's log posts.
fn posted(log: &[ChatRecord]) -> usize {
    log.iter()
        .filter(|record| matches!(record.event, rho_agent_types::ChatEvent::Posted { .. }))
        .count()
}

/// The fixture's agents: ids from a domain no real host draws from.
fn agent(n: u64) -> AgentId {
    AgentId::from_counter(n, &AgentIdDomain(u64::MAX - 7)).expect("fixture agent id")
}

fn text(words: impl Into<String>) -> Vec<ContentPart> {
    vec![ContentPart::Text { text: words.into() }]
}

fn said(from: Address, words: impl Into<String>) -> NewMessage {
    NewMessage {
        from,
        voice: Voice::Message,
        content: text(words),
    }
}

/// A thread to write: its fixture key, its project, its subject, and its
/// turns (`true` for the user's).
type Exchange<'a> = (&'a str, MessageId, &'a str, &'a [(bool, &'a str)]);

/// One host's chat as the fixture writes it: every post at its own time.
struct Chat<'a> {
    tx: &'a mut rho_db::WriteTxn,
}

impl Chat<'_> {
    fn project(
        &mut self,
        at: UnixMs,
        name: &str,
        place: &Path,
        mode: ProjectMode,
    ) -> anyhow::Result<MessageId> {
        let root = self.tx.post(at, None, said(Address::User, name), None)?;
        self.tx.set_project(
            at,
            ProjectId(root),
            ProjectConfig {
                place: ProjectPlace::parse(&place.to_string_lossy()).map_err(anyhow::Error::msg)?,
                mode,
                runtime: RuntimeLabel::parse("sol-medium").map_err(anyhow::Error::msg)?,
            },
        );
        Ok(root)
    }

    /// The user's idea under `project`, named, with `worker` joined to it.
    fn idea(
        &mut self,
        at: UnixMs,
        project: MessageId,
        name: &str,
        words: &str,
        worker: AgentId,
    ) -> anyhow::Result<MessageId> {
        let thread = self
            .tx
            .post(at, Some(project), said(Address::User, words), None)?;
        self.tx.name(at, thread, name.to_owned())?;
        self.tx.join(at, thread, worker)?;
        Ok(thread)
    }

    fn post(
        &mut self,
        at: UnixMs,
        thread: MessageId,
        from: Address,
        words: &str,
    ) -> anyhow::Result<MessageId> {
        self.tx.post(at, Some(thread), said(from, words), None)
    }

    /// A thread of turns that alternate from the agent, ending at `end`.
    fn exchange(
        &mut self,
        end: UnixMs,
        thread: MessageId,
        worker: AgentId,
        turns: &[(bool, &str)],
    ) -> anyhow::Result<Vec<MessageId>> {
        let step = 4 * MINUTE;
        let start = end.0 - step * turns.len() as u64;
        let mut ids = Vec::new();
        for (index, (user, words)) in turns.iter().enumerate() {
            let at = UnixMs(start + step * (index as u64 + 1));
            let from = if *user {
                Address::User
            } else {
                Address::Agent(worker)
            };
            ids.push(self.post(at, thread, from, words)?);
        }
        Ok(ids)
    }
}

fn ago(now: UnixMs, ms: u64) -> UnixMs {
    UnixMs(now.0 - ms)
}

fn repo(rig: &Path, name: &str) -> PathBuf {
    rig.join("repos").join(name)
}

/// The long thread, oldest first: `(from the user, words)`. Twenty messages
/// with the idea; the newest is the ask the card deals.
const FLAKY_CLOCK: [(bool, &str); 19] = [
    (
        false,
        "I can reproduce it locally: 11 failures in 50 runs with eight test threads.\n\n```text\n$ cargo test -p tide clock -- --test-threads=8\n---- clock::tests::deadline_fires_once stdout ----\nthread 'clock::tests::deadline_fires_once' panicked at src/clock.rs:214:9:\nassertion `left == right` failed\n  left: 2\n right: 1\n```\n\nIt never fails with `--test-threads=1`.",
    ),
    (
        true,
        "> 11 failures in 50 runs with eight test threads.\n\nIs it the shared `SystemClock`? The retry test sleeps.",
    ),
    (
        false,
        "Yes. `deadline_fires_once` and `retry_backs_off` both read the process-wide clock, and the retry test moves it forward with `sleep`:\n\n```rust\nstatic CLOCK: LazyLock<SystemClock> = LazyLock::new(SystemClock::new);\n\nfn now() -> Instant {\n    CLOCK.now()\n}\n```\n\nWhen the two overlap, the deadline sees the retry's jump and fires twice.",
    ),
    (
        false,
        "Two ways out:\n\n1. Give each test its own `ManualClock` and pass it in.\n2. Serialize the clock tests with a mutex.\n\nThe first is more work, but the scheduler tests could use it too.",
    ),
    (
        true,
        "Do the first. Keep `SystemClock` as the default for the binary.",
    ),
    (
        false,
        "Started on the per-test clock. `Scheduler::new` now takes `impl Clock`; 14 call sites change, all in tests.",
    ),
    (
        false,
        "Draft PR: https://github.com/maan2003/tide/pull/412\n\nIt has the trait and the test changes. I have not touched `tide-sim` yet.",
    ),
    (
        true,
        "> I have not touched `tide-sim` yet.\n\nWhy does tide-sim need it?",
    ),
    (
        false,
        "`tide-sim` builds its own `Scheduler` with `SystemClock::new()` in `sim/src/world.rs:88`. With the new signature it has to choose a clock. I passed `ManualClock` so the simulation stays deterministic.",
    ),
    (
        false,
        "CI on the draft: 3 of 4 jobs pass. The macOS job times out in `io::tests::slow_pipe`, which also fails on main: https://github.com/maan2003/tide/actions/runs/118790",
    ),
    (true, "That one is known. Ignore macOS for now."),
    (
        false,
        "Ran the clock tests 500 times on eight threads:\n\n```text\ntest result: ok. 500 passed; 0 failed; 0 ignored; finished in 41.3s\n```\n\nBefore the change it was 412 passed, 88 failed.",
    ),
    (
        false,
        "One more finding: `retry_backs_off` asserted on wall time with a 5 ms slack. With `ManualClock` the slack is gone; the assertion is exact now.",
    ),
    (
        true,
        "Good. What about the benchmarks in `benches/clock.rs`?",
    ),
    (
        false,
        "They use `SystemClock` on purpose, to measure the real cost. I left them alone and added a comment that says why.",
    ),
    (
        false,
        "Rebased #412 on main after the scheduler refactor landed. Two conflicts in `scheduler.rs`, both in imports.",
    ),
    (
        true,
        "> Rebased #412 on main\n\nRun the whole suite once more before you ask me.",
    ),
    (
        false,
        "Full suite on the rebased branch: 1,284 passed, 0 failed, 3 ignored. The macOS job still times out in `slow_pipe` only.",
    ),
    (
        false,
        "Both clock tests pass 500/500 with a per-test `ManualClock`, and CI is green except the known macOS timeout.\n\nShall I merge https://github.com/maan2003/tide/pull/412 now, or keep the old clock behind a `legacy-clock` feature for one release?",
    ),
];

/// Subjects and words of the settled day, by project. Each thread ends with
/// the user's last word or a done verdict on the device.
const CORPUS: [(&str, &str, &[&str]); 26] = [
    (
        "tide",
        "Watermark lag after compaction",
        &[
            "Compaction rewrites the segment index, and the watermark only moves after the rewrite commits.",
            "So the lag is the rewrite time?",
            "Yes: 1.8 s on the large fixture. Moving the watermark before the fsync would lose data on a crash.",
            "Keep it. Write it down in the compaction doc.",
            "Added a section on watermark lag to docs/compaction.md.",
        ],
    ),
    (
        "tide",
        "Tokio runtime in the bench harness",
        &[
            "The bench harness builds a multi-thread runtime per iteration, which costs 3 ms each time.",
            "Build it once per benchmark group.",
            "Done. The scheduler benches are 11% faster now and the variance is lower.",
            "Thanks, merged.",
        ],
    ),
    (
        "tide",
        "Monotonic deadline for timers",
        &[
            "Timers compare deadlines with `SystemTime`, which jumps with NTP.",
            "Use `Instant` everywhere it is a deadline.",
            "Changed 9 sites. Two log lines still print wall time, which is what we want there.",
            "Good.",
        ],
    ),
    (
        "tide",
        "Segment eviction under memory pressure",
        &[
            "Eviction picks the oldest segment, even when it is pinned by a reader.",
            "Skip pinned segments and try the next one.",
            "Done, with a test that pins the oldest segment and checks the second goes.",
            "Merged.",
        ],
    ),
    (
        "tide",
        "Backpressure on the ingest queue",
        &[
            "The ingest queue is unbounded; under load it grew to 2.1 GB.",
            "Bound it at 64k items and block the producer.",
            "Bounded with a semaphore. Throughput drops 3% at the bound, memory stays under 300 MB.",
            "Ship it.",
        ],
    ),
    (
        "tide",
        "Clock skew in the replication test",
        &[
            "The replication test fakes a 2 s skew between nodes but asserts ordering by wall time.",
            "Assert on sequence numbers instead.",
            "Changed. The test no longer depends on the skew at all.",
        ],
    ),
    (
        "rho",
        "Desk row truncation on narrow windows",
        &[
            "Long subjects push the project address off the right edge below 600 px.",
            "Cut the subject at a word, keep the address.",
            "Implemented with the existing word cutter. The phone Desk keeps the address on the first line.",
            "Looks right on my phone.",
        ],
    ),
    (
        "rho",
        "Places ranking for exact project names",
        &[
            "Typing `tide` ranks `Tide clock notes` above the project `tide@lab`.",
            "An exact project name should win.",
            "Exact names now rank first; prefixes next. Added the case to the ranking tests.",
            "Thanks.",
        ],
    ),
    (
        "rho",
        "Notes search misses edited text",
        &[
            "The note index keeps the first revision only, so edited words do not match.",
            "Index the newest revision.",
            "Fixed; the index follows each revision now. A note edited twice matches its latest words only.",
        ],
    ),
    (
        "rho",
        "Echo line clears too early",
        &[
            "The echo clears on the next key press instead of after four seconds.",
            "It should stay for its full life unless a prompt opens.",
            "Fixed. A prompt still takes it away at once.",
            "Good.",
        ],
    ),
    (
        "rho",
        "Ledger sync after a long sleep",
        &[
            "After a laptop sleeps overnight, the ledger stream reconnects but skips the first record.",
            "Can you reproduce it?",
            "Yes, by dropping the connection between the length check and the read. The read now starts from the stored offset.",
            "Merged, thanks.",
        ],
    ),
    (
        "rho",
        "Transcript fold for repeated errors",
        &[
            "Twenty identical retry errors fill the transcript.",
            "Fold repeats into one line with a count.",
            "Done: `connection refused ×20`. The first and last times stay visible.",
            "Nice.",
        ],
    ),
    (
        "rho",
        "Idea draft keeps the old runtime",
        &[
            "After switching the project to `sol-high`, an open idea draft still shows `opus-high`.",
            "An untouched runtime should follow the project.",
            "Fixed; an explicit choice in the draft is still kept.",
        ],
    ),
    (
        "rho",
        "Status line lamp flickers on reconnect",
        &[
            "When a host reconnects, the lamp turns off and on within one frame.",
            "Hold the lamp while the host is connecting.",
            "Fixed by keeping the cached waiting count until the host's first frame.",
            "Good.",
        ],
    ),
    (
        "infra",
        "Rotate the CI deploy key",
        &[
            "The deploy key for the CI runner expires on Friday.",
            "Rotate it and update the secret store.",
            "Rotated. The old key is revoked and the runner pulled with the new one.",
            "Thanks.",
        ],
    ),
    (
        "infra",
        "Disk usage on the build cache",
        &[
            "The build cache volume is at 91%.",
            "Prune everything older than 14 days.",
            "Pruned 212 GB. Usage is 54% now. I added a weekly prune timer.",
            "Good.",
        ],
    ),
    (
        "infra",
        "Postgres minor upgrade",
        &[
            "Postgres 16.4 is out with a fix for the vacuum stall we saw.",
            "Upgrade staging first.",
            "Staging is on 16.4 and has run vacuum twice without a stall.",
            "Do production on Thursday.",
        ],
    ),
    (
        "infra",
        "Alert on certificate expiry",
        &[
            "Nothing alerts before a certificate expires.",
            "Alert at 14 days.",
            "Added a probe and an alert rule. It fired once on staging, which has 9 days left.",
            "That is the one stuck now; leave the alert as it is.",
        ],
    ),
    (
        "infra",
        "Log retention for the gateway",
        &[
            "Gateway logs are kept for 90 days, which is 1.4 TB.",
            "30 days is enough.",
            "Changed to 30 days. The old indexes go in the next nightly cleanup.",
        ],
    ),
    (
        "tide",
        "Flamegraph of the compaction path",
        &[
            "Most of compaction is spent in `crc32` over the whole segment.",
            "Can we checksum per block?",
            "Yes; per-block checksums cut compaction time by 37% on the large fixture.",
            "Merged.",
        ],
    ),
    (
        "rho",
        "Wayland capture for the GUI tests",
        &[
            "The GUI tests need screenshots without a real display.",
            "Use the headless compositor from rho wayland.",
            "The tests run in a named session now and save a PNG per state.",
            "Great.",
        ],
    ),
    (
        "rho",
        "Card age words in the deal",
        &[
            "The deal says `waits 59m` and then `waits 1.0h` a minute later.",
            "That is the card age step. Keep it.",
            "Kept, with a test for the boundary.",
        ],
    ),
    (
        "tide",
        "Snapshot isolation for readers",
        &[
            "Readers can see a half-written batch when compaction swaps segments.",
            "Take the snapshot before the swap.",
            "Readers hold a snapshot now; the swap waits for none of them.",
            "Ship it.",
        ],
    ),
    (
        "infra",
        "Metrics volume IOPS",
        &[
            "The metrics volume hits its IOPS limit at night.",
            "Move it to a gp3 volume with 6000 IOPS.",
            "The migration is scheduled; see the stopped thread for the first attempt.",
            "OK.",
        ],
    ),
    (
        "rho",
        "Keyboard map for the notes list",
        &[
            "`o` and `O` both start a note; `i` edits the selected one.",
            "Document it in the notes help.",
            "Documented.",
        ],
    ),
    (
        "tide",
        "Release notes for 0.9",
        &[
            "Drafted release notes for 0.9: per-test clocks, bounded ingest, block checksums.",
            "Mention the breaking `Scheduler::new` change first.",
            "Moved it to the top with a migration example.",
            "Publish them with the tag.",
        ],
    ),
];

async fn seed_lab(rig: &Path, now: UnixMs, ids: &mut Ids) -> anyhow::Result<()> {
    let (db, endpoint) = open_host(rig, "lab").await?;
    ids.endpoints.insert("lab", endpoint);
    let day = ago(now, 8 * HOUR);
    let mut tx = db.write().await;
    tx.init_store_tables();
    let mut chat = Chat { tx: &mut tx };
    let tide = chat.project(day, "tide", &repo(rig, "tide"), ProjectMode::One)?;
    let rho = chat.project(day, "rho", &repo(rig, "rho"), ProjectMode::One)?;
    let infra = chat.project(day, "infra", &repo(rig, "infra"), ProjectMode::One)?;
    for (key, id) in [("lab/tide", tide), ("lab/rho", rho), ("lab/infra", infra)] {
        ids.thread(key, "lab", id);
    }
    let projects = BTreeMap::from([("tide", tide), ("rho", rho), ("infra", infra)]);

    // The settled day: what the user already dealt with.
    let mut corpus_messages = 0;
    let mut settled = Vec::new();
    for (index, (project, subject, turns)) in CORPUS.iter().enumerate() {
        let worker = agent(100 + index as u64);
        let opened = ago(now, 8 * HOUR - index as u64 * 15 * MINUTE);
        let thread = chat.idea(
            opened,
            projects[project],
            subject,
            &format!("{subject}: look into it and tell me what you find."),
            worker,
        )?;
        let mut at = opened.0 + MINUTE;
        chat.post(
            UnixMs(at),
            thread,
            Address::Agent(worker),
            &format!("Picked up `{subject}`. Reading the code and the last week of CI around it."),
        )?;
        let mut last = thread;
        for (turn, words) in turns.iter().enumerate() {
            at += 3 * MINUTE + (turn as u64 * 37_000);
            let from = if turn % 2 == 0 {
                Address::Agent(worker)
            } else {
                Address::User
            };
            last = chat.post(UnixMs(at), thread, from, words)?;
        }
        corpus_messages += turns.len() + 2;
        // An agent's last word waits until the user says done.
        if turns.len() % 2 == 1 {
            settled.push(format!("lab/settled/{index}"));
            ids.thread(&format!("lab/settled/{index}"), "lab", thread);
            ids.thread(&format!("lab/settled/{index}/last"), "lab", last);
        }
    }

    // The long thread.
    let clock_worker = agent(1);
    let opened = ago(now, 6 * HOUR + 40 * MINUTE);
    let flaky = chat.idea(
        opened,
        tide,
        "Flaky clock test",
        "The clock test in tide fails about one run in five on CI. Find out why and fix it.",
        clock_worker,
    )?;
    ids.thread("lab/flaky", "lab", flaky);
    let mut flaky_ids = Vec::new();
    let span = 6 * HOUR + 40 * MINUTE - 9 * MINUTE;
    for (index, (user, words)) in FLAKY_CLOCK.iter().enumerate() {
        let at = UnixMs(opened.0 + span * (index as u64 + 1) / FLAKY_CLOCK.len() as u64);
        let from = if *user {
            Address::User
        } else {
            Address::Agent(clock_worker)
        };
        let id = chat.post(at, flaky, from, words)?;
        flaky_ids.push(id);
        // The draft PR's first words named the wrong file; the agent fixed
        // them before anyone answered.
        if index == 8 {
            chat.tx.edit(
                UnixMs(at.0 + 2 * MINUTE),
                clock_worker,
                id,
                text(
                    "`tide-sim` builds its own `Scheduler` with `SystemClock::new()` in `sim/src/world.rs:88` (not `sim/src/lib.rs`, as I first wrote). With the new signature it has to choose a clock. I passed `ManualClock` so the simulation stays deterministic.",
                ),
            )?;
        }
    }
    ids.thread("lab/flaky/ask", "lab", *flaky_ids.last().unwrap());
    // The message the private reply answers.
    ids.thread("lab/flaky/sim", "lab", flaky_ids[8]);
    chat.tx.post_flash(
        ago(now, 2 * MINUTE),
        flaky,
        clock_worker,
        text("Watching the macOS job on #412 while you decide; slow_pipe timed out again."),
    )?;

    // Asks and results that wait.
    let waiting: [Exchange; 6] = [
        (
            "lab/retry-budget",
            tide,
            "Retry budget for the scheduler",
            &[
                (
                    false,
                    "Retries share no budget: a flapping peer can take every slot.",
                ),
                (true, "Cap it per peer."),
                (
                    false,
                    "A per-peer cap of 8 works in the soak test. Should the cap also apply to the first attempt, or only to retries?",
                ),
            ],
        ),
        (
            "lab/tick-source",
            tide,
            "Swap the tick source",
            &[(
                false,
                "The tick source now reads `CLOCK_MONOTONIC_COARSE`; ticks cost 18 ns instead of 61 ns.\n\n## proof\n```text\ntick/coarse   time: [17.9 ns 18.1 ns 18.4 ns]\ntick/precise  time: [60.7 ns 61.2 ns 61.9 ns]\n```\n\nPR: https://github.com/maan2003/tide/pull/415",
            )],
        ),
        (
            "lab/desk-wording",
            rho,
            "Desk overflow wording",
            &[
                (
                    false,
                    "The overflow row read `+2`, which nobody understood in the review.",
                ),
                (true, "Use words."),
                (
                    false,
                    "It reads `and 2 more · clock` now, naming the first hidden project. Screenshots are on https://github.com/maan2003/rho/pull/2291. Ready to merge?",
                ),
            ],
        ),
        (
            "lab/search-latency",
            rho,
            "Search latency on 10k messages",
            &[
                (
                    false,
                    "Full-text search over 10,000 messages takes 140 ms per key press, which drops frames.",
                ),
                (
                    false,
                    "With a trigram index it is 3 ms. The index costs 4 MB for 10k messages.\n\n> 140 ms per key press\n\nis now under one frame. Should I land the index, or try the cheaper prefix index first?",
                ),
            ],
        ),
        (
            "lab/notes-keys",
            rho,
            "Notes list keyboard map",
            &[(
                false,
                "The notes list binds `o`, `O` and `i`, but `space` opens the menu like everywhere else. Do you want `d` to delete from the list, or only from the note itself?",
            )],
        ),
        (
            "lab/cert-rotation",
            infra,
            "Rotate the staging certificates",
            &[(
                false,
                "Starting the rotation; staging has 9 days left on its certificate.",
            )],
        ),
    ];
    for (offset, (key, project, subject, turns)) in waiting.iter().enumerate() {
        let worker = agent(10 + offset as u64);
        let opened = ago(now, 3 * HOUR - offset as u64 * 20 * MINUTE);
        let thread = chat.idea(opened, *project, subject, &format!("{subject}."), worker)?;
        chat.exchange(
            ago(now, 25 * MINUTE + offset as u64 * 9 * MINUTE),
            thread,
            worker,
            turns,
        )?;
        ids.thread(key, "lab", thread);
    }

    // Three stops in `infra`: the certificate rotation got stuck, an image
    // bump failed, and a volume migration was lost with its process.
    let stops: [(&str, Option<&str>, &str, Stop, &str); 3] = [
        (
            "lab/cert-rotation",
            None,
            "",
            Stop::Stuck,
            "stuck · can't reach its model: error sending request for url (https://api.openai.com/v1/responses): connection refused ×14\n\nLast transcript lines:\n    $ kubectl -n staging get certificate gateway-tls\n    gateway-tls   True    9d",
        ),
        (
            "lab/base-image",
            Some("Bump the base image"),
            "Bump the base image to debian 13 and rebuild the runners.",
            Stop::Failed,
            "failed · the model refused the request: context window exceeded (412,118 of 400,000 tokens) ×3\n\nLast transcript lines:\n    $ docker build -t runner:trixie .\n    #14 ERROR: failed to solve: process \"/bin/sh -c apt-get install -y libssl1.1\" did not complete successfully: exit code: 100",
        ),
        (
            "lab/metrics-volume",
            Some("Migrate the metrics volume"),
            "Migrate the metrics volume to gp3 with 6000 IOPS.",
            Stop::Lost,
            "lost · its process ended without a word (signal 9)\n\nLast transcript lines:\n    $ rsync -a /srv/metrics/ /mnt/gp3/metrics/\n    sent 41.2G bytes  received 1.1M bytes",
        ),
    ];
    for (offset, (key, subject, words, stop, body)) in stops.iter().enumerate() {
        let worker = agent(20 + offset as u64);
        let thread = match subject {
            Some(subject) => {
                let thread = chat.idea(
                    ago(now, 2 * HOUR + offset as u64 * 10 * MINUTE),
                    infra,
                    subject,
                    words,
                    worker,
                )?;
                ids.thread(key, "lab", thread);
                thread
            }
            None => ids.get(key),
        };
        let since = ago(now, 50 * MINUTE - offset as u64 * 7 * MINUTE);
        chat.tx.post_stop(
            UnixMs(since.0 + 3 * MINUTE),
            thread,
            Address::System {
                agent: worker,
                stop: *stop,
                since,
            },
            (*body).to_owned(),
        )?;
    }

    // A start the host refused: the project's place moved.
    let refused = chat.tx.post(
        ago(now, 70 * MINUTE),
        Some(rho),
        said(
            Address::User,
            "Profile the GUI frame drop when the Desk has 40 cards.",
        ),
        None,
    )?;
    chat.tx.name(
        ago(now, 70 * MINUTE),
        refused,
        "Profile the GUI frame drop".into(),
    )?;
    chat.post(
        ago(now, 70 * MINUTE - 2_000),
        refused,
        Address::StartFailed,
        &format!(
            "failed · did not start · {} on lab: runtime refused: `opus-max` is not offered on this host; try opus-high\n\nreply to retry, or change where it works with set project place",
            repo(rig, "rho").display()
        ),
    )?;
    ids.thread("lab/refused", "lab", refused);

    // A muted ask and a snoozed one: both wait, neither is on the Desk.
    for (key, project, subject, ask, worker) in [
        (
            "lab/muted",
            rho,
            "Bump the vendored Zed",
            "Zed main has 214 new commits since our subtree. Pull them now, or wait for their release branch?",
            agent(30),
        ),
        (
            "lab/snoozed",
            tide,
            "Daylight saving edge",
            "The scheduler skips a 02:30 job on the spring-forward day. Run it at 03:00 instead, or skip it as cron does?",
            agent(31),
        ),
    ] {
        let thread = chat.idea(
            ago(now, 5 * HOUR),
            project,
            subject,
            &format!("{subject}."),
            worker,
        )?;
        chat.post(ago(now, 4 * HOUR), thread, Address::Agent(worker), ask)?;
        ids.thread(key, "lab", thread);
    }

    seed_usage(&mut tx, now);
    tx.commit();
    let messages = posted(&db.read().mail_since(ChatSeq::ZERO, usize::MAX));
    close(db).await?;
    eprintln!(
        "heavy_workday: lab posts {messages} messages ({corpus_messages} in the settled corpus, {} of those threads done on the device)",
        settled.len()
    );
    Ok(())
}

/// Thirty days of Claude allowance on two accounts and of spend by model.
fn seed_usage(tx: &mut rho_db::WriteTxn, now: UnixMs) {
    tx.set_claude_account("work");
    let week = 7 * DAY;
    // Each account resets weekly; `work` resets in 4.2 days with 74% left,
    // `home` in 1.3 days with 12% left.
    for (account, reset_in, used_now) in
        [("work", 42 * DAY / 10, 26u8), ("home", 13 * DAY / 10, 88u8)]
    {
        let next_reset = now.0 + reset_in;
        let mut at = now.0 - 30 * DAY;
        while at <= now.0 {
            // The end of the window `at` is in, and how far into it.
            let reset = at + (next_reset - at) % week;
            let elapsed = week - (reset - at);
            let used = (u64::from(used_now) * elapsed / (week - reset_in)).min(100) as u8;
            for model in [QuotaModel::OPUS, QuotaModel::FABLE] {
                let used = if model == QuotaModel::FABLE {
                    used / 2
                } else {
                    used
                };
                tx.record_quota_observation(QuotaObservationRecord {
                    provider: QuotaProvider::Claude,
                    model,
                    auth_namespace: Some(account.to_owned()),
                    observed_at: UnixMs(at),
                    used_percent: used,
                    reset_at_unix: Some((reset / 1000) as i64),
                });
            }
            at += 3 * HOUR;
        }
        tx.record_quota_observation(QuotaObservationRecord {
            provider: QuotaProvider::Claude,
            model: QuotaModel::OPUS,
            auth_namespace: Some(account.to_owned()),
            observed_at: now,
            used_percent: used_now,
            reset_at_unix: Some(((next_reset) / 1000) as i64),
        });
    }
    // Spend: a few agents a day on each model, busiest in working hours.
    let models = [
        (AgentUsageModel::OPUS, 900_000u64),
        (AgentUsageModel::GPT, 1_400_000),
        (AgentUsageModel::FABLE, 300_000),
        (AgentUsageModel::TERRA, 200_000),
    ];
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let start = (now.0 - 30 * DAY) / HOUR * HOUR;
    let mut hour = start;
    while hour < now.0 {
        let hour_of_day = (hour / HOUR) % 24;
        let busy = (8..19).contains(&hour_of_day);
        for (slot, (model, scale)) in models.iter().enumerate() {
            if !busy && next() % 4 != 0 {
                continue;
            }
            let day_index = (hour - start) / DAY;
            let agent_id = agent(1_000 + day_index * 8 + slot as u64 * 2 + next() % 2);
            let weight = 1 + next() % 5;
            let input = scale * weight / 4;
            tx.add_agent_usage(
                agent_id,
                &AgentUsageBucket {
                    bucket_start_ms: hour + (next() % 12) * AGENT_USAGE_BUCKET_MS,
                    model: *model,
                    input_tokens: input / 8,
                    cache_read_tokens: input,
                    cache_write_tokens: input / 20,
                    cache_write_1h_tokens: 0,
                    output_tokens: input / 30,
                    requests: 4 + next() % 20,
                    approximate: false,
                },
            );
        }
        hour += HOUR;
    }
}

async fn seed_ci(rig: &Path, now: UnixMs, ids: &mut Ids) -> anyhow::Result<()> {
    let (db, endpoint) = open_host(rig, "ci").await?;
    ids.endpoints.insert("ci", endpoint);
    let mut tx = db.write().await;
    tx.init_store_tables();
    let mut chat = Chat { tx: &mut tx };
    // An agent per idea, so the checks and the bisect each have their own.
    let nightly = chat.project(
        ago(now, 8 * HOUR),
        "nightly",
        &repo(rig, "nightly"),
        ProjectMode::PerIdea,
    )?;
    ids.thread("ci/nightly", "ci", nightly);
    let worker = agent(40);
    let thread = chat.idea(
        ago(now, 7 * HOUR),
        nightly,
        "Last night's acceptance run",
        "Summarize last night's acceptance run.",
        worker,
    )?;
    chat.exchange(
        ago(now, 6 * HOUR),
        thread,
        worker,
        &[
            (false, "Last night: 412 of 414 acceptance checks passed. Both failures are `upload_resumes_after_drop`, which times out on the shared runner."),
            (true, "Known flake; it is quarantined from tonight."),
        ],
    )?;
    tx.commit();
    close(db).await
}

async fn seed_gpu_box(rig: &Path, now: UnixMs, ids: &mut Ids) -> anyhow::Result<Vec<ChatRecord>> {
    let (db, endpoint) = open_host(rig, "gpu-box").await?;
    ids.endpoints.insert("gpu-box", endpoint);
    let mut tx = db.write().await;
    tx.init_store_tables();
    let mut chat = Chat { tx: &mut tx };
    let tide = chat.project(
        ago(now, 9 * HOUR),
        "tide",
        Path::new("/srv/tide"),
        ProjectMode::One,
    )?;
    let train = chat.project(
        ago(now, 9 * HOUR),
        "train",
        Path::new("/srv/train"),
        ProjectMode::PerIdea,
    )?;
    ids.thread("gpu-box/tide", "gpu-box", tide);
    ids.thread("gpu-box/train", "gpu-box", train);
    let threads: [Exchange; 4] = [
        (
            "gpu-box/clock-soak",
            tide,
            "Clock soak on the A100s",
            &[
                (
                    false,
                    "The clock soak ran 6 hours on four A100 nodes: no deadline fired twice, but p99 tick jitter is 2.4 ms under full GPU load.",
                ),
                (
                    false,
                    "Is 2.4 ms acceptable for the scheduler, or should I pin the tick thread to an isolated core and rerun?",
                ),
            ],
        ),
        (
            "gpu-box/warmup",
            train,
            "Sweep the warmup schedule",
            &[(
                false,
                "Sweep done.\n\n| warmup steps | loss @ 10k | diverged |\n|---|---|---|\n| 500 | 2.91 | 2 of 4 |\n| 2000 | 2.74 | 0 of 4 |\n| 4000 | 2.76 | 0 of 4 |\n\n2000 steps is the best trade-off. Report: https://github.com/maan2003/train/pull/77",
            )],
        ),
        (
            "gpu-box/checkpoint",
            train,
            "Checkpoint upload stalls",
            &[
                (false, "Checkpoint uploads stall at 99% about once a day."),
                (true, "Is it the multipart complete call?"),
                (
                    false,
                    "Yes: the complete call has no retry, and the bucket returns 503 under load. Add a retry with backoff there, or move to the resumable upload API?",
                ),
            ],
        ),
        (
            "gpu-box/nccl",
            train,
            "NCCL timeout on node 3",
            &[
                (
                    false,
                    "Node 3 dropped out of the ring twice with `NCCL WARN Net : Connection closed by remote peer`.",
                ),
                (true, "Drain node 3 and rerun on three nodes."),
                (
                    false,
                    "Drained. The rerun on three nodes is stable for 4 hours.",
                ),
                (true, "Good, leave it drained until the NIC is swapped."),
            ],
        ),
    ];
    for (offset, (key, project, subject, turns)) in threads.iter().enumerate() {
        let worker = agent(50 + offset as u64);
        let thread = chat.idea(
            ago(now, 7 * HOUR - offset as u64 * 30 * MINUTE),
            *project,
            subject,
            &format!("{subject}."),
            worker,
        )?;
        chat.exchange(
            // The host went away about two hours ago.
            ago(now, 2 * HOUR + 10 * MINUTE + offset as u64 * 25 * MINUTE),
            thread,
            worker,
            turns,
        )?;
        ids.thread(key, "gpu-box", thread);
    }
    tx.commit();
    let log = db.read().mail_since(ChatSeq::ZERO, usize::MAX);
    close(db).await?;
    eprintln!("heavy_workday: gpu-box posts {} messages", posted(&log));
    // Where the device finds the host that is away: its runtime directory
    // says who it was, and no host listens there.
    let runtime = rig.join("gpu-box/runtime");
    std::fs::create_dir_all(&runtime)?;
    std::fs::write(runtime.join("endpoint"), format!("{endpoint}\n"))?;
    Ok(log)
}

/// Fifteen notes, title first, as the user wrote them through the day.
const NOTES: [&str; 15] = [
    "Clock test plan\n\nPer-test ManualClock everywhere a test reads time. SystemClock stays the default in the binary. Check tide-sim before merging #412.",
    "Questions for Thursday's sync\n\n- who owns the macOS runner?\n- do we keep legacy-clock for one release?\n- metrics volume migration window",
    "Watermark lag numbers\n\n1.8 s on the large fixture, 0.2 s on the small one. Do not move the watermark before fsync.",
    "Release 0.9 checklist\n\n1. merge #412 and #415\n2. release notes: Scheduler::new breaking change first\n3. tag and publish",
    "gpu-box NIC swap\n\nNode 3 stays drained until the NIC is replaced. Ticket with the colo: #88213.",
    "Search latency idea\n\nTrigram index: 3 ms per key press at 10k messages, 4 MB. Try a prefix index first if memory matters on the phone.",
    "Staging certificates\n\nGateway TLS expires in 9 days. The rotation agent is stuck; rotate by hand if it is not back by Monday.",
    "Interview loop notes\n\nSystems design: ask about backpressure and bounded queues. Coding: the monotonic deadline exercise.",
    "Retry budget\n\nPer-peer cap of 8 survives the soak. Open question: does the first attempt count against it?",
    "Things to read\n\n- the Zed release branch notes\n- the tokio coop budget post\n- NCCL troubleshooting guide",
    "Base image\n\nlibssl1.1 is gone in trixie; the runner Dockerfile needs libssl3. Context window blew up on the build log.",
    "Desk wording\n\n`and 2 more · clock` reads well. Keep `0 wait · lab away` calm, no red.",
    "Ideas backlog\n\n- per-project dealer focus\n- note reminders on the Desk\n- push approval in the thread",
    "Ingest backpressure\n\nBounded at 64k items: -3% throughput, memory under 300 MB. Watch the p99 after the deploy.",
    "Daylight saving\n\nSpring-forward 02:30 job: cron skips it. Decide before March.",
];

fn seed_device(rig: &Path, now: UnixMs, ids: &Ids, gpu_log: Vec<ChatRecord>) -> anyhow::Result<()> {
    let state = rig.join("device/state/rho");
    anyhow::ensure!(
        !rho_db::client::path(&state).exists(),
        "the device must be new"
    );
    let db = rho_db::client::open(&state)?;
    // The copy the device kept of gpu-box before it went away, and the
    // reply the user started on the long thread.
    let cache = rho_chat::cache::Cache::open_on(db.clone())?;
    cache.write_records(ids.endpoints["gpu-box"], gpu_log);
    cache.write_draft(
        ids.endpoints["lab"],
        Some(ids.get("lab/flaky")),
        "Merge it, but keep the old clock behind `legacy-clock` for one release.\nAnd add a line about ManualClock to the 0.9 release notes.".into(),
    );
    cache.flush();
    drop(cache);

    let ledger = futures::executor::block_on(rho_ledger::Ledger::open(db.clone()));
    futures::executor::block_on(ledger.set_secret(rho_ledger::Secret::generate()))?;
    let device = Device(ledger.device().0);
    let zone = jiff::tz::TimeZone::system();
    let mut at = now.0 - 8 * HOUR;
    let mut entries = Vec::new();
    let mut say = |offset: u64, fact: Fact| {
        at = (now.0 - offset).max(at + 1);
        entries.push(
            Entry {
                device,
                at: jiff::Timestamp::from_millisecond(at as i64)
                    .expect("a fixture instant")
                    .to_zoned(zone.clone()),
                fact,
            }
            .encode(),
        );
    };
    // Done on the corpus threads the agent had the last word in.
    for (key, _) in ids.threads.range("lab/settled/".to_owned()..) {
        let Some(index) = key.strip_prefix("lab/settled/") else {
            break;
        };
        if index.contains('/') {
            continue;
        }
        let last = ids.get(&format!("{key}/last"));
        say(
            7 * HOUR,
            Fact::Settled {
                node: ids.address(key),
                seen: Seen::Topic(last.0.0),
            },
        );
    }
    for (index, words) in NOTES.iter().enumerate() {
        let note = uuid::Uuid::from_u128(0x5eed_0000_0000_0000_0000_0000_0000_0000 | index as u128);
        let written = now.0 - 7 * HOUR + index as u64 * 25 * MINUTE;
        say(
            now.0 - written,
            Fact::Wrote {
                note,
                text: (*words).to_owned(),
                created: jiff::Timestamp::from_millisecond(written as i64)?,
            },
        );
    }
    say(
        3 * HOUR,
        Fact::Mute {
            node: ids.address("lab/muted"),
        },
    );
    say(
        65 * MINUTE,
        Fact::Snooze {
            node: ids.address("lab/snoozed"),
            until: rho_dealer::until::Until::In(jiff::SignedDuration::from_hours(3)),
        },
    );
    // The private reply under the agent's word about tide-sim.
    let private = uuid::Uuid::from_u128(0x9e1_0000_0000_0000_0000_0000_0000_0000);
    say(
        40 * MINUTE,
        Fact::About {
            node: rho_dealer::Address::note(private),
            about: Some(ids.address("lab/flaky/sim")),
        },
    );
    say(
        40 * MINUTE - 1,
        Fact::Wrote {
            note: private,
            text: "Check whether tide-bench also builds its own SystemClock before #412 merges."
                .into(),
            created: jiff::Timestamp::from_millisecond((now.0 - 40 * MINUTE) as i64)?,
        },
    );
    let facts = entries.len();
    futures::executor::block_on(ledger.append(rho_ledger::Channel::Facts, entries));
    drop(ledger);
    futures::executor::block_on(close(db))?;
    eprintln!(
        "heavy_workday: the device holds {facts} facts and {} notes",
        NOTES.len() + 1
    );
    Ok(())
}

fn fixture(rig: &Path) -> anyhow::Result<serde_json::Value> {
    let text = std::fs::read_to_string(rig.join("fixture.json"))
        .context("seed the rig first: fixture.json is missing")?;
    Ok(serde_json::from_str(&text)?)
}

fn fixture_thread(fixture: &serde_json::Value, key: &str) -> anyhow::Result<MessageId> {
    fixture["threads"][key]["id"]
        .as_u64()
        .map(|id| MessageId(ChatSeq(id)))
        .with_context(|| format!("fixture.json names no thread {key}"))
}

fn host_paths(rig: &Path, host: &str) -> anyhow::Result<RuntimePaths> {
    RuntimePaths::new(Some(rig.join(host).join("runtime")))
}

fn client_id() -> u64 {
    uuid::Uuid::new_v4().as_u64_pair().0
}

async fn eval(paths: &RuntimePaths, workset: WorksetId, code: String) -> anyhow::Result<String> {
    let result = call(paths, Eval { workset, code }).await?;
    anyhow::ensure!(result.error.is_none(), "{:?}", result.error);
    Ok(result.output.trim().to_string())
}

/// The messages under `thread`, oldest first, as the host's Python reads
/// them.
async fn read_thread(
    paths: &RuntimePaths,
    workset: WorksetId,
    thread: MessageId,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let out = eval(
        paths,
        workset,
        format!(
            "import json\nfrom rho import chat\nprint(json.dumps(chat.read({}, 100), default=str))",
            thread.0.0
        ),
    )
    .await?;
    Ok(serde_json::from_str(out.lines().last().unwrap_or("[]"))?)
}

async fn until<T>(
    what: &str,
    timeout: Duration,
    mut probe: impl AsyncFnMut() -> anyhow::Result<Option<T>>,
) -> anyhow::Result<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(found) = probe().await? {
            return Ok(found);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn post_idea(
    paths: &RuntimePaths,
    project: MessageId,
    words: &str,
) -> anyhow::Result<MessageId> {
    match call(
        paths,
        rho_chat::protocol::PostIdea {
            client_id: client_id(),
            project: ProjectId(project),
            runtime: None,
            mode: None,
            content: text(words),
            host_label: "ci".into(),
        },
    )
    .await?
    {
        rho_chat::protocol::PostIdeaReply::Posted(id) => Ok(id),
        rho_chat::protocol::PostIdeaReply::RuntimeRefused(why) => {
            anyhow::bail!("ci refused the idea's runtime: {why}")
        }
    }
}

/// `ci`'s day: real commands, then its model goes away mid-thread.
async fn ci_day(rig: &Path, model_pid: i32) -> anyhow::Result<()> {
    let rig = rig.canonicalize()?;
    let fixture = fixture(&rig)?;
    let nightly = fixture_thread(&fixture, "ci/nightly")?;
    let paths = host_paths(&rig, "ci")?;
    // A workset of the fixture's own, to read the chat with.
    let helper = call(
        &paths,
        CreateWorkset {
            kind: WorksetKind::Host {
                cwd: repo(&rig, "nightly").to_string_lossy().into_owned().into(),
            },
            code: String::new(),
        },
    )
    .await?
    .workset;

    let checks = post_idea(
        &paths,
        nightly,
        "Run the nightly acceptance checks on main and send me the proof.",
    )
    .await?;
    let proof = until(
        "the acceptance proof",
        Duration::from_secs(60),
        async || {
            Ok(read_thread(&paths, helper, checks)
                .await?
                .into_iter()
                .find(|mail| mail.to_string().contains("rho-e2e-step-")))
        },
    )
    .await?;
    let proof_id = proof["id"].as_u64().context("the proof's id")?;
    // The model behind ci goes away; the next request can only retry.
    anyhow::ensure!(
        unsafe { libc::kill(model_pid, libc::SIGTERM) } == 0,
        "could not stop ci's model (pid {model_pid})"
    );
    call(
        &paths,
        rho_chat::protocol::Post {
            client_id: client_id(),
            parent: Some(checks),
            settles: Some(rho_agent_types::MessageVersion {
                message: MessageId(ChatSeq(proof_id)),
                version: ChatSeq(proof_id),
            }),
            content: text("Good. Run them once more on the release branch, then tag 0.9."),
        },
    )
    .await?;

    let bisect = post_idea(
        &paths,
        nightly,
        "Bisect the flaky integration shard between last Tuesday and today.",
    )
    .await?;
    let stopped = until("the bisect agent", Duration::from_secs(30), async || {
        let out = eval(
            &paths,
            helper,
            format!(
                "import json\nfrom rho import chat\nprint(json.dumps((chat.thread({}) or {{}}).get('agents', []), default=str))",
                bisect.0.0
            ),
        )
        .await?;
        let found: Vec<serde_json::Value> = serde_json::from_str(out.lines().last().unwrap_or("[]"))?;
        let found = found.into_iter().filter_map(|agent| match agent {
            serde_json::Value::String(id) => Some(id),
            serde_json::Value::Object(fields) => fields.get("id").and_then(|id| id.as_str()).map(str::to_owned),
            _ => None,
        });
        Ok(found.into_iter().next())
    })
    .await?;
    let stopped = AgentId::from_encoded(&stopped).map_err(|error| anyhow::anyhow!("{error}"))?;
    // Let it reach its first retry, as a user would notice and stop it.
    tokio::time::sleep(Duration::from_millis(400)).await;
    call(&paths, StopAgent { agent: stopped }).await?;
    call(
        &paths,
        rho_agents_client::protocol::worksets::DeleteWorksets {
            worksets: vec![helper],
        },
    )
    .await?;
    println!(
        "{}",
        json!({
            "ci/working": checks.0.0,
            "ci/proof": proof_id,
            "ci/stopped": bisect.0.0,
            "ci/stopped-agent": stopped.encoded(),
        })
    );
    Ok(())
}

/// Runs `code` in a workset of its own on a live host and prints what it
/// printed: how a check or a QA agent reads the host's chat and agents.
async fn py(rig: &Path, host: &str, code: &str) -> anyhow::Result<()> {
    let paths = host_paths(&rig.canonicalize()?, host)?;
    let workset = call(
        &paths,
        CreateWorkset {
            kind: WorksetKind::Host {
                cwd: rig.canonicalize()?.to_string_lossy().into_owned().into(),
            },
            code: String::new(),
        },
    )
    .await?
    .workset;
    let out = eval(&paths, workset, code.to_owned()).await;
    call(
        &paths,
        rho_agents_client::protocol::worksets::DeleteWorksets {
            worksets: vec![workset],
        },
    )
    .await?;
    println!("{}", out?);
    Ok(())
}

/// A fresh agent on a live host says something in `thread`.
async fn say(rig: &Path, host: &str, thread: u64, words: &str, flash: bool) -> anyhow::Result<()> {
    let rig = rig.canonicalize()?;
    let paths = host_paths(&rig, host)?;
    let created = call(
        &paths,
        CreateWorkset {
            kind: WorksetKind::Host {
                cwd: repo(&rig, "tide").to_string_lossy().into_owned().into(),
            },
            code: r#"new_agent(runtime={"kind": "native", "model": "sol", "effort": "medium"})"#
                .into(),
        },
    )
    .await?;
    let agent = created
        .result
        .agent
        .context("the agent was not created")?
        .encoded();
    let words = serde_json::to_string(words)?;
    let kind = if flash { "flash" } else { "message" };
    let out = eval(
        &paths,
        created.workset,
        format!(
            "from rho.tools import Chat\nprint(int(Chat({agent:?}).send({words}, {kind:?}, thread={thread})))"
        ),
    )
    .await?;
    let message: u64 = out.lines().last().context("the message id")?.parse()?;
    println!("{} {agent} {message}", created.workset);
    Ok(())
}

async fn edit(
    rig: &Path,
    host: &str,
    workset: WorksetId,
    agent: &str,
    message: u64,
    words: &str,
) -> anyhow::Result<()> {
    let paths = host_paths(&rig.canonicalize()?, host)?;
    eval(
        &paths,
        workset,
        format!(
            "from rho.tools import Chat\nChat({agent:?}).edit({message}, {})",
            serde_json::to_string(words)?
        ),
    )
    .await?;
    Ok(())
}

/// A screenshot of the window, judged: the window's whole size, some ink
/// on it (a blank frame is one color, or very nearly), and changed since
/// `previous`, the shot before it in the journey. Every step of a journey
/// changes the screen, so an identical frame means the window stopped
/// drawing.
fn check_png(file: &Path, previous: Option<&Path>) -> anyhow::Result<()> {
    let image = image::open(file)
        .with_context(|| format!("read {}", file.display()))?
        .to_rgba8();
    let (width, height) = image.dimensions();
    let mut colors = std::collections::HashMap::<[u8; 4], u64>::new();
    for pixel in image.pixels() {
        *colors.entry(pixel.0).or_default() += 1;
    }
    let pixels = u64::from(width) * u64::from(height);
    let background = colors.values().copied().max().unwrap_or(0);
    let ink = 1.0 - background as f64 / pixels.max(1) as f64;
    let changed = match previous {
        None => None,
        Some(previous) => {
            let before = image::open(previous)
                .with_context(|| format!("read {}", previous.display()))?
                .to_rgba8();
            Some(if before.dimensions() == image.dimensions() {
                before
                    .pixels()
                    .zip(image.pixels())
                    .filter(|(a, b)| a != b)
                    .count() as f64
                    / pixels.max(1) as f64
            } else {
                1.0
            })
        }
    };
    println!(
        "{}",
        json!({"width": width, "height": height, "colors": colors.len(), "ink": ink, "changed": changed})
    );
    anyhow::ensure!(
        colors.len() >= 16 && ink >= 0.002,
        "{} is blank: {} colors, {:.4} of it ink",
        file.display(),
        colors.len(),
        ink
    );
    anyhow::ensure!(
        changed != Some(0.0),
        "{} is the same frame as the step before it: the window stopped drawing",
        file.display()
    );
    Ok(())
}

/// A recording, judged by walking its EBML: a WebM with a video track and
/// frames in clusters. A recording stopped mid-cluster ends early, so the
/// walk stops at the end of the file rather than failing there.
fn check_webm(file: &Path) -> anyhow::Result<()> {
    let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let found = webm::walk(&bytes).with_context(|| format!("{} is not a WebM", file.display()))?;
    println!(
        "{}",
        json!({
            "bytes": bytes.len(),
            "doctype": found.doctype,
            "codec": found.codec,
            "width": found.width,
            "height": found.height,
            "clusters": found.clusters,
            "frames": found.frames,
            "keyframes": found.keyframes,
            "duration_ms": found.last_ms,
        })
    );
    anyhow::ensure!(found.doctype == "webm", "doctype {:?}", found.doctype);
    anyhow::ensure!(
        found.codec.starts_with("V_"),
        "no video track: {:?}",
        found.codec
    );
    anyhow::ensure!(
        found.keyframes >= 1 && found.frames >= 10,
        "{} frames, {} of them keyframes",
        found.frames,
        found.keyframes
    );
    Ok(())
}

mod webm {
    const EBML: u64 = 0x1A45_DFA3;
    const DOCTYPE: u64 = 0x4282;
    const SEGMENT: u64 = 0x1853_8067;
    const TRACKS: u64 = 0x1654_AE6B;
    const TRACK_ENTRY: u64 = 0xAE;
    const CODEC_ID: u64 = 0x86;
    const VIDEO: u64 = 0xE0;
    const PIXEL_WIDTH: u64 = 0xB0;
    const PIXEL_HEIGHT: u64 = 0xBA;
    const CLUSTER: u64 = 0x1F43_B675;
    const TIMESTAMP: u64 = 0xE7;
    const SIMPLE_BLOCK: u64 = 0xA3;
    const BLOCK_GROUP: u64 = 0xA0;
    const BLOCK: u64 = 0xA1;
    /// What can follow a cluster at the segment's level, which is where a
    /// cluster of unknown size ends.
    const LEVEL_ONE: [u64; 8] = [
        CLUSTER,
        0x1C53_BB6B,
        0x1254_C367,
        0x1043_A770,
        0x1941_A469,
        0x114D_9B74,
        0x1549_A966,
        TRACKS,
    ];

    #[derive(Default)]
    pub struct Found {
        pub doctype: String,
        pub codec: String,
        pub width: u64,
        pub height: u64,
        pub clusters: u64,
        pub frames: u64,
        pub keyframes: u64,
        pub last_ms: i64,
    }

    /// A variable-length integer at `at`: its value, without the length
    /// marker unless `id`, its length, and whether it is all ones (an
    /// unknown size).
    fn vint(bytes: &[u8], at: usize, id: bool) -> anyhow::Result<(u64, usize, bool)> {
        let first = *bytes
            .get(at)
            .ok_or_else(|| anyhow::anyhow!("ends inside a number"))?;
        anyhow::ensure!(first != 0, "a number longer than eight bytes at {at}");
        let len = first.leading_zeros() as usize + 1;
        let raw = bytes
            .get(at..at + len)
            .ok_or_else(|| anyhow::anyhow!("ends inside a number"))?;
        let mut value = if id {
            u64::from(first)
        } else {
            u64::from(first) & ((1 << (8 - len)) - 1)
        };
        for byte in &raw[1..] {
            value = value << 8 | u64::from(*byte);
        }
        let unknown = !id && value == (1u64 << (7 * len)) - 1;
        Ok((value, len, unknown))
    }

    struct Element {
        id: u64,
        body: usize,
        /// Where the body ends, or `None` for an unknown size.
        end: Option<usize>,
    }

    fn element(bytes: &[u8], at: usize) -> anyhow::Result<Element> {
        let (id, id_len, _) = vint(bytes, at, true)?;
        let (size, size_len, unknown) = vint(bytes, at + id_len, false)?;
        let body = at + id_len + size_len;
        Ok(Element {
            id,
            body,
            end: (!unknown).then(|| body.saturating_add(size as usize)),
        })
    }

    fn uint(bytes: &[u8]) -> u64 {
        bytes
            .iter()
            .fold(0, |value, byte| value << 8 | u64::from(*byte))
    }

    pub fn walk(bytes: &[u8]) -> anyhow::Result<Found> {
        let mut found = Found::default();
        let header = element(bytes, 0)?;
        anyhow::ensure!(header.id == EBML, "no EBML header");
        let header_end = header
            .end
            .ok_or_else(|| anyhow::anyhow!("EBML header of unknown size"))?;
        let mut at = header.body;
        while at < header_end.min(bytes.len()) {
            let child = element(bytes, at)?;
            let end = child.end.unwrap_or(header_end).min(bytes.len());
            if child.id == DOCTYPE {
                found.doctype = String::from_utf8_lossy(&bytes[child.body..end]).into_owned();
            }
            at = end;
        }
        let segment = element(bytes, header_end)?;
        anyhow::ensure!(segment.id == SEGMENT, "no segment after the header");
        let segment_end = segment.end.unwrap_or(bytes.len()).min(bytes.len());
        let mut at = segment.body;
        while at < segment_end {
            let Ok(child) = element(bytes, at) else { break };
            match child.id {
                TRACKS => tracks(
                    bytes,
                    child.body,
                    child.end.unwrap_or(segment_end).min(segment_end),
                    &mut found,
                )?,
                CLUSTER => {
                    found.clusters += 1;
                    at = cluster(
                        bytes,
                        child.body,
                        child.end.map(|end| end.min(segment_end)),
                        segment_end,
                        &mut found,
                    );
                    continue;
                }
                _ => {}
            }
            at = child.end.unwrap_or(segment_end);
        }
        Ok(found)
    }

    fn tracks(bytes: &[u8], mut at: usize, end: usize, found: &mut Found) -> anyhow::Result<()> {
        while at < end {
            let child = element(bytes, at)?;
            let child_end = child.end.unwrap_or(end).min(end);
            match child.id {
                TRACK_ENTRY | VIDEO => tracks(bytes, child.body, child_end, found)?,
                CODEC_ID if found.codec.is_empty() => {
                    found.codec =
                        String::from_utf8_lossy(&bytes[child.body..child_end]).into_owned();
                }
                PIXEL_WIDTH => found.width = uint(&bytes[child.body..child_end]),
                PIXEL_HEIGHT => found.height = uint(&bytes[child.body..child_end]),
                _ => {}
            }
            at = child_end;
        }
        Ok(())
    }

    /// Counts a cluster's frames; returns where the cluster ends.
    fn cluster(
        bytes: &[u8],
        mut at: usize,
        end: Option<usize>,
        limit: usize,
        found: &mut Found,
    ) -> usize {
        let mut timestamp = 0i64;
        loop {
            if end.is_some_and(|end| at >= end) || at >= limit {
                return end.unwrap_or(limit).min(limit);
            }
            let Ok(child) = element(bytes, at) else {
                return limit;
            };
            if end.is_none() && LEVEL_ONE.contains(&child.id) {
                return at;
            }
            let child_end = child.end.unwrap_or(limit).min(limit);
            match child.id {
                TIMESTAMP => timestamp = uint(&bytes[child.body..child_end]) as i64,
                SIMPLE_BLOCK | BLOCK_GROUP => {
                    let block = if child.id == SIMPLE_BLOCK {
                        Some(child.body)
                    } else {
                        element(bytes, child.body)
                            .ok()
                            .filter(|inner| inner.id == BLOCK)
                            .map(|inner| inner.body)
                    };
                    if let Some(block) = block
                        && let Ok((_, track_len, _)) = vint(bytes, block, false)
                        && let Some(head) = bytes.get(block + track_len..block + track_len + 3)
                    {
                        found.frames += 1;
                        let relative = i64::from(i16::from_be_bytes([head[0], head[1]]));
                        found.last_ms = found.last_ms.max(timestamp + relative);
                        if child.id == SIMPLE_BLOCK && head[2] & 0x80 != 0 {
                            found.keyframes += 1;
                        }
                    }
                }
                _ => {}
            }
            at = child_end;
        }
    }
}
