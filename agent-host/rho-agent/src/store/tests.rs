//! The store's folds, against a disposable database.

use rho_agent_types::{
    Address, ChatEvent, ChatSeq, ContentPart, Mention, MentionKind, MessageId, Role, StreamId,
    TopicId, UnixMs, WorksetId, WorksetKind, Wpos,
};
use rho_db::RhoDb;

use super::*;
use crate::inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use crate::record::Runtime;

fn spec(name: &str) -> AgentSpec {
    AgentSpec {
        runtime: Runtime::Native {
            model: InferenceModel::Gpt61Sol,
            profile: InferenceProfile {
                effort: ReasoningEffort::High,
                fast_mode: false,
            },
            cache_key: crate::inference::PromptCacheKey::generate(),
        },
        props: [("name".to_owned(), name.to_owned())].into(),
    }
}

async fn db() -> (tempfile::TempDir, RhoDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = RhoDb::open(dir.path().join("db"));
    let mut write = db.write().await;
    write.init_store_tables();
    write.commit();
    (dir, db)
}

fn agent(n: u64) -> AgentId {
    AgentId::from_counter(n, &rho_agent_types::AgentIdDomain(7)).unwrap()
}

fn input(agent: AgentId, message: Option<u64>) -> Record<'static> {
    Record {
        at: UnixMs(5),
        event: Event::carrying(
            agent,
            vec![crate::record::Input {
                role: Role::User,
                message: message.map(|seq| MessageId(ChatSeq(seq))),
                content: vec![ContentPart::Text { text: "hi".into() }],
            }],
        ),
    }
}

fn at_six(event: Event<'static>) -> Record<'static> {
    Record {
        at: UnixMs(6),
        event,
    }
}

#[tokio::test]
async fn agents_fold_only_their_own_rows_and_rewinds_hide_only_theirs() {
    let (_dir, db) = db().await;
    let ws = WorksetId([1; 6]);
    let (a, b) = (agent(1), agent(2));
    let mut write = db.write().await;
    write.create_workset(ws, UnixMs(1), WorksetKind::Isolated, "setup()".into());
    for (id, name) in [(a, "a"), (b, "b")] {
        write
            .append(
                ws,
                &Record {
                    at: UnixMs(2),
                    event: Event::AgentCreated {
                        agent: id,
                        spec: spec(name),
                    },
                },
            )
            .unwrap();
    }
    let a_input = write.append(ws, &input(a, Some(3))).unwrap();
    let b_input = write.append(ws, &input(b, Some(4))).unwrap();
    let a_later = write.append(ws, &input(a, None)).unwrap();
    // A rewind of `a` back to its first input hides it and the later one,
    // but not `b`'s input that sits between them.
    let rewound = write
        .append(
            ws,
            &Record {
                at: UnixMs(9),
                event: Event::Rewound {
                    agent: a,
                    to: a_input,
                },
            },
        )
        .unwrap();
    write.commit();

    let read = db.read();
    assert_eq!(read.workset(ws).unwrap().agents, vec![a, b]);
    assert_eq!(read.workset(ws).unwrap().next, rewound.next());
    let a_rows: Vec<Wpos> = read
        .agent_rows(a, Wpos::ZERO)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(a_rows, vec![Wpos(1), rewound]);
    assert!(!a_rows.contains(&a_later));
    let b_rows: Vec<Wpos> = read
        .agent_rows(b, Wpos::ZERO)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(b_rows, vec![Wpos(2), b_input]);
    // Delivery is the newest message an input handed over, rewound or
    // not: a message once handed over is not handed over again.
    assert_eq!(read.agent(a).unwrap().delivered, ChatSeq(3));
    assert_eq!(read.agent(b).unwrap().delivered, ChatSeq(4));
}

/// The Claude worker reads `archived` back after a rewind, so the rewind has to
/// take the notice with it.
#[tokio::test]
async fn a_rewind_past_an_archive_takes_it_back() {
    let (_dir, db) = db().await;
    let ws = WorksetId([1; 6]);
    let a = agent(1);
    let mut write = db.write().await;
    write.create_workset(ws, UnixMs(1), WorksetKind::Isolated, "setup()".into());
    write
        .append(
            ws,
            &at_six(Event::AgentCreated {
                agent: a,
                spec: spec("a"),
            }),
        )
        .unwrap();
    let archive = |write: &mut rho_db::WriteTxn| {
        write
            .append(
                ws,
                &at_six(Event::Notice {
                    agent: a,
                    notice: Notice::Archived,
                }),
            )
            .unwrap()
    };
    let before = write.append(ws, &input(a, None)).unwrap();
    let archived = archive(&mut write);
    let after = write.append(ws, &input(a, None)).unwrap();
    assert!(write.agent_head(a).unwrap().native.archived);

    // To a row after the notice: it stands.
    write
        .append(
            ws,
            &at_six(Event::Rewound {
                agent: a,
                to: after,
            }),
        )
        .unwrap();
    assert!(write.agent_head(a).unwrap().native.archived);
    // To the notice itself: it goes, and a rewind to the row before keeps it
    // gone.
    write
        .append(
            ws,
            &at_six(Event::Rewound {
                agent: a,
                to: archived,
            }),
        )
        .unwrap();
    assert!(!write.agent_head(a).unwrap().native.archived);
    write
        .append(
            ws,
            &at_six(Event::Rewound {
                agent: a,
                to: before,
            }),
        )
        .unwrap();
    assert!(!write.agent_head(a).unwrap().native.archived);
    write.commit();
    assert!(!db.read().agent(a).unwrap().native.archived);
}

#[tokio::test]
async fn a_row_for_an_agent_of_another_workset_is_refused() {
    let (_dir, db) = db().await;
    let (one, two) = (WorksetId([1; 6]), WorksetId([2; 6]));
    let a = agent(1);
    let mut write = db.write().await;
    write.create_workset(one, UnixMs(1), WorksetKind::Isolated, String::new());
    write.create_workset(two, UnixMs(1), WorksetKind::Isolated, String::new());
    write
        .append(
            one,
            &Record {
                at: UnixMs(2),
                event: Event::AgentCreated {
                    agent: a,
                    spec: spec("a"),
                },
            },
        )
        .unwrap();
    assert!(write.append(two, &input(a, None)).is_err());
    assert!(write.append(one, &input(agent(9), None)).is_err());
    assert_eq!(write.append(one, &input(a, None)).unwrap(), Wpos(2));
}

#[tokio::test]
async fn compaction_starts_the_context_after_the_request_it_answered() {
    let (_dir, db) = db().await;
    let ws = WorksetId([3; 6]);
    let a = agent(1);
    let mut write = db.write().await;
    write.create_workset(ws, UnixMs(1), WorksetKind::Isolated, String::new());
    write
        .append(
            ws,
            &Record {
                at: UnixMs(2),
                event: Event::AgentCreated {
                    agent: a,
                    spec: spec("a"),
                },
            },
        )
        .unwrap();
    let request = write
        .append(
            ws,
            &Record {
                at: UnixMs(6),
                event: Event::Request {
                    agent: a,
                    output: Vec::new(),
                    inputs: Vec::new(),
                    notices: Vec::new(),
                    compact: true,
                    turn: None,
                    timing: None,
                },
            },
        )
        .unwrap();
    // A row lands while the compacting response streams: it stays in the
    // context after the compaction.
    let late = write
        .append(
            ws,
            &at_six(Event::Notice {
                agent: a,
                notice: Notice::Error("retry".into()),
            }),
        )
        .unwrap();
    write
        .append(
            ws,
            &at_six(Event::Step {
                agent: a,
                exec: None,
                prose: String::new(),
                carry: crate::inference::Carry::new(serde_json::json!({}), vec![], true),
                usage: None,
            }),
        )
        .unwrap();
    write.commit();
    let head = db.read().agent(a).unwrap();
    assert_eq!(head.native.from, request.next());
    assert_eq!(head.native.from, late);
    assert!(head.native.woken);
}

fn said(from: Address, text: &str) -> NewMessage {
    NewMessage {
        from,
        mentions: Vec::new(),
        content: vec![ContentPart::Text { text: text.into() }],
    }
}

fn received(read: &rho_db::ReadTxn, agent: AgentId, after: u64) -> Vec<u64> {
    read.inbox(agent, ChatSeq(after))
        .iter()
        .map(|message| message.id.0.0)
        .collect()
}

#[tokio::test]
async fn chat_delivers_to_members_and_the_mentioned_but_never_the_sender() {
    let (_dir, db) = db().await;
    let ws = WorksetId([4; 6]);
    let (a, b, c) = (agent(1), agent(2), agent(3));
    let mut write = db.write().await;
    write.create_workset(
        ws,
        UnixMs(1),
        WorksetKind::Host { cwd: "/tmp".into() },
        String::new(),
    );
    for (agent, name) in [(a, "a"), (b, "b"), (c, "c")] {
        write
            .append(
                ws,
                &Record {
                    at: UnixMs(2),
                    event: Event::AgentCreated {
                        agent,
                        spec: spec(name),
                    },
                },
            )
            .unwrap();
    }
    // A new store's chat is empty: the first stream is its first event,
    // and asking for it by name again makes no second one.
    let rho = write.stream(UnixMs(3), "rho".into()).unwrap();
    assert_eq!(write.stream(UnixMs(3), " rho ".into()).unwrap(), rho);
    assert_eq!(rho, StreamId(ChatSeq(1)));

    // The user starts a topic; it has no members, and a resend under its
    // client id is the same topic and logs nothing.
    let (topic, first) = write
        .start_topic(
            UnixMs(3),
            rho,
            "Fix it".into(),
            said(Address::User, "go"),
            Some(77),
        )
        .unwrap();
    assert_eq!((topic, first), (TopicId(ChatSeq(2)), MessageId(ChatSeq(3))));
    let again = write
        .start_topic(
            UnixMs(4),
            rho,
            "Fix it".into(),
            said(Address::User, "go!"),
            Some(77),
        )
        .unwrap();
    assert_eq!(again, (topic, first));

    // `a` joins after the first message, so it does not get it.
    write.join(UnixMs(4), topic, a).unwrap();
    // `b` is no member: posting reaches `a` and does not make `b` one.
    let from_b = write
        .post(UnixMs(5), topic, said(Address::Agent(b), "me too"), None)
        .unwrap();
    let from_user = write
        .post(UnixMs(6), topic, said(Address::User, "both"), Some(78))
        .unwrap();
    assert_eq!(
        write
            .post(UnixMs(7), topic, said(Address::User, "resent"), Some(78))
            .unwrap(),
        from_user
    );
    // `a` mentions `c`: `c` gets that message only and is no member.
    let mention_c = write
        .post(
            UnixMs(8),
            topic,
            NewMessage {
                mentions: vec![
                    Mention {
                        to: Address::Agent(c),
                        kind: MentionKind::Ask,
                    },
                    Mention {
                        to: Address::User,
                        kind: MentionKind::Result,
                    },
                ],
                ..said(Address::Agent(a), "c, look")
            },
            None,
        )
        .unwrap();
    // Refused posts take no seq.
    for (to, text) in [
        (TopicId(ChatSeq(99)), "x"),
        (topic, "  "),
        (TopicId(first.0), "x"),
    ] {
        assert!(
            write
                .post(UnixMs(9), to, said(Address::User, text), None)
                .is_err()
        );
    }
    // Joining twice logs once.
    write.join(UnixMs(10), topic, a).unwrap();

    // Two agents' direct conversation is one, whichever asks; only they
    // post in it, and no one joins it.
    let direct = write.direct(UnixMs(11), a, b).unwrap();
    assert_eq!(write.direct(UnixMs(11), b, a).unwrap(), direct);
    assert!(write.direct(UnixMs(11), a, a).is_err());
    let in_direct = write
        .post(UnixMs(12), direct, said(Address::Agent(a), "brief"), None)
        .unwrap();
    assert!(
        write
            .post(UnixMs(12), direct, said(Address::User, "hi"), None)
            .is_err()
    );
    assert!(
        write
            .post(UnixMs(12), direct, said(Address::Agent(c), "hi"), None)
            .is_err()
    );
    assert!(write.join(UnixMs(12), direct, c).is_err());

    let after = write
        .post(UnixMs(13), topic, said(Address::User, "all"), None)
        .unwrap();
    assert!(write.delete_workset(ws));
    write.commit();

    let read = db.read();
    let seq = |id: MessageId| id.0.0;
    assert_eq!(
        (
            seq(from_b),
            seq(from_user),
            seq(mention_c),
            direct.0.0,
            seq(in_direct),
            seq(after)
        ),
        (5, 6, 7, 8, 9, 10),
        "dense, and the refused posts and repeats took none"
    );
    assert_eq!(received(&read, a, 0), vec![5, 6, 10]);
    assert_eq!(received(&read, b, 0), vec![9]);
    assert_eq!(received(&read, c, 0), vec![7]);
    assert_eq!(received(&read, a, 5), vec![6, 10]);
    assert_eq!(read.topic(topic).unwrap().agents, vec![a]);
    assert_eq!(read.topic(direct).unwrap().place, chat::Place::Direct);
    let events: Vec<_> = read.chat_since(ChatSeq::ZERO, 100);
    assert_eq!(events.len(), 10);
    assert!(matches!(&events[3].event, ChatEvent::Joined { agent, .. } if *agent == a));
    assert!(matches!(&events[7].event, ChatEvent::DirectStarted { members } if *members == [a, b]));
    // Chat outlives the workset.
    let kept = read.message(mention_c).unwrap();
    assert_eq!(kept.mentions(Address::User), Some(MentionKind::Result));
    assert_eq!(kept.mentions(Address::Agent(b)), None);
    assert!(
        read.message(MessageId(topic.0)).is_none(),
        "a topic is not a message"
    );
    assert!(read.workset(ws).is_none());
    assert!(read.agent(a).is_none());
    assert!(read.workset_rows(ws, Wpos::ZERO, 10).is_empty());
}

/// A leave stops what a topic delivers but not what mentions; a change is
/// logged only when it changes something; a move takes messages out of
/// their topics, delivers nothing again, and a resend under its client id
/// starts no second topic.
#[tokio::test]
async fn chat_leaves_changes_and_moves() {
    let (_dir, db) = db().await;
    let (a, b) = (agent(1), agent(2));
    let mut write = db.write().await;
    let rho = write.stream(UnixMs(1), "rho".into()).unwrap();
    let ops = write.stream(UnixMs(1), "ops".into()).unwrap();
    let (topic, first) = write
        .start_topic(
            UnixMs(2),
            rho,
            "Fix it".into(),
            said(Address::User, "go"),
            None,
        )
        .unwrap();
    write.join(UnixMs(3), topic, a).unwrap();
    let kept = write
        .post(UnixMs(4), topic, said(Address::User, "one"), None)
        .unwrap();
    write.leave(UnixMs(5), topic, a).unwrap();
    // Leaving twice logs once.
    write.leave(UnixMs(5), topic, a).unwrap();
    let unheard = write
        .post(UnixMs(6), topic, said(Address::User, "two"), None)
        .unwrap();
    let mention = write
        .post(
            UnixMs(7),
            topic,
            NewMessage {
                mentions: vec![Mention {
                    to: Address::Agent(a),
                    kind: MentionKind::Ask,
                }],
                ..said(Address::User, "a, look")
            },
            None,
        )
        .unwrap();
    write
        .change_topic(UnixMs(8), topic, ops, " Fixed ".into(), true)
        .unwrap();
    let head = db_head(&mut write);
    write
        .change_topic(UnixMs(8), topic, ops, "Fixed".into(), true)
        .unwrap();
    assert_eq!(
        db_head(&mut write),
        head,
        "a change to nothing logs nothing"
    );
    assert!(
        write
            .change_topic(UnixMs(8), topic, StreamId(ChatSeq(99)), "x".into(), false)
            .is_err()
    );
    let direct = write.direct(UnixMs(9), a, b).unwrap();
    let in_direct = write
        .post(UnixMs(9), direct, said(Address::Agent(a), "psst"), None)
        .unwrap();
    assert!(write.leave(UnixMs(9), direct, a).is_err());
    assert!(
        write
            .change_topic(UnixMs(9), direct, rho, "x".into(), false)
            .is_err()
    );
    // Out of a direct conversation, or into one: refused, and logs nothing.
    let before = db_head(&mut write);
    for (messages, to) in [
        (vec![in_direct], MoveTo::Topic(topic)),
        (vec![kept], MoveTo::Topic(direct)),
        (vec![MessageId(ChatSeq(999))], MoveTo::Topic(topic)),
        (Vec::new(), MoveTo::Topic(topic)),
    ] {
        assert!(
            write
                .move_messages(UnixMs(10), messages, to, Address::User, None)
                .is_err()
        );
    }
    assert_eq!(db_head(&mut write), before);
    // Two to a new topic, under a client id; the resend is the same.
    let aside = write
        .move_messages(
            UnixMs(10),
            vec![mention, unheard],
            MoveTo::NewTopic {
                stream: rho,
                name: "aside".into(),
            },
            Address::User,
            Some(5),
        )
        .unwrap();
    assert_eq!(
        write
            .move_messages(
                UnixMs(11),
                vec![mention, unheard],
                MoveTo::NewTopic {
                    stream: rho,
                    name: "aside".into(),
                },
                Address::User,
                Some(5),
            )
            .unwrap(),
        aside
    );
    // And one of them back where it was posted.
    write
        .move_messages(
            UnixMs(12),
            vec![unheard],
            MoveTo::Topic(topic),
            Address::User,
            None,
        )
        .unwrap();
    write.commit();

    let read = db.read();
    let ids = |topic| {
        read.topic_messages(topic, 10)
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(topic), [first, kept, unheard]);
    assert_eq!(ids(aside), [mention]);
    assert_eq!(read.message(mention).unwrap().topic, aside);
    assert_eq!(read.message(unheard).unwrap().topic, topic);
    assert_eq!(
        read.topic(topic).unwrap().place,
        chat::Place::Stream {
            stream: ops,
            name: "Fixed".into(),
            resolved: true
        }
    );
    assert!(read.topic(topic).unwrap().agents.is_empty());
    // `a` heard what came while it was a member, and the mention; the
    // moves delivered nothing.
    assert_eq!(received(&read, a, 0), vec![kept.0.0, mention.0.0]);
    let moves = read
        .chat_since(ChatSeq::ZERO, 100)
        .into_iter()
        .filter(|record| matches!(record.event, ChatEvent::Moved { .. }))
        .count();
    assert_eq!(moves, 2, "the resend logged no second move");
}

fn db_head(write: &mut rho_db::WriteTxn) -> u64 {
    write
        .open_table(redb::TableDefinition::<
            u64,
            rho_db::Sen<rho_agent_types::ChatRecord>,
        >::new("chat_log"))
        .iter()
        .next_back()
        .map_or(0, |(key, _)| key.value())
}

/// A workset's props are what Python said last before its first agent;
/// after that they are fixed.
#[tokio::test]
async fn workset_props_are_set_before_the_first_agent_only() {
    let (_dir, db) = db().await;
    let workset = WorksetId([3; 6]);
    let mut write = db.write().await;
    write.create_workset(workset, UnixMs(1), WorksetKind::Isolated, String::new());
    set_props(&mut write, workset, policy("first")).unwrap();
    set_props(&mut write, workset, policy("second")).unwrap();
    create_agent(&mut write, workset, 1);
    let error = set_props(&mut write, workset, policy("third")).unwrap_err();
    assert!(error.to_string().contains("once after it"), "{error}");
    write.commit();
    assert_eq!(db.read().workset(workset).unwrap().props, policy("second"));
}

/// A workset made before its policy named it gets its props once, also
/// with agents, and keeps them from then on.
#[tokio::test]
async fn a_workset_without_props_sets_them_once_after_its_first_agent() {
    let (_dir, db) = db().await;
    let workset = WorksetId([4; 6]);
    let mut write = db.write().await;
    write.create_workset(workset, UnixMs(1), WorksetKind::Isolated, String::new());
    create_agent(&mut write, workset, 2);
    set_props(&mut write, workset, policy("late")).unwrap();
    let error = set_props(&mut write, workset, policy("again")).unwrap_err();
    assert!(error.to_string().contains("once after it"), "{error}");
    write.commit();
    assert_eq!(db.read().workset(workset).unwrap().props, policy("late"));
}

fn policy(name: &str) -> rho_agent_types::Props {
    [("policy".to_owned(), name.to_owned())].into()
}

fn set_props(
    write: &mut rho_db::WriteTxn,
    workset: WorksetId,
    props: rho_agent_types::Props,
) -> anyhow::Result<Wpos> {
    write.append(
        workset,
        &Record {
            at: UnixMs(2),
            event: Event::WorksetPropsSet { props },
        },
    )
}

fn create_agent(write: &mut rho_db::WriteTxn, workset: WorksetId, n: u64) {
    write
        .append(
            workset,
            &Record {
                at: UnixMs(3),
                event: Event::AgentCreated {
                    agent: agent(n),
                    spec: spec("a"),
                },
            },
        )
        .unwrap();
}
