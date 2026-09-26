use std::sync::Arc;
use std::time::Duration;

use rho_inference2::Item;
use rho_inference2::scripted::Scripted;

use crate::chat::ChatKind;
use crate::log::{Block, Entry, Log, Party, Wake};
use crate::{Agent, AgentHandle, Config, Inbound};

fn shell(dir: &tempfile::TempDir) -> rho_tool_shell::ShellTools {
    rho_tool_shell::ShellTools::in_directory(
        Duration::from_secs(20),
        camino::Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap(),
        rho_fs_view::PathOverrides::default(),
    )
}

fn start(dir: &tempfile::TempDir, log: Log, model: Arc<Scripted>) -> (Agent, AgentHandle) {
    Agent::new(Config {
        id: "a1".into(),
        log,
        model: Arc::new(rho_inference2::Model::Scripted(model)),
        shell: shell(dir),
        instructions: "test".into(),
    })
    .unwrap()
}

fn say(handle: &AgentHandle, text: &str) {
    handle
        .send(Inbound {
            from: Party::Human,
            body: vec![Block::Text(text.into())],
        })
        .unwrap();
}

/// Waits for a chat event `want` accepts.
async fn until_chat(
    chat: &mut tokio::sync::broadcast::Receiver<crate::chat::ChatEvent>,
    want: impl Fn(&ChatKind) -> bool,
) -> ChatKind {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = chat.recv().await.unwrap();
            if want(&event.kind) {
                return event.kind;
            }
        }
    })
    .await
    .expect("the chat event arrived")
}

fn sent(text: &str) -> impl Fn(&ChatKind) -> bool + '_ {
    move |kind| {
        matches!(kind, ChatKind::Message { from: Party::Agent(_), body, .. }
            if body == &[Block::Text(text.into())])
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_talks_only_through_messages_and_waits_on_the_human() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model
        .then("human.status('greeting')\nhuman.send('hello')\nawait human.reply()")
        .then("print('got it')\nawait human.reply()");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());

    say(&handle, "hi");
    until_chat(&mut chat, sent("hello")).await;
    say(&handle, "bye");
    tokio::time::timeout(Duration::from_secs(10), async {
        while model.remaining() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(model.remaining(), 0);

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    let [
        Item::User { text: woken, .. },
        Item::User { text: first, .. },
    ] = &requests[0].items[..]
    else {
        panic!("{:?}", requests[0].items)
    };
    assert_eq!(woken, "New messages below.");
    assert_eq!(first, "Message from the human:\nhi");
    let [
        _,
        _,
        Item::Step(_),
        Item::Result { call_id, text, .. },
        Item::User { text: second, .. },
    ] = &requests[1].items[..]
    else {
        panic!("{:?}", requests[1].items)
    };
    assert_eq!(call_id, "call_1");
    assert_eq!(text, "Task finished", "the cell said nothing");
    assert_eq!(second, "Message from the human:\nbye");
    drop(handle);
    running.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn prose_is_undelivered_and_the_model_is_told_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model
        .then_prose()
        .then("human.send('sorry')\nawait human.reply()");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "hi");
    until_chat(&mut chat, sent("sorry")).await;
    let requests = model.requests();
    let Some(Item::User { text, .. }) = requests[1].items.last() else {
        panic!("{:?}", requests[1].items)
    };
    assert!(text.contains("no exec call"), "{text}");
    drop(handle);
    running.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_returning_cell_wakes_the_model_with_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model
        .then("print(6 * 7)")
        .then("human.send('done')\nawait human.reply()");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "compute");
    until_chat(&mut chat, sent("done")).await;
    let requests = model.requests();
    let Some(Item::Result { text, .. }) = requests[1].items.last() else {
        panic!("{:?}", requests[1].items)
    };
    assert_eq!(text.trim(), "42");
    drop(handle);
    running.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_agent_is_told_its_notebook_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    let model = Arc::new(Scripted::new());
    model.then("human.send('working')\nawait asyncio.sleep(60)");
    let (agent, handle) = start(&dir, Log::open(&path).unwrap(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "go");
    until_chat(&mut chat, sent("working")).await;
    drop(handle);
    running.abort();
    let _ = running.await;

    let model = Arc::new(Scripted::new());
    model.then("await human.reply()");
    let (agent, handle) = start(&dir, Log::open(&path).unwrap(), Arc::clone(&model));
    assert!(
        !agent
            .chat()
            .iter()
            .any(|event| matches!(event.kind, ChatKind::Status(_)))
    );
    let running = tokio::spawn(agent.run());
    tokio::time::timeout(Duration::from_secs(10), async {
        while model.remaining() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let request = &model.requests()[0];
    // The step never finished, so there is no call to answer.
    let Some(Item::User { text, .. }) = request.items.last() else {
        panic!("{:?}", request.items)
    };
    assert!(text.contains("rho restarted"), "{text}");
    drop(handle);
    running.abort();
    let log = Log::open(&path).unwrap();
    assert!(log.entries().iter().any(|entry| matches!(
        entry,
        Entry::Woken {
            why: Wake::Restarted,
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_off_cell_keeps_the_statements_that_ran() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model
        .then_cut("human.send('started')\nawait asyncio.sleep(0.3)\nhuman.send('never")
        .then("await human.reply()");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "go");
    until_chat(&mut chat, sent("started")).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while model.remaining() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let request = &model.requests()[1];
    let Some(Item::Result { call_id, text, .. }) = request
        .items
        .iter()
        .rev()
        .find(|item| matches!(item, Item::Result { .. }))
    else {
        panic!("{:?}", request.items)
    };
    assert_eq!(call_id, "call_1");
    assert!(text.starts_with("Your response was cut off"), "{text}");
    let steps = model.requests()[1]
        .items
        .iter()
        .filter(|item| matches!(item, Item::Step(_)))
        .count();
    assert_eq!(steps, 1);
    drop(handle);
    running.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_mid_response_waits_two_seconds_after_completion() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model
        .then("x = 1\ny = 2\nz = 3\na = 4\nb = 5\nc = 6\nawait asyncio.sleep(5)")
        .then("human.send('saw second')\nawait human.reply()");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut trace = handle.trace();
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "first");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(trace.recv().await.unwrap(), crate::Trace::Woken { .. }) {}
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    say(&handle, "second");
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(trace.recv().await.unwrap(), crate::Trace::Step { .. }) {
                break tokio::time::Instant::now();
            }
        }
    })
    .await
    .unwrap();
    until_chat(&mut chat, sent("saw second")).await;
    assert!(
        completed.elapsed() >= Duration::from_millis(1900),
        "mid-response mail woke too soon: {:?}",
        completed.elapsed()
    );
    drop(handle);
    running.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn checkin_runs_while_awaiting_human_reply() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model
        .then("set_max_wait(1)\nawait human.reply()")
        .then("human.send('checked')\nawait human.reply()");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut trace = handle.trace();
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "wait");
    until_chat(&mut chat, sent("checked")).await;
    let mut saw_checkin = false;
    while let Ok(event) = trace.try_recv() {
        if matches!(
            event,
            crate::Trace::Woken {
                why: Wake::Checkin,
                ..
            }
        ) {
            saw_checkin = true;
        }
    }
    assert!(saw_checkin);
    drop(handle);
    running.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_mutes_until_human_revives_a_fresh_notebook() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model
        .then("archive()")
        .then("human.send(str('x' in globals()))\nawait human.reply()");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "archive");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(model.remaining(), 1, "archive allowed an automatic wake");
    say(&handle, "return");
    until_chat(&mut chat, sent("False")).await;
    assert!(
        model.requests()[1].items.iter().any(
            |item| matches!(item, Item::Result { text, .. } if text.contains("fresh notebook"))
        )
    );
    drop(handle);
    running.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn human_revives_archive_only_after_the_streaming_response_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    let mut code = String::from("archive()\n");
    for n in 0..30 {
        code.push_str(&format!("x{n} = {n}\n"));
    }
    model
        .then(&code)
        .then("human.send('revived')\nawait human.reply()");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut trace = handle.trace();
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "archive then return");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(trace.recv().await.unwrap(), crate::Trace::Woken { .. }) {}
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    say(&handle, "return during response");
    until_chat(&mut chat, sent("revived")).await;
    assert!(
        model.requests()[1].items.iter().any(
            |item| matches!(item, Item::Result { text, .. } if text.contains("fresh notebook"))
        )
    );
    drop(handle);
    running.abort();
}
