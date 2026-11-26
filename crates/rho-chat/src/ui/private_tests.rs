//! A private reply is the user's own note under a message: it opens where
//! the message is read, and writing it leaves the thread's card alone.

use editor::ToPoint as _;
use gpui::VisualTestContext;
use rho_agent_types::{ChatEvent, ChatRecord, ChatSeq, ContentPart, UnixMs, Voice};
use rho_dealer::Addressed as _;
use rho_frame::Frame;
use rho_frame::attention::Write;

use super::thread::{KeepPrivateReply, ReplyPrivately};
use super::*;

const HOST: EndpointId = EndpointId([3; 32]);

fn message(seq: u64) -> ThreadId {
    ThreadId {
        host: HOST,
        message: MessageId(ChatSeq(seq)),
    }
}

/// A project `clock` whose thread 2 the agent wrote in, `Reading 3.` to
/// `Reading {last}.`, the last asking the user.
fn clock(last: u64) -> HostChat {
    let agent = Party::Agent(AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap());
    let mut log = HostChat::default();
    for seq in 1..=last {
        let (parent, from, text) = match seq {
            1 => (None, Party::User, "Clock".to_owned()),
            2 => (Some(1), Party::User, "Check the clocks.".to_owned()),
            seq => (
                Some(2),
                agent,
                format!("Reading {seq}.\n\nThe clock drifted."),
            ),
        };
        log.tell(&ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(1_760_000_000_000 + seq),
            event: ChatEvent::Posted {
                parent: parent.map(|id| MessageId(ChatSeq(id))),
                retry_of: None,
                thread_reply: None,
                from,
                voice: Voice::Message,
                content: vec![ContentPart::Text { text }],
            },
        });
    }
    log.tell(&ChatRecord {
        seq: ChatSeq(last + 1),
        at: UnixMs(1_760_000_000_000 + last + 1),
        event: ChatEvent::Project {
            project: ProjectId(MessageId(ChatSeq(1))),
            config: rho_agent_types::ProjectConfig {
                place: rho_agent_types::ProjectPlace::parse("~/src/clock").unwrap(),
                mode: rho_agent_types::ProjectMode::PerIdea,
                runtime: rho_agent_types::RuntimeLabel::default(),
            },
        },
    });
    log
}

/// The line the point is on in the screen in view.
fn point_line(window: gpui::WindowHandle<Frame>, cx: &mut gpui::TestAppContext) -> String {
    cx.run_until_parked();
    cx.draw_window(window.into());
    window
        .update(cx, |frame, _, cx| {
            let editor = frame.current_surface().editor(cx).expect("a buffer");
            let editor = editor.read(cx);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let row = editor
                .selections
                .newest_anchor()
                .head()
                .to_point(&snapshot)
                .row;
            let end = snapshot.line_len(multi_buffer::MultiBufferRow(row));
            snapshot
                .text_for_range(language::Point::new(row, 0)..language::Point::new(row, end))
                .collect()
        })
        .unwrap()
}

/// Opened by name, from `Notes` or `Places`, a private reply opens the
/// thread its message is in, with the point on the message.
#[gpui::test]
fn a_private_reply_opens_its_thread_at_its_message(cx: &mut gpui::TestAppContext) {
    let (window, _mail, _db) = search_bench::fixture(cx, clock(40));
    window
        .update(cx, |frame, window, cx| {
            let note = rho_notes::create_private(frame, &message(9).address(), cx);
            frame.write_marks(
                vec![Write::Note {
                    note: note.0,
                    body: Some("Is reading 9 right?".into()),
                    deleted: None,
                }],
                cx,
            );
            assert!(frame.marks().get(&note.address()).about.is_some());
            frame.open(note.address(), window, cx);
            assert_eq!(frame.current_address(), &message(2).address());
        })
        .unwrap();
    assert!(
        point_line(window, cx).contains("Reading 9."),
        "the point is on the message"
    );
}

/// Writing a private reply under the message that asks leaves the
/// thread's card as it was: still waiting, from the same message, with
/// nothing said about the thread.
#[gpui::test]
fn writing_a_private_reply_leaves_the_card_alone(cx: &mut gpui::TestAppContext) {
    let (window, mail, _db) = search_bench::fixture(cx, clock(4));
    let thread = message(2).address();
    let card = |window: gpui::WindowHandle<Frame>, cx: &mut gpui::TestAppContext| {
        cx.run_until_parked();
        window
            .update(cx, |frame, _, cx| {
                frame.systems_changed(cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |frame, _, _| {
                frame
                    .hand()
                    .cards
                    .iter()
                    .find(|card| card.address == thread)
                    .cloned()
            })
            .unwrap()
    };
    // Its priority rises as the test runs; everything else is the card.
    let card = move |window, cx: &mut gpui::TestAppContext| {
        card(window, cx).map(|card| rho_dealer::Card {
            priority: 0.0,
            ..card
        })
    };
    let before = card(window, cx).expect("the ask waits");
    window
        .update(cx, |frame, window, cx| {
            frame.open_at(
                thread.clone(),
                &rho_frame::SearchPoint::Message {
                    message: message(4).address(),
                    line: 0,
                },
                window,
                cx,
            );
        })
        .unwrap();
    assert!(point_line(window, cx).contains("Reading 4."));
    cx.dispatch_action(window.into(), ReplyPrivately);
    cx.run_until_parked();
    {
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.simulate_input("Is this the drift?");
        visual.dispatch_action(KeepPrivateReply);
    }
    cx.run_until_parked();
    window
        .update(cx, |frame, _, _| {
            let replies = frame.marks().replies_to(&message(4).address());
            assert_eq!(
                replies.first().and_then(|(_, held)| held.body.as_deref()),
                Some("Is this the drift?"),
                "the reply is written"
            );
            assert!(
                frame.marks().get(&thread).facts.is_empty(),
                "nothing is said about the thread"
            );
        })
        .unwrap();
    assert_eq!(card(window, cx), Some(before), "the card is as it was");
    cx.update(|cx| {
        assert!(
            mail.update(cx, |system, _| system.outbox.next(HOST).is_none()),
            "nothing is sent"
        )
    });
}
