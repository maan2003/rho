//! Chat beyond one reply: what is unread and the browser that lists it,
//! marking read, the topic edits each sending the call it names, and `#N`
//! links.

use gpui::TestAppContext;
use rho_agent_hosts::HostId;
use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatSeq, ContentPart, MessageId, StreamId, TopicId, UnixMs,
};
use rho_agents_client::HostTopic;
use rho_agents_client::protocol::worksets::{MoveTo, Request};
use rho_dealer::NodeId;
use rho_dealer::facts::Fact;

use super::{agent, bind_test_keymaps, ready_with, story, test_workspace, ui_head};
use crate::chat::MoveMode;
use crate::pane::SurfaceKey;
use crate::workspace::Workspace;

const RHO: StreamId = StreamId(ChatSeq(1));

fn posted(topic: TopicId, from: Address, text: &str, at: u64) -> rho_agent_types::ChatRecord {
    story::record(
        UnixMs(at),
        ChatEvent::Posted {
            topic,
            from,
            mentions: Vec::new(),
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
        },
    )
}

fn started(name: &str, at: u64) -> rho_agent_types::ChatRecord {
    story::record(
        UnixMs(at),
        ChatEvent::TopicStarted {
            stream: RHO,
            name: name.to_owned(),
            by: Address::User,
        },
    )
}

fn host_topic(topic: TopicId) -> HostTopic {
    HostTopic {
        machine_seed: 0,
        topic,
    }
}

/// A workspace whose host knows `agent`, and the host's call queue.
fn attached(
    cx: &mut TestAppContext,
    member: AgentId,
) -> (
    gpui::WindowHandle<Workspace>,
    tokio::sync::mpsc::UnboundedReceiver<rho_rpc::Stream>,
) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    let host = workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(member)], 900),
                window,
                cx,
            );
            workspace.host_in_process_for_test(HostId::default())
        })
        .unwrap();
    (workspace, host)
}

fn feed(
    workspace: &gpui::WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    records: Vec<rho_agent_types::ChatRecord>,
) {
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                story::chat(records),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
}

/// Seeds the view of `topic` with its messages, opens it, and puts the
/// point on the message `at`.
fn open_at(
    workspace: &gpui::WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    topic: HostTopic,
    messages: &[(MessageId, Address, &str)],
    at: MessageId,
) {
    workspace
        .update(cx, |workspace, window, cx| {
            let mut state = rho_agents_client::chat::topic_state(&[], &[], &[]);
            state.blocks = messages
                .iter()
                .map(|(id, from, text)| {
                    std::sync::Arc::new(rho_agents_client::state::UiBlock::ChatMessage {
                        id: *id,
                        from: *from,
                        mentions: Vec::new(),
                        text: (*text).to_owned(),
                        named: false,
                    })
                })
                .collect();
            workspace.open_topic(topic, window, cx);
            let summary = workspace.transcripts.set_topic(topic, state);
            workspace.sync_topic_model(topic, summary, cx);
            workspace.open_topic_at(topic, at, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
}

/// Unread is what others wrote after the newest of what the user marked
/// read and what they wrote; the user's own messages never count, and
/// opening a topic marks nothing.
#[gpui::test]
fn unread_counts_others_after_the_read_and_never_the_users_own(cx: &mut TestAppContext) {
    let member = agent(901);
    let (workspace, _host) = attached(cx, member);
    let older = started("older", 1);
    let topic = TopicId(older.seq);
    let from = Address::Agent(member);
    let records = vec![
        older,
        posted(topic, from, "one", 2),
        posted(topic, Address::User, "mine", 3),
        posted(topic, from, "two", 4),
        posted(topic, from, "three", 5),
        posted(
            topic,
            Address::User,
            "mine again, before the others read",
            6,
        ),
    ];
    feed(&workspace, cx, records);
    let newer = started("newer", 7);
    let newer_topic = TopicId(newer.seq);
    let a = posted(newer_topic, from, "a", 8);
    let b = posted(newer_topic, from, "b", 9);
    let c = posted(newer_topic, Address::User, "c", 10);
    feed(&workspace, cx, vec![newer, a, b, c]);
    let unread = |workspace: &gpui::WindowHandle<Workspace>, cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, _| {
                (
                    workspace.unread(host_topic(topic)),
                    workspace.unread(host_topic(newer_topic)),
                )
            })
            .unwrap()
    };
    assert_eq!(
        unread(&workspace, cx),
        (0, 0),
        "the user's newest message reads what came before it"
    );
    feed(&workspace, cx, vec![posted(newer_topic, from, "d", 11)]);
    assert_eq!(unread(&workspace, cx), (0, 1));
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_topic(host_topic(newer_topic), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(unread(&workspace, cx), (0, 1), "opening reads nothing");
    let rows = workspace
        .update(cx, |workspace, _, _| workspace.browser_rows())
        .unwrap();
    let listed = rows.streams[0]
        .topics
        .iter()
        .map(|row| (row.name.as_str(), row.unread))
        .collect::<Vec<_>>();
    assert_eq!(listed, vec![("newer", 1), ("older", 0)], "newest first");
    workspace
        .update(cx, |workspace, _, cx| workspace.topic_done(cx))
        .unwrap();
    assert_eq!(unread(&workspace, cx), (0, 0));
    let read = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .attention
                .marks
                .get(&NodeId::Topic(host_topic(newer_topic)))
                .facts
                .iter()
                .filter(|entry| matches!(entry.fact, Fact::Settled { .. }))
                .count()
        })
        .unwrap();
    assert_eq!(read, 1);
    workspace
        .update(cx, |workspace, _, cx| workspace.topic_done(cx))
        .unwrap();
    let again = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .attention
                .marks
                .get(&NodeId::Topic(host_topic(newer_topic)))
                .facts
                .len()
        })
        .unwrap();
    assert_eq!(again, 1, "done again says nothing new");
}

/// Done on a topic the user already read still takes its todo back, and
/// `shift-u` puts the todo back.
#[gpui::test]
fn done_takes_back_a_todo_on_a_read_topic_and_undo_restores_it(cx: &mut TestAppContext) {
    let member = agent(912);
    let (workspace, _host) = attached(cx, member);
    let start = started("chores", 1);
    let topic = TopicId(start.seq);
    feed(
        &workspace,
        cx,
        vec![start, posted(topic, Address::User, "remind me", 2)],
    );
    let node = NodeId::Topic(host_topic(topic));
    let todo = |workspace: &gpui::WindowHandle<Workspace>, cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, _| {
                workspace
                    .attention
                    .marks
                    .get(&node)
                    .facts()
                    .todo()
                    .is_some()
            })
            .unwrap()
    };
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.write_marks(
                vec![
                    Fact::Todo {
                        node: node.clone(),
                        start: None,
                        seen: rho_dealer::facts::Seen::Topic(0),
                    }
                    .into(),
                ],
                cx,
            );
            workspace.open_topic(host_topic(topic), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert!(todo(&workspace, cx));
    workspace
        .update(cx, |workspace, _, cx| workspace.topic_done(cx))
        .unwrap();
    assert!(
        !todo(&workspace, cx),
        "the user wrote last, and done still settles"
    );
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.undo_verdict(window, cx)
        })
        .unwrap();
    assert!(todo(&workspace, cx), "undo takes the done back");
}

/// "Done through here" reads up to the message at point and no further:
/// what came after it stays unread.
#[gpui::test]
fn done_through_a_message_leaves_the_later_ones_unread(cx: &mut TestAppContext) {
    let member = agent(911);
    let (workspace, _host) = attached(cx, member);
    let start = started("steps", 1);
    let topic = TopicId(start.seq);
    let from = Address::Agent(member);
    let records = vec![
        start,
        posted(topic, from, "one", 2),
        posted(topic, from, "two", 3),
        posted(topic, from, "three", 4),
    ];
    let ids = records[1..]
        .iter()
        .map(|record| MessageId(record.seq))
        .collect::<Vec<_>>();
    feed(&workspace, cx, records);
    let messages = ids
        .iter()
        .zip(["one", "two", "three"])
        .map(|(id, text)| (*id, from, text))
        .collect::<Vec<_>>();
    open_at(&workspace, cx, host_topic(topic), &messages, ids[1]);
    let unread = workspace
        .update(cx, |workspace, _, cx| {
            let before = workspace.unread(host_topic(topic));
            workspace.topic_done_through_here(cx);
            (before, workspace.unread(host_topic(topic)))
        })
        .unwrap();
    assert_eq!(unread, (3, 1), "only the third is left");
}

/// The browser opens the row under the cursor, and hides a topic once it
/// is resolved, which its title then says with `✔`.
#[gpui::test]
fn the_browser_opens_its_row_and_a_resolved_topic_says_so(cx: &mut TestAppContext) {
    let member = agent(921);
    let (workspace, mut host) = attached(cx, member);
    let first = started("first", 1);
    let second = started("second", 2);
    let (first_topic, second_topic) = (TopicId(first.seq), TopicId(second.seq));
    feed(&workspace, cx, vec![first, second]);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_browser(window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape enter");
    cx.run_until_parked();
    let current = workspace
        .update(cx, |workspace, _, _| {
            workspace.current_surface_key_for_test()
        })
        .unwrap();
    assert_eq!(
        current,
        SurfaceKey::Topic(host_topic(second_topic)),
        "the cursor starts on the newest topic"
    );
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.toggle_resolved(window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    let mut calls = story::calls(&mut host);
    assert_eq!(calls.len(), 1);
    let (call, _) = calls.pop().unwrap();
    let Request::ChangeTopic(change) = call else {
        panic!("resolving changes the topic: {call:?}");
    };
    assert_eq!(
        (
            change.topic,
            change.stream,
            change.name.as_str(),
            change.resolved
        ),
        (second_topic, RHO, "second", true),
        "the whole head, with only resolved changed"
    );
    feed(
        &workspace,
        cx,
        vec![story::record(
            UnixMs(3),
            ChatEvent::TopicChanged {
                topic: second_topic,
                stream: RHO,
                name: "second".to_owned(),
                resolved: true,
            },
        )],
    );
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.topic_title(host_topic(second_topic)),
                "rho › ✔ second"
            );
            let rows = workspace.browser_rows();
            let resolved = rows.streams[0]
                .topics
                .iter()
                .map(|row| (row.topic.topic, row.resolved))
                .collect::<Vec<_>>();
            assert_eq!(resolved, vec![(second_topic, true), (first_topic, false)]);
        })
        .unwrap();
}

/// A move of "this and later" sends the message at point and every later
/// one of its topic, none before; "this message" to a topic picked by
/// its title sends that one alone to that topic.
#[gpui::test]
fn a_move_sends_the_messages_its_mode_names(cx: &mut TestAppContext) {
    let member = agent(931);
    let (workspace, mut host) = attached(cx, member);
    let source = started("source", 1);
    let other = started("other", 2);
    let (topic, other_topic) = (TopicId(source.seq), TopicId(other.seq));
    let from = Address::Agent(member);
    let mut records = vec![source, other];
    for (index, text) in ["one", "two", "three", "four"].iter().enumerate() {
        records.push(posted(topic, from, text, 3 + index as u64));
    }
    let ids = records[2..]
        .iter()
        .map(|record| MessageId(record.seq))
        .collect::<Vec<_>>();
    feed(&workspace, cx, records);
    let messages = ids
        .iter()
        .zip(["one", "two", "three", "four"])
        .map(|(id, text)| (*id, from, text))
        .collect::<Vec<_>>();
    open_at(&workspace, cx, host_topic(topic), &messages, ids[2]);
    let mut moved = |mode: MoveMode, typed: &str, cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, window, cx| {
                workspace.prompt_move_messages(mode, window, cx);
                let minibuffer = workspace.minibuffer.as_mut().expect("a move asks where");
                minibuffer.set_input(typed.to_owned(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.dispatch_action(*workspace, crate::MinibufferConfirm);
        cx.run_until_parked();
        let mut calls = story::calls(&mut host);
        assert_eq!(calls.len(), 1, "one move");
        let (call, _) = calls.pop().unwrap();
        let Request::MoveMessages(sent) = call else {
            panic!("a move: {call:?}");
        };
        (sent.messages, sent.to)
    };
    assert_eq!(
        moved(MoveMode::ThisAndLater, "rho › aside", cx),
        (
            ids[2..].to_vec(),
            MoveTo::NewTopic {
                stream: RHO,
                name: "aside".to_owned()
            }
        )
    );
    assert_eq!(
        moved(MoveMode::This, "rho › other", cx),
        (vec![ids[2]], MoveTo::Topic(other_topic))
    );
    assert_eq!(
        moved(MoveMode::Topic, "elsewhere", cx).0,
        ids,
        "every message"
    );
}

/// Taking a member out of a topic sends `Leave` for that member.
#[gpui::test]
fn removing_a_member_sends_leave(cx: &mut TestAppContext) {
    let member = agent(941);
    let (workspace, mut host) = attached(cx, member);
    let start = started("with one", 1);
    let topic = TopicId(start.seq);
    let joined = story::record(
        UnixMs(2),
        ChatEvent::Joined {
            topic,
            agent: member,
        },
    );
    feed(&workspace, cx, vec![start, joined]);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_topic(host_topic(topic), window, cx);
            workspace.prompt_remove_member(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();
    let mut calls = story::calls(&mut host);
    assert_eq!(calls.len(), 1);
    let (call, _) = calls.pop().unwrap();
    let Request::Leave(leave) = call else {
        panic!("removing a member: {call:?}");
    };
    assert_eq!((leave.topic, leave.agent), (topic, member));
}

/// A topic shows each join and leave where it happened, and `space o w`
/// tells who is in it now.
#[gpui::test]
fn a_topic_shows_who_joined_and_left_and_tells_its_members(cx: &mut TestAppContext) {
    let (stays, goes) = (agent(961), agent(962));
    let (workspace, _host) = attached(cx, stays);
    let start = started("with two", 1);
    let topic = TopicId(start.seq);
    let change = |at, event| story::record(UnixMs(at), event);
    feed(
        &workspace,
        cx,
        vec![
            start,
            change(2, ChatEvent::Joined { topic, agent: goes }),
            change(
                3,
                ChatEvent::Joined {
                    topic,
                    agent: stays,
                },
            ),
            change(4, ChatEvent::Left { topic, agent: goes }),
        ],
    );
    let labels = workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_topic(host_topic(topic), window, cx);
            workspace.show_members(cx);
            [stays, goes].map(|agent| workspace.registry.agent_short_label(agent))
        })
        .unwrap();
    cx.run_until_parked();
    let [stays, goes] = labels;
    let echo = workspace
        .update(cx, |workspace, _, _| {
            workspace.echo_text_for_test().map(str::to_owned)
        })
        .unwrap();
    assert_eq!(echo.as_deref(), Some(format!("members: {stays}").as_str()));
    let editor = super::active_editor(&workspace, cx);
    let text = workspace
        .update(cx, |_, _, cx| editor.read(cx).text(cx))
        .unwrap();
    assert_eq!(
        text.trim_end(),
        format!("{goes} joined\n\n{stays} joined\n\n{goes} left"),
        "in the order they happened"
    );
}

/// `enter` on `#N` in a topic opens topic N; on `#M` for a message in the
/// topic in view it puts the point there; anywhere else it is vim's.
#[gpui::test]
fn enter_on_a_link_opens_what_it_names(cx: &mut TestAppContext) {
    let member = agent(951);
    let (workspace, _host) = attached(cx, member);
    let target = started("target", 1);
    let here = started("here", 2);
    let (target_topic, here_topic) = (TopicId(target.seq), TopicId(here.seq));
    feed(&workspace, cx, vec![target, here]);
    let text = format!("see #{} for it", target_topic.0.0);
    let id = MessageId(ChatSeq(here_topic.0.0 + 1));
    open_at(
        &workspace,
        cx,
        host_topic(here_topic),
        &[(id, Address::Agent(member), text.as_str())],
        id,
    );
    let editor = super::active_editor(&workspace, cx);
    let place = |needle: &str, cx: &mut TestAppContext| {
        workspace
            .update(cx, |_, window, cx| {
                editor.update(cx, |editor, cx| {
                    let at = editor.text(cx).find(needle).expect("the text is drawn");
                    let at = multi_buffer::MultiBufferOffset(at + 2);
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_ranges([at..at]);
                    });
                });
            })
            .unwrap();
    };
    cx.simulate_keystrokes(*workspace, "escape");
    place("see #", cx);
    cx.simulate_keystrokes(*workspace, "enter");
    cx.run_until_parked();
    let current = workspace
        .update(cx, |workspace, _, _| {
            workspace.current_surface_key_for_test()
        })
        .unwrap();
    assert_eq!(
        current,
        SurfaceKey::Topic(host_topic(here_topic)),
        "control: the point was on `see`, not on the link"
    );
    place(&format!("#{}", target_topic.0.0), cx);
    cx.simulate_keystrokes(*workspace, "left left enter");
    cx.run_until_parked();
    let current = workspace
        .update(cx, |workspace, _, _| {
            workspace.current_surface_key_for_test()
        })
        .unwrap();
    assert_eq!(current, SurfaceKey::Topic(host_topic(target_topic)));
}

/// What `agent`'s runtime says it is writing, as its host streams it.
fn drafting(
    agent: AgentId,
    draft: Option<(TopicId, &str)>,
) -> rho_agents_client::stream::AgentFrame {
    use rho_agents_client::protocol::transcript::{Draft, Live, RuntimeState};
    rho_agents_client::stream::AgentFrame::Server(
        rho_agents_client::protocol::worksets::ServerFrame::Live {
            agent_id: agent,
            live: Live::Snapshot {
                state: RuntimeState::default(),
                response: None,
                draft: draft.map(|(topic, text)| Draft {
                    topic: Some(topic),
                    text: text.to_owned(),
                }),
            },
        },
    )
}

/// A member's live draft shows after the messages of the topic it names,
/// under the member's name, while it is written: it leaves when it names
/// another topic, and when the member stops writing it. The topic on
/// screen asks its host for its members' live frames.
#[gpui::test]
fn a_draft_shows_in_the_topic_it_names_while_it_is_written(cx: &mut TestAppContext) {
    let member = agent(961);
    let (workspace, _host) = attached(cx, member);
    let here = started("here", 1);
    let here_topic = TopicId(here.seq);
    let joined = story::record(
        UnixMs(2),
        ChatEvent::Joined {
            topic: here_topic,
            agent: member,
        },
    );
    let elsewhere = started("elsewhere", 3);
    let elsewhere_topic = TopicId(elsewhere.seq);
    feed(&workspace, cx, vec![here, joined, elsewhere]);
    let said = MessageId(ChatSeq(40));
    open_at(
        &workspace,
        cx,
        host_topic(here_topic),
        &[(said, Address::User, "go on")],
        said,
    );
    let focused = workspace
        .update(cx, |workspace, _, _| workspace.focused_agents())
        .unwrap();
    assert!(
        focused.contains(&member),
        "the topic on screen focuses its member"
    );

    let tell = |cx: &mut TestAppContext, draft| {
        workspace
            .update(cx, |workspace, window, cx| {
                story::feed(
                    workspace,
                    HostId::default(),
                    drafting(member, draft),
                    window,
                    cx,
                );
            })
            .unwrap();
        cx.run_until_parked();
        super::buffer_text(&workspace, cx)
    };

    let text = tell(cx, Some((here_topic, "half a thought")));
    let header = text.find("· writing…").expect("the draft's header");
    assert!(
        text.find("go on").unwrap() < header,
        "after the messages: {text}"
    );
    assert!(text[header..].contains("half a thought"), "{text}");

    let text = tell(cx, Some((elsewhere_topic, "other words")));
    assert!(
        !text.contains("half a thought"),
        "left for another topic: {text}"
    );
    assert!(!text.contains("other words"), "not this topic's: {text}");

    let text = tell(cx, Some((here_topic, "again")));
    assert!(text.contains("again"), "{text}");
    let text = tell(cx, None);
    assert!(!text.contains("writing…"), "stopped: {text}");
    assert!(text.contains("go on"), "the messages stay: {text}");
}

/// The agent transient restarts the selected agent's whole workset, not
/// another agent's, and reports completion only when the host acknowledges it.
#[gpui::test]
fn restart_workset_dispatches_for_the_subject_and_waits_for_the_host(cx: &mut TestAppContext) {
    let first = agent(951);
    let selected = agent(952);
    let (workspace, mut host) = attached(cx, first);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(first), ui_head(selected)], 900),
                window,
                cx,
            );
            workspace.select_agent(Some(selected), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape space a shift-r");
    cx.run_until_parked();

    let mut calls = story::calls(&mut host);
    assert_eq!(calls.len(), 1, "Space a Shift-R sends exactly one call");
    let (call, mut stream) = calls.pop().unwrap();
    let Request::RestartWorkset(restart) = call else {
        panic!("the transient must request a workset restart: {call:?}");
    };
    assert_eq!(
        restart.agent, selected,
        "the selected subject, not the first agent"
    );
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.menu_title_for_test(), None);
            assert_eq!(
                workspace.echo_text_for_test(),
                None,
                "sending is not success"
            );
        })
        .unwrap();

    story::answer(&mut stream, rho_rpc::protocol::Answer::Done(()));
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("restarted whole workset; running work ended, saved agents reloaded")
            );
        })
        .unwrap();
}

/// A connected host cannot restart an absent subject, and another host
/// being online does not make an offline subject actionable.
#[gpui::test]
fn restart_workset_requires_an_online_subject(cx: &mut TestAppContext) {
    let member = agent(961);
    let (workspace, mut host) = attached(cx, member);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(None, window, cx);
            workspace.cmd_restart_workset(window, cx);
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("restart-workset: no agent in focus")
            );
            let other = workspace.attach_host(
                crate::workspace::HostSpec {
                    name: "other".to_owned(),
                    target: crate::workspace::AttachTarget::Unix(
                        std::env::temp_dir().join("rho-gui-restart-other-nonexistent.sock"),
                    ),
                },
                cx,
            );
            story::feed(workspace, other, ready_with(Vec::new(), 0), window, cx);
            workspace.select_agent(Some(member), window, cx);
            workspace.hosts.set_status(
                HostId::default(),
                rho_agent_hosts::HostStatus::Disconnected("test offline".to_owned()),
            );
            assert!(
                workspace.hosts.any_online(),
                "the unrelated host is still online"
            );
            workspace.cmd_restart_workset(window, cx);
            assert!(
                workspace
                    .echo_text_for_test()
                    .is_some_and(|text| text.starts_with("not connected to ")),
                "an offline subject gets a connection notice"
            );
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        story::calls(&mut host).is_empty(),
        "neither case may send a restart"
    );
}

/// A refused restart is not reported as completed.
#[gpui::test]
fn restart_workset_reports_host_failure_instead_of_success(cx: &mut TestAppContext) {
    let member = agent(971);
    let (workspace, mut host) = attached(cx, member);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(Some(member), window, cx);
            workspace.cmd_restart_workset(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let mut calls = story::calls(&mut host);
    assert_eq!(calls.len(), 1);
    let (call, mut stream) = calls.pop().unwrap();
    assert!(matches!(call, Request::RestartWorkset(_)));
    story::answer(
        &mut stream,
        rho_rpc::protocol::Answer::<()>::Failed {
            reason: "could not reload saved agents".to_owned(),
        },
    );
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            let echo = workspace.echo_text_for_test().expect("host error is shown");
            assert!(echo.contains("could not reload saved agents"), "{echo}");
            assert!(!echo.contains("restarted whole workset"), "{echo}");
        })
        .unwrap();
}
