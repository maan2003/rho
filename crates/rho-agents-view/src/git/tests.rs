//! The cards as the window deals them, with a host whose requests the test
//! says it heard.

use std::cell::RefCell;

use gpui::{Entity, TestAppContext, WindowHandle};
use rho_agent_hosts::git::{GitAnswered, GitAsk, GitChange, GitEvent, GitStreams};
use rho_agent_hosts::protocol::{
    GitCommit, GitDecision, GitDiff, GitIntent, GitPush, GitRefChange, GitRefUpdate,
    GitTransportRequest,
};
use rho_agent_hosts::{AttachTarget, HostSpec};
use rho_agent_types::{AgentId, AgentIdDomain};
use rho_frame::{AnySystem, Frame, Surface as _};

use super::surface::Shown;
use super::*;

fn asker() -> AgentId {
    AgentId::from_counter(1, &AgentIdDomain(0)).unwrap()
}

/// An agent whose work has no thread.
fn loner() -> AgentId {
    AgentId::from_counter(2, &AgentIdDomain(0)).unwrap()
}

fn request(intent: GitIntent, agent: Option<AgentId>) -> GitTransportRequest {
    GitTransportRequest {
        host: "github.com".to_owned(),
        port: 22,
        user: "git".to_owned(),
        repository: "acme/repo".to_owned(),
        intent,
        agent,
    }
}

/// A push that creates `refs` and says nothing more.
fn push(refs: &[&str], agent: Option<AgentId>) -> GitTransportRequest {
    rich(
        refs.iter()
            .map(|reference| GitRefUpdate {
                reference: (*reference).to_owned(),
                change: GitRefChange::Create {
                    new: "1".repeat(40),
                },
            })
            .collect(),
        agent,
    )
}

fn rich(updates: Vec<GitRefUpdate>, agent: Option<AgentId>) -> GitTransportRequest {
    request(
        GitIntent::Push(GitPush {
            updates,
            commits: Vec::new(),
            commit_count: 0,
            diffs: Vec::new(),
            diff_cut: false,
        }),
        agent,
    )
}

struct Window {
    frame: WindowHandle<Frame>,
    git: Entity<GitApprovals>,
    lab: HostId,
}

fn window(cx: &mut TestAppContext) -> Window {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
        gpui_tokio::init(cx);
    });
    let git = Rc::new(RefCell::new(None));
    let made = git.clone();
    let frame = cx.add_window(|window, cx| {
        let handle = window.window_handle().downcast::<Frame>().unwrap();
        let system = cx.new(|cx| {
            GitApprovals::new(
                GitStreams::new(),
                Rc::new(|agent, _| {
                    if agent == asker() {
                        Some(KnownAgent {
                            name: "eng-q9tz".to_owned(),
                            work: Some("tide · Flaky clock test".to_owned()),
                        })
                    } else {
                        (agent == loner()).then(|| KnownAgent {
                            name: "eng-ka35".to_owned(),
                            work: None,
                        })
                    }
                }),
                FrameLink::new(handle),
                cx,
            )
        });
        *made.borrow_mut() = Some(system.clone());
        Frame::new(
            rho_db::RhoDb::in_memory(),
            vec![AnySystem::new(system)],
            Vec::new(),
            Rc::new(|| {}),
            window,
            cx,
        )
    });
    let lab = frame
        .update(cx, |frame, _, cx| {
            frame.attach_host(
                HostSpec {
                    name: "lab".to_owned(),
                    target: AttachTarget::Local {
                        runtime_dir: "/nonexistent/lab".into(),
                    },
                },
                cx,
            )
        })
        .unwrap();
    cx.run_until_parked();
    let git = git.borrow_mut().take().unwrap();
    Window { frame, git, lab }
}

impl Window {
    /// The host says `request` waits, asked `ago` minutes back; the
    /// answer arrives at what this returns.
    fn asks(
        &self,
        id: u64,
        request: GitTransportRequest,
        ago: i64,
        cx: &mut TestAppContext,
    ) -> GitAnswered {
        let at = Timestamp::now() - jiff::SignedDuration::from_mins(ago);
        let (ask, answered) = GitAsk::for_test(id, request, at.as_millisecond() as u64);
        self.hears(GitChange::Asked(ask), cx);
        answered
    }

    fn hears(&self, change: GitChange, cx: &mut TestAppContext) {
        let host = self.lab;
        self.git
            .update(cx, |git, cx| git.heard(GitEvent { host, change }, cx));
        cx.run_until_parked();
    }

    fn hand(&self, cx: &mut TestAppContext) -> Vec<(String, String, String)> {
        self.frame
            .update(cx, |frame, _, _| {
                frame
                    .hand()
                    .cards
                    .iter()
                    .map(|card| {
                        (
                            card.address.key().to_owned(),
                            card.heading(),
                            card.label.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap()
    }

    fn current(&self, cx: &mut TestAppContext) -> String {
        self.frame
            .update(cx, |frame, _, _| frame.current_address().key().to_owned())
            .unwrap()
    }

    /// The text of the card's screen in view.
    fn screen(&self, cx: &mut TestAppContext) -> String {
        self.frame
            .update(cx, |frame, _, cx| {
                let surface = frame
                    .current_surface()
                    .view()
                    .downcast::<GitAskSurface>()
                    .unwrap();
                surface.read(cx).editor(cx).unwrap().read(cx).text(cx)
            })
            .unwrap()
    }

    fn deal(&self, cx: &mut TestAppContext) {
        self.frame
            .update(cx, |frame, window, cx| frame.pull_card(window, cx))
            .unwrap();
        cx.run_until_parked();
    }

    /// Opens the verdict menu over the card in view and presses `key`, as
    /// `space` then `key` does.
    fn press(&self, key: &str, cx: &mut TestAppContext) {
        cx.dispatch_action(self.frame.into(), rho_frame::keys::VerdictMenu);
        cx.run_until_parked();
        let rows = self
            .frame
            .update(cx, |frame, _, _| frame.menu_open())
            .unwrap();
        assert!(rows, "the verdict menu is open");
        cx.simulate_keystrokes(self.frame.into(), key);
        cx.run_until_parked();
    }
}

fn card(id: u64) -> String {
    GitAskAddress(id).address().key().to_owned()
}

/// Two agents push and one fetches at once: three cards, each saying
/// which work asks, or who when the work has no thread, what it does and
/// where, the oldest first. Nothing opens by itself.
#[gpui::test]
fn two_pushes_at_once_are_two_cards(cx: &mut TestAppContext) {
    let window = window(cx);
    let _main = window.asks(0xa1, push(&["refs/heads/main"], Some(asker())), 5, cx);
    let _tag = window.asks(
        0xb2,
        push(&["refs/heads/master", "refs/tags/v1"], None),
        2,
        cx,
    );
    let _fetch = window.asks(0xc3, request(GitIntent::Fetch, Some(loner())), 1, cx);
    assert_eq!(
        window.hand(cx),
        [
            (
                card(0xa1),
                "tide · Flaky clock test › push branch main to github.com/acme/repo".to_owned(),
                "waits · 5m".to_owned(),
            ),
            (
                card(0xb2),
                "lab › push branch master, tag v1 to github.com/acme/repo".to_owned(),
                "waits · 2m".to_owned(),
            ),
            (
                card(0xc3),
                "eng-ka35@lab › fetch from github.com/acme/repo".to_owned(),
                "waits · 1m".to_owned(),
            ),
        ]
    );
    assert_eq!(
        window.current(cx),
        Frame::desk_address().key(),
        "nothing popped up"
    );
}

/// Dealt, the card's screen says exactly what the push would do; `space
/// a` hands the approval to the host and deals the next card, and `space
/// d` refuses that one.
#[gpui::test]
fn approve_and_deny_answer_the_host_and_deal_on(cx: &mut TestAppContext) {
    let window = window(cx);
    let mut main = window.asks(0xa1, push(&["refs/heads/main"], Some(asker())), 5, cx);
    let mut tag = window.asks(0xb2, push(&["refs/tags/v1"], Some(asker())), 1, cx);
    window.deal(cx);
    assert_eq!(window.current(cx), card(0xa1));
    let text = window.screen(cx);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[..7],
        [
            "tide · Flaky clock test",
            "eng-q9tz@lab asks to push with your ssh login",
            "",
            "repository ssh://github.com:22/acme/repo",
            "changes, from where the agent's repository last saw them",
            "  branch main  new at 11111111",
            "0 commits",
        ]
    );
    assert!(lines[7].starts_with("asked "));
    assert_eq!(
        lines.last(),
        Some(&"space a approves · space d denies · space s snoozes")
    );
    assert!(main.try_recv().is_err(), "dealing answers nothing");

    window.press("a", cx);
    assert_eq!(main.try_recv(), Ok(GitDecision::Approve));
    assert_eq!(window.current(cx), card(0xb2), "the next card is dealt");
    assert_eq!(window.hand(cx).len(), 1);

    window.press("d", cx);
    assert_eq!(tag.try_recv(), Ok(GitDecision::Deny));
    assert!(window.hand(cx).is_empty());
    assert_eq!(window.current(cx), Frame::desk_address().key());
}

/// `space s` snoozes the card: it leaves the deal for an hour, and the
/// host hears nothing, so the push still waits.
#[gpui::test]
fn snooze_puts_the_card_away_and_answers_nothing(cx: &mut TestAppContext) {
    let window = window(cx);
    let mut main = window.asks(0xa1, push(&["refs/heads/main"], Some(asker())), 5, cx);
    window.deal(cx);
    window.press("s", cx);
    cx.simulate_keystrokes(window.frame.into(), "h");
    cx.run_until_parked();
    assert!(window.hand(cx).is_empty(), "snoozed out of the deal");
    assert!(main.try_recv().is_err(), "the host heard nothing");
    assert!(
        window
            .git
            .read_with(cx, |git, _| git.waiting.contains_key(&GitAskAddress(0xa1)))
    );
}

fn date(seconds: i64) -> String {
    Timestamp::from_second(seconds)
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%Y-%m-%d")
        .to_string()
}

/// A push's screen says which work asks, each ref it changes with a
/// force-push called out, the commits it carries, and its diff, and says
/// where it stops; the verdicts come before the diff. What the agent's
/// repository wrote shows no control characters.
#[test]
fn a_push_screen_shows_its_commits_and_diff() {
    let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
    let commit = |short_id: &str, subject: &str, authored_at| GitCommit {
        short_id: short_id.to_owned(),
        author: "Ada Lovelace".to_owned(),
        authored_at,
        subject: subject.to_owned(),
    };
    let GitIntent::Push(mut push) = rich(
        vec![
            GitRefUpdate {
                reference: "refs/heads/main".to_owned(),
                change: GitRefChange::FastForward {
                    old: a.clone(),
                    new: b.clone(),
                },
            },
            GitRefUpdate {
                reference: "refs/heads/rewrite".to_owned(),
                change: GitRefChange::Force {
                    old: c.clone(),
                    new: b.clone(),
                    dropped: Some(2),
                },
            },
            GitRefUpdate {
                reference: "refs/heads/stale".to_owned(),
                change: GitRefChange::Force {
                    old: c.clone(),
                    new: b.clone(),
                    dropped: None,
                },
            },
            GitRefUpdate {
                reference: "refs/tags/v1".to_owned(),
                change: GitRefChange::Create { new: b.clone() },
            },
            GitRefUpdate {
                reference: "refs/heads/gone".to_owned(),
                change: GitRefChange::Delete { old: Some(a) },
            },
        ],
        None,
    )
    .intent
    else {
        unreachable!()
    };
    push.commits = vec![
        commit("bbbbbbb", "Test the clock", 1_764_000_180),
        commit("ddddddd", "Fix the \u{202e}clock", 1_700_000_000),
    ];
    push.commit_count = 7;
    push.diffs = vec![GitDiff {
        reference: "refs/heads/main".to_owned(),
        text: " clock.rs | 2 +-\n\n--- a/clock.rs\n+++ b/clock.rs\n-tick\n+\ttock\n".to_owned(),
    }];
    push.diff_cut = true;
    let shown = Shown {
        title: "push".to_owned(),
        work: Some("tide · Flaky clock test".to_owned()),
        asker: "eng-q9tz@lab".to_owned(),
        destination: "ssh://github.com:22/acme/repo".to_owned(),
        push: Some(push),
        asked: Timestamp::from_second(1_764_000_000).unwrap(),
    };
    let text = shown.text();
    let lines: Vec<&str> = text.lines().collect();
    let asked = lines
        .iter()
        .position(|line| line.starts_with("asked "))
        .unwrap();
    assert_eq!(
        lines[..asked],
        [
            "tide · Flaky clock test".to_owned(),
            "eng-q9tz@lab asks to push with your ssh login".to_owned(),
            String::new(),
            "repository ssh://github.com:22/acme/repo".to_owned(),
            "changes, from where the agent's repository last saw them".to_owned(),
            "  branch main  aaaaaaaa..bbbbbbbb".to_owned(),
            "  branch rewrite  FORCE-PUSH cccccccc..bbbbbbbb · drops 2 commits".to_owned(),
            "  branch stale  FORCE-PUSH cccccccc..bbbbbbbb · drops commits the agent never \
             fetched"
                .to_owned(),
            "  tag v1  new at bbbbbbbb".to_owned(),
            "  branch gone  deleted, was aaaaaaaa".to_owned(),
            "7 commits".to_owned(),
            format!(
                "  bbbbbbb Test the clock · Ada Lovelace · {}",
                date(1_764_000_180)
            ),
            format!(
                "  ddddddd Fix the \u{fffd}clock · Ada Lovelace · {}",
                date(1_700_000_000)
            ),
            "  and 5 more".to_owned(),
        ]
    );
    assert_eq!(
        lines[asked + 1..],
        [
            "",
            "space a approves · space d denies · space s snoozes",
            "",
            "diff of branch main",
            " clock.rs | 2 +-",
            "",
            "--- a/clock.rs",
            "+++ b/clock.rs",
            "-tick",
            "+    tock",
            "",
            "the diff stops here: it is longer than 64 KiB",
        ]
    );
}

/// A fetch's screen names the work and the repository, and says that
/// Git names the refs only after the approval.
#[test]
fn a_fetch_screen_says_what_it_can() {
    let shown = Shown {
        title: "fetch".to_owned(),
        work: None,
        asker: "lab".to_owned(),
        destination: "ssh://git.sr.ht:22/~alice/repo".to_owned(),
        push: None,
        asked: Timestamp::from_second(1_764_000_000).unwrap(),
    };
    let text = shown.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[..4],
        [
            "lab asks to fetch with your ssh login",
            "",
            "repository ssh://git.sr.ht:22/~alice/repo",
            "a fetch changes nothing there · Git names the refs it wants once you approve",
        ]
    );
    assert_eq!(
        lines.last(),
        Some(&"space a approves · space d denies · space s snoozes")
    );
}

/// A request that ends unanswered, because its agent was stopped or
/// another device answered, takes its card and its screen with it; a host
/// whose stream goes takes every card it asked, and only those.
#[gpui::test]
fn a_request_that_ends_takes_its_card_and_screen(cx: &mut TestAppContext) {
    let window = window(cx);
    let _first = window.asks(0xa1, push(&["refs/heads/main"], None), 5, cx);
    let _second = window.asks(0xb2, push(&["refs/heads/master"], None), 1, cx);
    window.deal(cx);
    assert_eq!(window.current(cx), card(0xa1));
    window.hears(GitChange::Ended { request_id: 0xa1 }, cx);
    assert_ne!(window.current(cx), card(0xa1), "its screen closed");
    assert_eq!(
        window
            .hand(cx)
            .into_iter()
            .map(|(address, ..)| address)
            .collect::<Vec<_>>(),
        [card(0xb2)]
    );
    // Another host's request is not this host's to forget.
    let other = HostId(window.lab.0 + 1);
    let (ask, _elsewhere) = GitAsk::for_test(0xc3, push(&["refs/heads/main"], None), 0);
    window.git.update(cx, |git, cx| {
        git.heard(
            GitEvent {
                host: other,
                change: GitChange::Asked(ask),
            },
            cx,
        )
    });
    window.hears(GitChange::Lost, cx);
    assert_eq!(
        window
            .hand(cx)
            .into_iter()
            .map(|(address, ..)| address)
            .collect::<Vec<_>>(),
        [card(0xc3)]
    );
}

/// An answer that finds the host's stream gone says so, and the card
/// still goes: the host asks again when it is back.
#[gpui::test]
fn an_answer_to_a_host_that_has_gone_says_so(cx: &mut TestAppContext) {
    let window = window(cx);
    let answered = window.asks(0xa1, push(&["refs/heads/main"], None), 1, cx);
    drop(answered);
    let said = window.git.update(cx, |git, _| {
        git.answer(GitAskAddress(0xa1), GitDecision::Approve)
    });
    assert_eq!(said, "lab: not connected · it asks again when it is back");
    assert!(window.git.read_with(cx, |git, _| git.waiting.is_empty()));
}

#[test]
fn a_card_address_reads_back() {
    let address = GitAskAddress(0x00ff_1234_abcd_0001);
    assert_eq!(address.address().key(), "git:00ff1234abcd0001");
    assert_eq!(
        GitAskAddress::from_address(&address.address()),
        Some(address)
    );
    assert_eq!(GitAskAddress::from_rest("ff"), None);
}
