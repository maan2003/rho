//! The store's folds, against a disposable database.

use rho_agent_types::{
    Address, ChatEvent, ChatSeq, ContentPart, InputWake, Mention, MentionKind, MessageId, StreamId,
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
        event: Event::Input {
            agent,
            message: message.map(|seq| MessageId(ChatSeq(seq))),
            content: vec![ContentPart::Text { text: "hi".into() }],
            wake: InputWake::Person,
        },
    }
}

fn native(agent: AgentId, entry: Entry) -> Record<'static> {
    Record {
        at: UnixMs(6),
        event: Event::Native { agent, entry },
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
    // Delivery is the newest mail an input handed over, rewound or not:
    // mail once handed over is not handed over again.
    assert_eq!(read.agent(a).unwrap().delivered, ChatSeq(3));
    assert_eq!(read.agent(b).unwrap().delivered, ChatSeq(4));
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
    use crate::entry::{Report, Wake};
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
            &native(
                a,
                Entry::RequestSent {
                    why: Wake::Compaction,
                    report: Report::default(),
                    compact: true,
                },
            ),
        )
        .unwrap();
    // An input lands while the compacting response streams: it may not be
    // in that request, so it must stay in the context after the compaction.
    let late = write.append(ws, &input(a, None)).unwrap();
    write
        .append(
            ws,
            &native(
                a,
                Entry::Step {
                    exec: None,
                    prose: String::new(),
                    carry: crate::inference::Carry::new(serde_json::json!({}), vec![], true),
                    usage: None,
                },
            ),
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
async fn chat_delivers_to_a_topics_agents_and_the_mentioned_but_never_the_sender() {
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
    // A new store's chat starts with its default stream.
    let rho = write.chat_stream_named(chat::DEFAULT_STREAM).unwrap();
    assert_eq!(rho, StreamId(ChatSeq(1)));

    // The user starts a topic with `a`; a resend under its client id is the
    // same topic and logs nothing.
    let (topic, first) = write
        .start_topic(
            UnixMs(3),
            rho,
            "Fix it".into(),
            vec![a, a],
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
            vec![a],
            said(Address::User, "go!"),
            Some(77),
        )
        .unwrap();
    assert_eq!(again, (topic, first));

    // `b` was not in the topic: posting puts it there, and it does not get
    // its own message.
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
    // `a` mentions `c`, which is not in the topic: `c` gets that message
    // only, and stays out of the topic.
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
    assert!(
        write
            .post(
                UnixMs(9),
                TopicId(ChatSeq(99)),
                said(Address::User, "x"),
                None
            )
            .is_err()
    );
    assert!(
        write
            .post(UnixMs(9), topic, said(Address::User, "  "), None)
            .is_err()
    );
    assert!(
        write
            .post(UnixMs(9), TopicId(first.0), said(Address::User, "x"), None)
            .is_err()
    );
    // Joining twice logs once.
    write.join(UnixMs(10), topic, c).unwrap();
    write.join(UnixMs(11), topic, c).unwrap();
    let after_join = write
        .post(UnixMs(12), topic, said(Address::User, "all"), None)
        .unwrap();
    assert!(write.delete_workset(ws));
    write.commit();

    let read = db.read();
    let seq = |id: MessageId| id.0.0;
    assert_eq!(
        (seq(from_b), seq(from_user), seq(mention_c), seq(after_join)),
        (4, 5, 6, 8),
        "dense, and the refused posts took none"
    );
    assert_eq!(received(&read, a, 0), vec![3, 4, 5, 8]);
    assert_eq!(
        received(&read, b, 0),
        vec![3, 5, 6, 8]
            .into_iter()
            .filter(|s| *s != 3)
            .collect::<Vec<_>>()
    );
    assert_eq!(received(&read, c, 0), vec![6, 8]);
    assert_eq!(received(&read, a, 4), vec![5, 8]);
    assert_eq!(read.topic(topic).unwrap().agents, vec![a, b, c]);
    let events: Vec<_> = read.chat_since(ChatSeq::ZERO, 100);
    assert_eq!(events.len(), 8);
    assert!(matches!(&events[6].event, ChatEvent::Joined { agent, .. } if *agent == c));
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
