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
        id: crate::log::AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(42)).unwrap(),
        log,
        model: Arc::new(rho_inference2::Model::Scripted(model)),
        shell: shell(dir),
        instructions: "test".into(),
        agent_tools: None,
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
    // The model step may finish after sending the message but before abort.
    // Restart must explain lost notebook state whether its call was logged or not.
    let had_step = Log::open(&path)
        .unwrap()
        .entries()
        .iter()
        .any(|entry| matches!(entry, Entry::Step { .. }));

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
    // A logged call receives the restart as its result; an interrupted step
    // receives it as a user report. Neither path can inherit the old notebook.
    match (had_step, request.items.last()) {
        (true, Some(Item::Result { text, .. })) | (false, Some(Item::User { text, .. })) => {
            assert!(text.contains("rho restarted"), "{text}")
        }
        _ => panic!("{:?}", request.items),
    }
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

#[tokio::test(flavor = "multi_thread")]
async fn rewind_branches_context_but_preserves_live_notebook_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    let model = Arc::new(Scripted::new());
    model
        .then("marker = 31\nhuman.send('first branch')\nawait human.reply()")
        .then("human.send(f'marker={marker}')\nawait human.reply()");
    let (agent, handle) = start(&dir, Log::open(&path).unwrap(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "discard this prompt");
    until_chat(&mut chat, sent("first branch")).await;
    handle.rewind(1).await.unwrap();
    until_chat(&mut chat, sent("marker=31")).await;
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    let text = requests[1]
        .items
        .iter()
        .filter_map(|item| match item {
            Item::User { text, .. } | Item::Result { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("rewound your visible history"), "{text}");
    assert!(!text.contains("discard this prompt"), "{text}");
    assert!(!text.contains("first branch"), "{text}");
    assert!(matches!(
        handle.rewind(0).await,
        Err(error) if error.to_string().contains("greater than zero")
    ));
    drop(handle);
    running.abort();
    let log = Log::open(&path).unwrap();
    assert!(
        log.entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Rewound { .. }))
    );
    assert!(
        log.entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Received { body, .. }
        if body == &[Block::Text("discard this prompt".into())]))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_during_a_streaming_model_step_drains_the_notebook() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model.then(&"marker = 1\n".repeat(250));
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let running = tokio::spawn(agent.run());
    say(&handle, "start a long model step");
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    handle.stop();
    tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("stop interrupts the in-flight provider step")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_interrupts_model_step_but_allows_a_follow_up_turn() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model
        .then(&"marker = 1\n".repeat(250))
        .then("human.send('followed up')");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "start a long model step");
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    handle.cancel();
    say(&handle, "please follow up");
    until_chat(&mut chat, sent("followed up")).await;
    handle.stop();
    tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_compaction_keeps_the_provider_boundary_and_waits_for_new_input() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Scripted::new());
    model
        .then_compaction()
        .then("human.send('after compaction')");
    let (agent, handle) = start(&dir, Log::in_memory(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    handle.compact().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        model.requests()[0].items.as_slice(),
        [Item::CompactionTrigger]
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        model.requests().len(),
        1,
        "manual-only compaction owes no answer"
    );
    say(&handle, "continue");
    until_chat(&mut chat, sent("after compaction")).await;
    let requests = model.requests();
    assert!(matches!(requests[1].items.first(), Some(Item::Step(carry)) if carry.has_compaction()));
    assert!(
        !requests[1]
            .items
            .iter()
            .any(|item| matches!(item, Item::CompactionTrigger))
    );
    handle.stop();
    running.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn threshold_compaction_replies_from_the_new_window() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::in_memory();
    let call = rho_inference2::Call {
        id: rho_inference2::CallId::new("previous"),
        code: "print('old context')".into(),
    };
    log.append(Entry::Created {
        at: rho_agent_types::UnixMs(0),
        cache_key: Default::default(),
    })
    .unwrap();
    log.append(Entry::Step {
        at: rho_agent_types::UnixMs(1),
        call: Some(call.clone()),
        prose: String::new(),
        carry: rho_inference2::Carry::bare(call),
        usage: rho_inference2::Usage {
            input_tokens: 232_560,
            ..Default::default()
        },
    })
    .unwrap();
    log.append(Entry::Woken {
        at: rho_agent_types::UnixMs(2),
        why: Wake::Returned,
        report: "old code returned".into(),
        images: Vec::new(),
        messages: Vec::new(),
    })
    .unwrap();
    let model = Arc::new(Scripted::new());
    model.then_compaction().then("human.send('continued')");
    let (agent, handle) = start(&dir, log, Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    until_chat(&mut chat, sent("continued")).await;
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        requests[0].items.last(),
        Some(Item::CompactionTrigger)
    ));
    assert!(matches!(requests[1].items.first(), Some(Item::Step(carry)) if carry.has_compaction()));
    assert!(
        !requests[1].items.iter().any(|item| matches!(item,
        Item::Result { text, .. } | Item::User { text, .. } if text.contains("old code returned")))
    );
    handle.stop();
    running.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rewind_before_compaction_reopens_the_older_branch_only() {
    let scripted = Arc::new(Scripted::new());
    scripted.then_compaction();
    let compacted = rho_inference2::Model::Scripted(scripted)
        .step(
            &rho_inference2::Request {
                instructions: "test".into(),
                items: Vec::new(),
                cache_key: Default::default(),
            },
            &mut |_| {},
        )
        .await
        .unwrap()
        .carry;
    let at = rho_agent_types::UnixMs(1);
    let mut log = Log::in_memory();
    log.append(Entry::Woken {
        at,
        why: Wake::Message,
        report: "old branch".into(),
        images: Vec::new(),
        messages: Vec::new(),
    })
    .unwrap();
    log.append(Entry::CompactionTrigger { at, manual: true })
        .unwrap();
    log.append(Entry::Step {
        at,
        call: None,
        prose: String::new(),
        carry: compacted,
        usage: Default::default(),
    })
    .unwrap();
    log.append(Entry::Woken {
        at,
        why: Wake::Message,
        report: "new branch".into(),
        images: Vec::new(),
        messages: Vec::new(),
    })
    .unwrap();
    let compact_request = crate::context::request("test".into(), log.entries(), Default::default());
    assert!(
        matches!(compact_request.items.first(), Some(Item::Step(carry)) if carry.has_compaction())
    );
    assert!(!compact_request.items.iter().any(|item| matches!(item,
        Item::User { text, .. } if text == "old branch")));
    log.append(Entry::Rewound { at, to: 1 }).unwrap();
    log.append(Entry::Woken {
        at,
        why: Wake::Rewound,
        report: "replacement branch".into(),
        images: Vec::new(),
        messages: Vec::new(),
    })
    .unwrap();
    let rewound = crate::context::request("test".into(), log.entries(), Default::default());
    assert!(rewound.items.iter().any(|item| matches!(item,
        Item::User { text, .. } if text == "old branch")));
    assert!(rewound.items.iter().any(|item| matches!(item,
        Item::User { text, .. } if text == "replacement branch")));
    assert!(
        !rewound
            .items
            .iter()
            .any(|item| matches!(item, Item::Step(carry) if carry.has_compaction()))
    );
    assert!(
        !rewound
            .items
            .iter()
            .any(|item| matches!(item, Item::CompactionTrigger))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_retries_a_pending_trigger_then_keeps_its_compacted_carry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    let at = rho_agent_types::UnixMs(1);
    let mut log = Log::open(&path).unwrap();
    log.append(Entry::Created {
        at,
        cache_key: Default::default(),
    })
    .unwrap();
    log.append(Entry::CompactionTrigger { at, manual: true })
        .unwrap();
    let model = Arc::new(Scripted::new());
    model.then_compaction();
    let (agent, handle) = start(&dir, log, Arc::clone(&model));
    let mut trace = handle.trace();
    let running = tokio::spawn(agent.run());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(trace.recv().await.unwrap(), crate::Trace::Step { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        model.requests()[0].items.last(),
        Some(Item::CompactionTrigger)
    ));
    handle.stop();
    running.await.unwrap().unwrap();
    assert!(
        Log::open(&path)
            .unwrap()
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Step { carry, .. } if carry.has_compaction()))
    );

    let model = Arc::new(Scripted::new());
    model.then("human.send('restored')");
    let (agent, handle) = start(&dir, Log::open(&path).unwrap(), Arc::clone(&model));
    let mut chat = handle.chat();
    let running = tokio::spawn(agent.run());
    say(&handle, "after restart");
    until_chat(&mut chat, sent("restored")).await;
    let request = &model.requests()[0];
    assert!(matches!(request.items.first(), Some(Item::Step(carry)) if carry.has_compaction()));
    assert!(
        !request
            .items
            .iter()
            .any(|item| matches!(item, Item::CompactionTrigger))
    );
    handle.stop();
    running.await.unwrap().unwrap();
}
