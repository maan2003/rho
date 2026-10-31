//! The store's folds, against a disposable database.

use rho_agent_types::{
    Address, ContentPart, InputWake, MailId, UnixMs, WorksetId, WorksetKind, Wpos,
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

fn input(agent: AgentId, mail: Option<u64>) -> Record<'static> {
    Record {
        at: UnixMs(5),
        event: Event::Input {
            agent,
            mail: mail.map(MailId),
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
    assert_eq!(read.agent(a).unwrap().delivered, MailId(3));
    assert_eq!(read.agent(b).unwrap().delivered, MailId(4));
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

#[tokio::test]
async fn mail_is_dense_logged_once_per_client_id_and_outlives_its_workset() {
    let (_dir, db) = db().await;
    let ws = WorksetId([4; 6]);
    let a = agent(1);
    let mut write = db.write().await;
    write.create_workset(
        ws,
        UnixMs(1),
        WorksetKind::Host { cwd: "/tmp".into() },
        String::new(),
    );
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
    let to_a = |text: &str| NewMail {
        from: Address::User,
        to: Address::Agent(a),
        re: None,
        kind: None,
        content: vec![ContentPart::Text { text: text.into() }],
    };
    let first = write.post_mail(UnixMs(3), to_a("one"), Some(77));
    let again = write.post_mail(UnixMs(4), to_a("one, resent"), Some(77));
    let other = write.post_mail(
        UnixMs(5),
        NewMail {
            from: Address::Agent(a),
            to: Address::User,
            re: Some(first),
            kind: Some(rho_agent_types::SendKind::Result),
            content: vec![],
        },
        None,
    );
    let second = write.post_mail(UnixMs(6), to_a("two"), Some(78));
    assert_eq!(
        (first, again, other, second),
        (MailId(1), MailId(1), MailId(2), MailId(3))
    );
    assert!(write.delete_workset(ws));
    write.commit();
    let read = db.read();
    assert_eq!(
        read.inbox(Address::Agent(a), MailId(0))
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>(),
        vec![MailId(1), MailId(3)]
    );
    assert_eq!(read.inbox(Address::Agent(a), MailId(1)).len(), 1);
    assert_eq!(read.mail_head(), MailId(3));
    assert!(read.workset(ws).is_none());
    assert!(read.agent(a).is_none());
    assert!(read.workset_rows(ws, Wpos::ZERO, 10).is_empty());
}
