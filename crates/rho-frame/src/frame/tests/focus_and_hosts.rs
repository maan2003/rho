//! Dealer focus, `Places` states, commands run from another place, and
//! hosts that are away, over a system of threads in two projects.

use std::cell::RefCell;

use jiff::SignedDuration;
use rho_agent_hosts::hosts::HostStatus;

use super::*;
use crate::command::Spelling;
use crate::focus::DealerFocus;
use crate::hosts::Notice;
use crate::system::FoundThread;

static STOP_AGENT: Spelling = Spelling {
    name: "stop agent",
    aliases: &[("kill", 9)],
    argument: None,
};

/// What the test sets: the claims, the host every thread lives on, and
/// where `stop agent` was run from.
#[derive(Default)]
struct World {
    claims: Vec<rho_dealer::Claim>,
    host: Option<HostId>,
    stopped: Vec<Address>,
}

/// Threads in two projects, `tide` and `reef`.
struct Work(Rc<RefCell<World>>);

fn tide() -> Address {
    Address::new("thread", "tide")
}

fn reef() -> Address {
    Address::new("thread", "reef")
}

/// `Flaky clock test`, in tide, at work.
fn clock() -> Address {
    Address::new("thread", "tide/1")
}

/// `Retry window`, in reef.
fn retry() -> Address {
    Address::new("thread", "reef/1")
}

impl crate::System for Work {
    fn kinds(&self) -> &'static [&'static str] {
        &["thread"]
    }

    fn open(&mut self, _: &Address, _: &mut Window, cx: &mut Context<Self>) -> Opening {
        Opening::Surface(AnySurface::new(cx.new(|cx| OpenCard(cx.focus_handle()))))
    }

    fn claims(
        &self,
        _: &rho_dealer::Marks,
        _: Timestamp,
        _: Option<&mut rho_dealer::rank::Trace>,
        _: &App,
    ) -> Vec<rho_dealer::Claim> {
        self.0.borrow().claims.clone()
    }

    fn project(&self, address: &Address, _: &App) -> Option<crate::Project> {
        let name = address.rest().split('/').next()?;
        matches!(name, "tide" | "reef").then(|| crate::Project {
            address: Address::new("thread", name),
            name: name.to_owned(),
        })
    }

    fn host(&self, _: &Address, _: &App) -> Option<HostId> {
        self.0.borrow().host
    }

    fn find(&self, _: &rho_dealer::Marks, _: &App) -> Vec<Found> {
        let found = |address: Address,
                     title: &str,
                     project: &str,
                     work: Option<Option<rho_agent_types::ThreadWork>>| Found {
            address,
            title: title.into(),
            description: project.into(),
            thread: work.map(|work| FoundThread {
                status: Some("Checking timing".into()),
                work,
            }),
            recency: 0,
            also: Vec::new(),
        };
        vec![
            found(tide(), "tide", "", None),
            found(reef(), "reef", "", None),
            found(
                clock(),
                "Flaky clock test",
                "tide",
                Some(Some(rho_agent_types::ThreadWork::Working)),
            ),
            found(retry(), "Retry window", "reef", Some(None)),
        ]
    }

    /// `stop agent`, acting on the agent of the place it is run from.
    fn commands(&mut self, point: &Point, _: &mut Window, _: &mut Context<Self>) -> Vec<Command> {
        let world = self.0.clone();
        let surface = point.surface.clone();
        vec![
            Command::simple(&STOP_AGENT, move |_, _, _| {
                world.borrow_mut().stopped.push(surface.clone())
            })
            .acts_on(format!("agent of {}", point.surface.rest())),
        ]
    }
}

/// A frame over [`Work`], and how often its chime rang.
fn work_frame(
    cx: &mut gpui::TestAppContext,
) -> (
    gpui::WindowHandle<Frame>,
    Rc<RefCell<World>>,
    Rc<std::cell::Cell<u32>>,
) {
    let world = Rc::new(RefCell::new(World::default()));
    let rings = Rc::new(std::cell::Cell::new(0));
    // As `test_frame_with`, with a chime that counts.
    let _ = test_frame(cx, rho_db::RhoDb::in_memory());
    let system = world.clone();
    let counted = rings.clone();
    let frame = cx.add_window(|window, cx| {
        let chime = Rc::new(move || counted.set(counted.get() + 1));
        Frame::new(
            rho_db::RhoDb::in_memory(),
            vec![AnySystem::new(cx.new(|_| Work(system)))],
            Vec::new(),
            chime,
            window,
            cx,
        )
    });
    (frame, world, rings)
}

/// Runs `f`, then lets the lamp and the chime catch up.
fn settle(
    frame: &gpui::WindowHandle<Frame>,
    cx: &mut gpui::TestAppContext,
    f: impl FnOnce(&mut Frame, &mut Window, &mut Context<Frame>),
) {
    frame
        .update(cx, |frame, window, cx| {
            f(frame, window, cx);
            frame.systems_changed(cx);
        })
        .unwrap();
    cx.run_until_parked();
}

fn on_page(frame: &mut Frame, window: &mut Window, cx: &mut Context<Frame>) {
    let page = Address::new("page", "7");
    let view = AnySurface::new(cx.new(|cx| Page(cx.focus_handle())));
    frame.surfaces.insert(page.clone(), view);
    frame.open(page, window, cx);
}

fn hours(hours: i64) -> SignedDuration {
    SignedDuration::from_hours(hours)
}

/// The deal answers to a project focus: tide's older card waits while
/// reef's deals, and an empty focused deal says where it looked. Desk
/// still counts both.
#[gpui::test]
fn a_project_focus_deals_only_its_cards_while_desk_counts_all(cx: &mut gpui::TestAppContext) {
    let (frame, world, _) = work_frame(cx);
    world.borrow_mut().claims = vec![
        titled(asks(&clock(), hours(3), false), "Flaky clock test"),
        titled(asks(&retry(), hours(1), false), "Retry window"),
    ];
    settle(&frame, cx, |frame, window, cx| {
        run_line(frame, "focus reef", window, cx);
        assert_eq!(frame.dealer_focus(), &DealerFocus::Project(reef()));
        assert_eq!(echo_of(frame).as_deref(), Some("dealer focus: reef"));
        frame.pull_card(window, cx);
        assert_eq!(
            frame.current_address(),
            &retry(),
            "tide's older card is out of focus"
        );
        frame.pull_card(window, cx);
        assert_eq!(
            echo_of(frame).as_deref(),
            Some("only waiting card: Retry window")
        );
    });
    frame
        .update(cx, |frame, window, cx| {
            assert_eq!(frame.hand().cards.len(), 2, "Desk counts every card");
            assert_eq!(
                frame.focus_note(Timestamp::now(), cx).as_deref(),
                Some("focus reef")
            );
            // The bar says the focus on every screen, once the echo is
            // gone.
            assert_eq!(frame.status_notes(cx).focus, None, "the echo has the room");
            frame.echo = None;
            assert_eq!(frame.status_notes(cx).focus.as_deref(), Some("focus reef"));
            // The host answers the done: reef's card goes.
            world
                .borrow_mut()
                .claims
                .retain(|claim| claim.address != retry());
            frame.verdict(&retry(), Verdict::Done, "done".into(), window, cx);
            assert!(frame.desk_in_view(), "nothing else deals in reef");
            assert_eq!(
                echo_of(frame).as_deref(),
                Some("done: Retry window · nothing waits in reef")
            );
            frame.pull_card(window, cx);
            assert!(frame.desk_in_view(), "tide still waits, out of focus");
            assert_eq!(echo_of(frame).as_deref(), Some("nothing waits in reef"));
            run_line(frame, "unfocus", window, cx);
            assert_eq!(frame.dealer_focus(), &DealerFocus::All);
            assert_eq!(echo_of(frame).as_deref(), Some("dealer focus: all"));
            frame.pull_card(window, cx);
            assert_eq!(frame.current_address(), &clock());
        })
        .unwrap();
}

/// What follows `focus`, `quiet` and `dnd` on the line is the focus; with
/// nothing after it, the project in view.
#[gpui::test]
fn focus_words_on_the_line_set_the_focus(cx: &mut gpui::TestAppContext) {
    let (frame, _, _) = work_frame(cx);
    frame
        .update(cx, |frame, window, cx| {
            frame.open(clock(), window, cx);
            run_line(frame, "focus", window, cx);
            assert_eq!(
                frame.dealer_focus(),
                &DealerFocus::Project(tide()),
                "in view"
            );
            let quiet_for = |frame: &Frame| match frame.dealer_focus() {
                DealerFocus::QuietUntil(until) => Timestamp::now().duration_until(*until),
                other => panic!("{other:?}"),
            };
            run_line(frame, "quiet 45m", window, cx);
            let left = quiet_for(frame);
            assert!(
                left > SignedDuration::from_mins(44) && left <= SignedDuration::from_mins(45),
                "{left:?}"
            );
            assert_eq!(
                frame.focus_note(Timestamp::now(), cx).as_deref(),
                Some("quiet 45m")
            );
            run_line(frame, "dnd 2h", window, cx);
            assert!(quiet_for(frame) > SignedDuration::from_mins(119));
            run_line(frame, "focus off", window, cx);
            assert_eq!(frame.dealer_focus(), &DealerFocus::All);
            run_line(frame, "set dealer focus project reef", window, cx);
            assert_eq!(frame.dealer_focus(), &DealerFocus::Project(reef()));
            run_line(frame, "clear dealer focus", window, cx);
            assert_eq!(frame.dealer_focus(), &DealerFocus::All);
            run_line(frame, "focus kelp", window, cx);
            assert_eq!(
                echo_of(frame).as_deref(),
                Some("set dealer focus: no project called kelp")
            );
            assert_eq!(frame.dealer_focus(), &DealerFocus::All, "nothing changed");
        })
        .unwrap();
}

/// The lamp and the chime answer to the focus too: a card out of focus
/// neither lights nor rings, a quiet silences both, and a focus cleared
/// later rings for nothing that crossed meanwhile.
#[gpui::test]
fn the_lamp_and_chime_keep_to_the_focus(cx: &mut gpui::TestAppContext) {
    let (frame, world, rings) = work_frame(cx);
    let lamp = |frame: &gpui::WindowHandle<Frame>, cx: &mut gpui::TestAppContext| {
        frame
            .update(cx, |frame, _, _| frame.signals.lamp_on)
            .unwrap()
    };
    settle(&frame, cx, on_page);
    settle(&frame, cx, |frame, _, cx| {
        frame.set_dealer_focus(DealerFocus::Project(reef()), cx)
    });
    world
        .borrow_mut()
        .claims
        .push(asks(&clock(), SignedDuration::ZERO, true));
    settle(&frame, cx, |_, _, _| {});
    assert_eq!(rings.get(), 0, "tide is out of focus");
    assert!(!lamp(&frame, cx));
    world
        .borrow_mut()
        .claims
        .push(asks(&retry(), SignedDuration::ZERO, true));
    settle(&frame, cx, |_, _, _| {});
    assert_eq!(rings.get(), 1, "reef is in focus");
    assert!(lamp(&frame, cx));

    settle(&frame, cx, |frame, _, cx| {
        frame.set_dealer_focus(
            DealerFocus::QuietUntil(Timestamp::now() + SignedDuration::from_mins(45)),
            cx,
        )
    });
    assert!(!lamp(&frame, cx), "quiet");
    let later = Address::new("thread", "reef/2");
    world
        .borrow_mut()
        .claims
        .push(asks(&later, SignedDuration::ZERO, true));
    settle(&frame, cx, |_, _, _| {});
    assert_eq!(rings.get(), 1, "quiet");

    settle(&frame, cx, |frame, _, cx| {
        frame.set_dealer_focus(DealerFocus::All, cx)
    });
    assert!(lamp(&frame, cx));
    assert_eq!(
        rings.get(),
        1,
        "what crossed in the quiet does not ring after it"
    );
}

/// A night asleep is not time unseen: a card that came in during it does
/// not ring on waking, though one that waited as long awake does, and an
/// answer to work the user just did still rings at once.
#[gpui::test]
fn the_chime_counts_only_time_awake(cx: &mut gpui::TestAppContext) {
    let (frame, world, rings) = work_frame(cx);
    settle(&frame, cx, on_page);
    world
        .borrow_mut()
        .claims
        .push(asks(&clock(), hours(3), false));
    settle(&frame, cx, |_, _, _| {});
    assert_eq!(rings.get(), 1, "three hours awake and unseen");

    // The lid was shut for the last nine hours.
    settle(&frame, cx, |frame, _, _| {
        let awake = &mut frame.signals.awake;
        awake.observe(Timestamp::now() - hours(9), crate::awake::awake_clock());
        awake.observe(Timestamp::now(), crate::awake::awake_clock());
        assert!(awake.has_slept());
    });
    world
        .borrow_mut()
        .claims
        .push(asks(&retry(), hours(3), false));
    settle(&frame, cx, |_, _, _| {});
    assert_eq!(rings.get(), 1, "asked three hours ago, all of it asleep");

    let answered = Address::new("thread", "reef/2");
    world
        .borrow_mut()
        .claims
        .push(asks(&answered, SignedDuration::ZERO, true));
    settle(&frame, cx, |_, _, _| {});
    assert_eq!(rings.get(), 2, "an answer to what the user just wrote");
}

/// `muted:`, `snoozed:`, `stopped:` and `working:` read what the user said
/// and what the deal holds, so a muted thread is found by its state.
#[gpui::test]
fn places_finds_threads_by_their_state(cx: &mut gpui::TestAppContext) {
    let (frame, world, _) = work_frame(cx);
    let mut stop = asks(&retry(), hours(1), false);
    stop.kind = rho_dealer::rank::CardKind::Stop("failed");
    world.borrow_mut().claims = vec![titled(stop, "Retry window")];
    settle(&frame, cx, |_, _, _| {});
    let rows = |frame: &mut Frame, typed: &str, window: &mut Window, cx: &mut Context<Frame>| {
        frame.places(window, cx);
        let prompt = frame.minibuffer_mut().expect("places");
        prompt.set_input(typed.to_owned(), window, cx);
        frame.refresh_minibuffer(cx);
        let rows = frame.minibuffer.as_ref().unwrap().row_names();
        frame.minibuffer_cancel(window, cx);
        rows
    };
    frame
        .update(cx, |frame, window, cx| {
            assert_eq!(rows(frame, "working:", window, cx), ["Flaky clock test"]);
            assert_eq!(rows(frame, "stopped:", window, cx), ["Retry window"]);
            assert!(rows(frame, "muted:", window, cx).is_empty());
            frame.verdict(&clock(), Verdict::Mute, "mute".into(), window, cx);
            assert_eq!(rows(frame, "muted:", window, cx), ["Flaky clock test"]);
            assert_eq!(
                rows(frame, "muted: flaky", window, cx),
                ["Flaky clock test"]
            );
            assert!(rows(frame, "muted: retry", window, cx).is_empty());
            frame.verdict(
                &retry(),
                Verdict::Snooze(rho_dealer::Until::In(hours(1))),
                "snooze".into(),
                window,
                cx,
            );
            assert_eq!(rows(frame, "snoozed:", window, cx), ["Retry window"]);
            assert_eq!(rows(frame, "snoozed: reef", window, cx), ["Retry window"]);
        })
        .unwrap();
}

/// `stop agent clock` stops the agent of the thread `clock` finds while
/// another screen is in view; the row says so before enter.
#[gpui::test]
fn words_after_a_command_run_it_from_the_place_they_name(cx: &mut gpui::TestAppContext) {
    let (frame, world, _) = work_frame(cx);
    frame
        .update(cx, |frame, window, cx| {
            frame.open(retry(), window, cx);
            frame.commands_prompt(window, cx);
            let prompt = frame.minibuffer_mut().unwrap();
            prompt.set_input("stop agent clock".to_owned(), window, cx);
            frame.refresh_minibuffer(cx);
            assert_eq!(
                frame.minibuffer.as_ref().unwrap().command_rows()[0],
                (
                    "stop agent".to_owned(),
                    Some("clock".to_owned()),
                    Some("Flaky clock test · tide".to_owned())
                ),
                "the name, the words apart, and the place they found"
            );
            frame.minibuffer_confirm(window, cx);
            assert_eq!(world.borrow().stopped, [clock()]);
            assert_eq!(
                frame.current_address(),
                &retry(),
                "the user stays where they are"
            );

            // Without words, the place in view.
            frame.commands_prompt(window, cx);
            let rows = frame.minibuffer.as_ref().unwrap().command_rows();
            let stop = rows.iter().find(|row| row.0 == "stop agent").unwrap();
            assert_eq!(stop.2.as_deref(), Some("agent of reef/1"));
            frame.minibuffer_cancel(window, cx);
            run_line(frame, "stop agent", window, cx);
            assert_eq!(world.borrow().stopped, [clock(), retry()]);

            // An alias takes a place too; words that find none run nothing.
            run_line(frame, "kill flaky", window, cx);
            assert_eq!(world.borrow().stopped.last(), Some(&clock()));
            frame.commands_prompt(window, cx);
            answer(frame, "stop agent nowhere", window, cx);
            assert_eq!(world.borrow().stopped.len(), 3, "no match runs nothing");
            frame.minibuffer_cancel(window, cx);
        })
        .unwrap();
}

/// A host away is everyday: its cached cards count and deal, a verdict on
/// one lands on this device, its threads say `lab away` rather than
/// `working`, and only the screen that needs it turns its notice red.
#[gpui::test]
fn an_away_hosts_cached_cards_stay_answerable(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_tokio::init);
    let (frame, world, _) = work_frame(cx);
    world.borrow_mut().claims = vec![titled(asks(&clock(), hours(2), false), "Flaky clock test")];
    frame
        .update(cx, |frame, window, cx| {
            let spec = HostSpec {
                name: "lab".to_owned(),
                target: rho_agent_hosts::AttachTarget::Local {
                    runtime_dir: "/nonexistent/lab".into(),
                },
            };
            let (host, _) = frame.attach_host_in_process(spec, cx);
            world.borrow_mut().host = Some(host);
            frame
                .hosts
                .set_status(host, HostStatus::Disconnected("connection lost".into()));

            frame.pull_card(window, cx);
            assert_eq!(frame.current_address(), &clock(), "the cached card deals");
            assert_eq!(
                frame.host_notices(cx),
                (
                    Some("lab".to_owned()),
                    vec![Notice {
                        words: "lab away".into(),
                        alarm: true
                    }]
                ),
                "this screen needs lab"
            );
            assert_eq!(
                crate::desk::HostsAway::of(frame.hosts()).note().as_deref(),
                Some("lab away")
            );
            let row = frame
                .place_rows(None, cx)
                .into_iter()
                .find(|place| place.found.address == clock())
                .unwrap();
            assert_eq!(row.away.as_deref(), Some("lab away"));
            assert!(
                !row.states.contains(&places::PlaceState::Working),
                "not known to work while lab is away"
            );

            world.borrow_mut().claims.clear();
            frame.verdict(&clock(), Verdict::Done, "done".into(), window, cx);
            assert!(frame.attention.can_undo(), "the verdict is this device's");
            assert_eq!(
                echo_of(frame).as_deref(),
                Some("done: Flaky clock test · nothing waits · lab away")
            );
            assert!(frame.desk_in_view());
            assert_eq!(
                frame.host_notices(cx),
                (
                    None,
                    vec![Notice {
                        words: "lab away".into(),
                        alarm: false
                    }]
                ),
                "Desk needs no host: calm"
            );
            // The echo has the bar to itself while it shows; a quiet host
            // note comes back once it is gone.
            assert_eq!(frame.status_notes(cx).notices, []);
            frame.echo = None;
            assert_eq!(
                frame.status_notes(cx).notices,
                [Notice {
                    words: "lab away".into(),
                    alarm: false
                }]
            );
        })
        .unwrap();
}

/// Reconnecting clears that host's stale away echo, while leaving any
/// other echo on the line alone.
#[gpui::test]
fn reconnecting_clears_that_hosts_stale_away_echo(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_tokio::init);
    let (frame, _, _) = work_frame(cx);
    frame
        .update(cx, |frame, _, cx| {
            let spec = HostSpec {
                name: "lab".to_owned(),
                target: rho_agent_hosts::AttachTarget::Local {
                    runtime_dir: "/nonexistent/lab".into(),
                },
            };
            let (host, _) = frame.attach_host_in_process(spec, cx);
            frame.host_event(
                host,
                rho_agent_hosts::connection::ConnEvent::Disconnected("connection lost".into()),
                cx,
            );
            assert_eq!(
                echo_of(frame).as_deref(),
                Some("lab away · connection lost")
            );
            frame.host_event(host, rho_agent_hosts::connection::ConnEvent::Recovered, cx);
            assert_eq!(echo_of(frame), None);

            frame.echo("reef away · connection lost", StyleClass::SystemInfo, cx);
            frame.host_event(host, rho_agent_hosts::connection::ConnEvent::Ready, cx);
            assert_eq!(
                echo_of(frame).as_deref(),
                Some("reef away · connection lost")
            );
        })
        .unwrap();
}
