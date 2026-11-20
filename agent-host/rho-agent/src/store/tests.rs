//! The store's folds, against a disposable database.

use rho_agent_types::{
    Address, ChatEvent, ChatSeq, ContentPart, Mention, MentionKind, MessageId, Role, UnixMs,
    WorksetId, WorksetKind, Wpos,
};
use rho_db::RhoDb;

use super::*;
use crate::inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use crate::record::Runtime;

fn spec(name: &str) -> AgentSpec {
    AgentSpec {
        runtime: Runtime::Native {
            model: InferenceModel::Sol,
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

fn ids(messages: &[rho_agent_types::Message]) -> Vec<MessageId> {
    messages.iter().map(|message| message.id).collect()
}

/// A message reaches the members of the thread it is posted in and of the
/// threads above it, by the nearest record for each agent, and whom it
/// mentions, never its sender. A resend under a client id is one message.
#[tokio::test]
async fn chat_delivers_down_the_tree_and_to_the_mentioned_but_never_the_sender() {
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
    // A new store's chat is empty: the first root is its first event. A
    // root reaches nobody but whom it mentions.
    let rho = write
        .post(UnixMs(3), None, said(Address::User, "rho"), None)
        .unwrap();
    assert_eq!(rho, MessageId(ChatSeq(1)));
    // The user starts work under the root; a resend under its client id
    // is the same message and logs nothing.
    let work = write
        .post(
            UnixMs(4),
            Some(rho),
            said(Address::User, "Fix it"),
            Some(78),
        )
        .unwrap();
    assert_eq!(work, MessageId(ChatSeq(2)));
    let again = write
        .post(
            UnixMs(4),
            Some(rho),
            said(Address::User, "resent"),
            Some(78),
        )
        .unwrap();
    assert_eq!(again, work);
    assert_eq!(db_head(&mut write), 2);
    // `a` joins the root: it hears everything under it. `b` joins the work.
    write.join(UnixMs(5), rho, a).unwrap();
    write.join(UnixMs(5), work, b).unwrap();
    // Joining again logs nothing.
    write.join(UnixMs(5), work, b).unwrap();
    assert_eq!(db_head(&mut write), 4);
    let both = write
        .post(UnixMs(6), Some(work), said(Address::User, "both"), None)
        .unwrap();
    // `b` replies: `a` hears it through the root, `b` is the sender.
    let reply = write
        .post(
            UnixMs(7),
            Some(work),
            said(Address::Agent(b), "me too"),
            None,
        )
        .unwrap();
    // A subthread under `b`'s reply reaches both through their joins above.
    let deeper = write
        .post(UnixMs(8), Some(reply), said(Address::User, "deeper"), None)
        .unwrap();
    // `b` leaves the subthread: it hears nothing more there, `a` still does.
    write.leave(UnixMs(9), reply, b).unwrap();
    let after_leave = write
        .post(UnixMs(10), Some(reply), said(Address::User, "b gone"), None)
        .unwrap();
    // But a mention reaches it, and reaches `c`, who is nowhere, without
    // joining either.
    let mention = write
        .post(
            UnixMs(11),
            Some(reply),
            NewMessage {
                mentions: vec![
                    Mention {
                        to: Address::Agent(b),
                        kind: MentionKind::Ask,
                    },
                    Mention {
                        to: Address::Agent(c),
                        kind: MentionKind::Fyi,
                    },
                ],
                ..said(Address::User, "b, c, look")
            },
            None,
        )
        .unwrap();
    // Leaving where nothing made you a member logs nothing; where a join
    // above did, it does.
    let head = db_head(&mut write);
    write.leave(UnixMs(12), work, c).unwrap();
    assert_eq!(db_head(&mut write), head);
    write.leave(UnixMs(12), work, a).unwrap();
    assert_eq!(db_head(&mut write), head + 1);
    let elsewhere = write
        .post(
            UnixMs(13),
            Some(rho),
            said(Address::User, "still the root"),
            None,
        )
        .unwrap();
    // Refused, and logs nothing: an empty message, a parent that is not.
    for (parent, text) in [(Some(work), "  "), (Some(MessageId(ChatSeq(999))), "x")] {
        assert!(
            write
                .post(UnixMs(14), parent, said(Address::User, text), None)
                .is_err()
        );
    }
    let seqs = db_head(&mut write);
    write.commit();

    let read = db.read();
    assert_eq!(read.chat_head(), ChatSeq(seqs));
    assert_eq!(
        received(&read, a, 0),
        [both, reply, deeper, after_leave, mention, elsewhere].map(|id| id.0.0)
    );
    assert_eq!(
        received(&read, b, 0),
        [both, deeper, mention].map(|id| id.0.0)
    );
    assert_eq!(received(&read, c, 0), [mention.0.0]);
    assert_eq!(
        received(&read, a, both.0.0),
        [reply, deeper, after_leave, mention, elsewhere].map(|id| id.0.0)
    );
    // The tree as it stands.
    assert_eq!(ids(&read.thread_messages(None, 10)), [rho]);
    assert_eq!(ids(&read.thread_messages(Some(rho), 10)), [work, elsewhere]);
    assert_eq!(ids(&read.thread_messages(Some(work), 1)), [reply]);
    assert_eq!(
        ids(&read.thread_messages(Some(reply), 10)),
        [deeper, after_leave, mention]
    );
    assert_eq!(read.message(deeper).unwrap().parent, Some(reply));
    // Members as a new message under each would see them: the root's `a`
    // left the work, `b` left the reply.
    assert_eq!(read.members_under(Some(rho)), [a]);
    assert_eq!(read.members_under(Some(work)), [b]);
    assert_eq!(read.members_under(Some(reply)), Vec::<AgentId>::new());
    assert_eq!(read.members_under(None), Vec::<AgentId>::new());
    let thread = read.thread(reply).unwrap();
    assert_eq!(thread.name, "me too");
    assert_eq!(
        thread.path,
        vec![
            (rho, "rho".to_owned()),
            (work, "Fix it".to_owned()),
            (reply, "me too".to_owned())
        ]
    );
    assert!(read.thread(MessageId(ChatSeq(999))).is_none());
    assert!(
        read.message(MessageId(ChatSeq(3))).is_none(),
        "a join is not a message"
    );
}

/// A move is a parent change: the subtree follows, nothing is delivered
/// again, and a move to where a message is logs nothing, as does a name a
/// thread has.
#[tokio::test]
async fn chat_moves_subtrees_and_names_threads() {
    let (_dir, db) = db().await;
    let (a, b) = (agent(1), agent(2));
    let mut write = db.write().await;
    let rho = write
        .post(UnixMs(1), None, said(Address::User, "rho"), None)
        .unwrap();
    let ops = write
        .post(UnixMs(1), None, said(Address::User, "ops"), None)
        .unwrap();
    let work = write
        .post(UnixMs(2), Some(rho), said(Address::User, "Fix it"), None)
        .unwrap();
    write.join(UnixMs(3), work, a).unwrap();
    let kept = write
        .post(UnixMs(4), Some(work), said(Address::User, "one"), None)
        .unwrap();
    let aside = write
        .post(
            UnixMs(5),
            Some(kept),
            said(Address::Agent(b), "an aside"),
            None,
        )
        .unwrap();
    // Refused, and logs nothing: under itself, under its own child, a
    // message that is not.
    let before = db_head(&mut write);
    assert!(write.reparent(UnixMs(6), work, Some(work)).is_err());
    assert!(write.reparent(UnixMs(6), work, Some(aside)).is_err());
    assert!(
        write
            .reparent(UnixMs(6), MessageId(ChatSeq(999)), Some(ops))
            .is_err()
    );
    assert!(
        write
            .reparent(UnixMs(6), kept, Some(MessageId(ChatSeq(999))))
            .is_err()
    );
    assert_eq!(db_head(&mut write), before);
    // Where it is: nothing.
    write.reparent(UnixMs(6), work, Some(rho)).unwrap();
    assert_eq!(db_head(&mut write), before);
    // The work moves to ops; `kept` and `aside` go with it.
    write.reparent(UnixMs(7), work, Some(ops)).unwrap();
    // `kept` becomes a root of its own, taking `aside`.
    write.reparent(UnixMs(8), kept, None).unwrap();
    // Names: given, the same again, taken back.
    write.name(UnixMs(9), work, "  Fixed   it ".into()).unwrap();
    let named = db_head(&mut write);
    write.name(UnixMs(9), work, "Fixed it".into()).unwrap();
    assert_eq!(
        db_head(&mut write),
        named,
        "a name a thread has logs nothing"
    );
    write.name(UnixMs(10), aside, "An aside".into()).unwrap();
    write.name(UnixMs(11), aside, String::new()).unwrap();
    // The host's own naming yields to a name a thread has, and to nothing.
    assert!(
        !write
            .name_unnamed(UnixMs(12), work, "Other".into())
            .unwrap()
    );
    assert!(!write.name_unnamed(UnixMs(12), aside, "  ".into()).unwrap());
    assert!(
        write
            .name_unnamed(UnixMs(12), aside, "Set aside".into())
            .unwrap()
    );
    assert!(
        !write
            .name_unnamed(UnixMs(12), aside, "Again".into())
            .unwrap()
    );
    assert!(
        write
            .name(UnixMs(11), MessageId(ChatSeq(999)), "x".into())
            .is_err()
    );
    write.commit();

    let read = db.read();
    assert_eq!(ids(&read.thread_messages(None, 10)), [rho, ops, kept]);
    assert_eq!(
        ids(&read.thread_messages(Some(rho), 10)),
        Vec::<MessageId>::new()
    );
    assert_eq!(ids(&read.thread_messages(Some(ops), 10)), [work]);
    assert_eq!(
        ids(&read.thread_messages(Some(work), 10)),
        Vec::<MessageId>::new()
    );
    assert_eq!(ids(&read.thread_messages(Some(kept), 10)), [aside]);
    assert_eq!(read.message(work).unwrap().parent, Some(ops));
    assert_eq!(read.message(kept).unwrap().parent, None);
    assert_eq!(read.message(aside).unwrap().parent, Some(kept));
    let thread = read.thread(work).unwrap();
    assert_eq!(thread.name, "Fixed it");
    assert_eq!(
        thread.path,
        vec![(ops, "ops".to_owned()), (work, "Fixed it".to_owned())]
    );
    assert_eq!(thread.members, [a]);
    assert_eq!(read.thread(aside).unwrap().name, "Set aside");
    assert_eq!(
        read.thread(aside).unwrap().path,
        vec![(kept, "one".to_owned()), (aside, "Set aside".to_owned())]
    );
    // `a` heard what came while it was a member; the moves delivered
    // nothing.
    assert_eq!(received(&read, a, 0), vec![kept.0.0, aside.0.0]);
    let events: Vec<_> = read.chat_since(ChatSeq::ZERO, 100);
    let moves = events
        .iter()
        .filter(|record| matches!(record.event, ChatEvent::Reparented { .. }))
        .count();
    let names = events
        .iter()
        .filter(|record| matches!(record.event, ChatEvent::Named { .. }))
        .count();
    assert_eq!((moves, names), (2, 4));
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

#[tokio::test]
async fn an_answered_ask_keeps_its_body_even_if_its_answer_moves() {
    use rho_agent_types::Revision;

    let (_dir, db) = db().await;
    let mut write = db.write().await;
    let (a, b) = (agent(1), agent(2));
    let at = UnixMs(20);
    let channel = write
        .post(at, None, said(Address::User, "rho"), None)
        .unwrap();
    let thread = write
        .post(at, Some(channel), said(Address::User, "Fix it"), None)
        .unwrap();
    let other = write
        .post(at, Some(channel), said(Address::User, "Other idea"), None)
        .unwrap();
    let mut asked = said(Address::Agent(a), "Which clock?");
    asked.mentions.push(Mention {
        to: Address::User,
        kind: MentionKind::Ask,
    });
    let ask = write.post(at, Some(thread), asked, None).unwrap();
    let revised = said(Address::Agent(a), "Which monotonic clock?").content;
    write
        .post(
            at,
            Some(other),
            said(Address::User, "Use the old host"),
            None,
        )
        .unwrap();
    assert!(
        write
            .edit(at, b, ask, revised.clone())
            .unwrap_err()
            .to_string()
            .contains("own mail")
    );
    write.edit(at, a, ask, revised.clone()).unwrap();
    let answer = write
        .post(at, Some(thread), said(Address::User, "Use mock time"), None)
        .unwrap();
    write.reparent(at, answer, Some(other)).unwrap();
    write.reparent(at, ask, Some(other)).unwrap();
    let err = write
        .edit(at, a, ask, said(Address::Agent(a), "Ship it").content)
        .unwrap_err();
    assert!(err.to_string().contains("already answered"));
    assert!(
        write
            .delete(at, a, ask)
            .unwrap_err()
            .to_string()
            .contains("already answered")
    );

    let note = write
        .post(
            at,
            Some(thread),
            said(Address::Agent(a), "Transient report"),
            None,
        )
        .unwrap();
    let reply = write
        .post(
            at,
            Some(note),
            said(Address::User, "Context to retain"),
            None,
        )
        .unwrap();
    write.delete(at, a, note).unwrap();
    assert!(
        write
            .edit(at, a, note, revised)
            .unwrap_err()
            .to_string()
            .contains("deleted")
    );
    write.commit();

    let read = db.read();
    let ask = read.message(ask).unwrap();
    assert_eq!(ask.text(), "Which monotonic clock?");
    assert_eq!(ask.revision, Revision::Edited);
    assert_eq!(ask.mentions(Address::User), Some(MentionKind::Ask));
    let removed = read.message(note).unwrap();
    assert_eq!(removed.revision, Revision::Deleted);
    assert!(removed.text().is_empty());
    assert_eq!(read.message(reply).unwrap().parent, Some(note));
}

#[tokio::test]
async fn a_stop_is_logged_once_per_agent_per_stop_and_is_not_in_the_inbox() {
    let (_dir, db) = db().await;
    let mut write = db.write().await;
    let (a, b) = (agent(1), agent(2));
    let at = UnixMs(90);
    let root = write
        .post(at, None, said(Address::User, "tide"), None)
        .unwrap();
    let work = write
        .post(at, Some(root), said(Address::User, "Carry on"), None)
        .unwrap();
    write.join(at, work, a).unwrap();
    write.join(at, work, b).unwrap();
    let sender = Address::System {
        agent: a,
        stop: rho_agent_types::Stop::Lost,
        since: UnixMs(75),
    };
    let stop = write
        .post_stop(at, work, sender, "lost · worker exited".into())
        .unwrap()
        .unwrap();
    write.reparent(at, stop, Some(root)).unwrap();
    write.commit();

    let mut write = db.write().await;
    assert_eq!(
        write
            .post_stop(UnixMs(96), work, sender, "repeat".into())
            .unwrap(),
        None
    );
    let next = Address::System {
        agent: a,
        stop: rho_agent_types::Stop::Lost,
        since: UnixMs(95),
    };
    let again = write
        .post_stop(UnixMs(96), work, next, "lost again".into())
        .unwrap()
        .unwrap();
    write.commit();
    let read = db.read();
    assert_eq!(read.message(stop).unwrap().from, sender);
    assert_eq!(
        read.message(again).unwrap().mentions(Address::User),
        Some(MentionKind::Ask)
    );
    assert!(read.inbox(a, ChatSeq::ZERO).is_empty());
    assert!(read.inbox(b, ChatSeq::ZERO).is_empty());
}

#[tokio::test]
async fn mail_search_sees_the_current_subject_and_body_only_inside_scope() {
    let (_dir, db) = db().await;
    let mut write = db.write().await;
    let at = UnixMs(7);
    let a = agent(7);
    let tide = write
        .post(at, None, said(Address::User, "tide"), None)
        .unwrap();
    let reef = write
        .post(at, None, said(Address::User, "reef"), None)
        .unwrap();
    let topic = write
        .post(at, Some(tide), said(Address::User, "Set it up"), None)
        .unwrap();
    write
        .name(at, topic, "NAPI socket strategy".into())
        .unwrap();
    let reply = write
        .post(
            at,
            Some(topic),
            said(Address::Agent(a), "Use protocol Polling."),
            None,
        )
        .unwrap();
    let removed = write
        .post(
            at,
            Some(topic),
            said(Address::Agent(a), "Never NAPI socket."),
            None,
        )
        .unwrap();
    write
        .edit(
            at,
            a,
            reply,
            said(Address::Agent(a), "Use a native NAPI socket.").content,
        )
        .unwrap();
    write.delete(at, a, removed).unwrap();
    let different = write
        .post(
            at,
            Some(reef),
            said(Address::User, "Test NAPI socket."),
            None,
        )
        .unwrap();
    write.commit();

    let read = db.read();
    assert_eq!(read.project_named(" tide "), Some(tide));
    assert_eq!(
        read.project_named("NAPI socket strategy"),
        None,
        "a subject is not a project address"
    );
    let scoped = read.search_mail("Socket NAPI", Some(tide), 10).unwrap();
    assert_eq!(
        scoped
            .iter()
            .map(|hit| (hit.thread, hit.message))
            .collect::<Vec<_>>(),
        [(topic, reply), (topic, topic)]
    );
    assert_eq!(
        (&*scoped[0].subject, &*scoped[0].excerpt),
        ("NAPI socket strategy", "Use a native NAPI socket.")
    );
    assert_eq!(
        read.search_mail("socket NAPI", None, 1).unwrap()[0].message,
        different
    );
    assert!(read.search_mail("polling", None, 10).unwrap().is_empty());
    assert!(read.search_mail("NAPI", Some(topic), 10).is_err());
}
