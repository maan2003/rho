use rho_agent_types::{ChatEvent, ChatRecord, UnixMs};
use rho_frame::Frame;

use super::*;

fn corpus() -> HostChat {
    let agent = Party::Agent(AgentId::from_counter(7, &rho_agent_types::AgentIdDomain(0)).unwrap());
    let mut log = HostChat::default();
    for seq in 1..=10_001 {
        let parent = if seq == 1 {
            None
        } else if (seq - 2) % 100 == 0 {
            Some(1)
        } else {
            Some(2 + ((seq - 2) / 100) * 100)
        };
        let first = match seq % 101 {
            1 => "The clock behavior changes when two jobs overlap. The result is pending.",
            2 => "The clock can move forward without affecting this run.",
            _ => "The worker finished the run and recorded the output.",
        };
        let text = format!(
            "{first} {seq}\n\n{}",
            concat!(
                "We checked the fixtures and the run configuration after the latest change. ",
                "The result was consistent with the earlier attempt. The output is available ",
                "in the report along with the details needed to try it again.\n\n",
                "The next step is to compare the measurements with the original test run. ",
                "We can do that after this check is complete and write down the difference."
            )
            .repeat(3)
        );
        log.tell(&ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(1_760_000_000_000 + seq),
            event: ChatEvent::Posted {
                parent: parent.map(|id| MessageId(ChatSeq(id))),
                retry_of: None,
                thread_reply: None,
                from: if seq == 1 { Party::User } else { agent },
                voice: rho_agent_types::Voice::Message,
                content: vec![ContentPart::Text { text }],
            },
        });
    }
    log.tell(&ChatRecord {
        seq: ChatSeq(10_002),
        at: UnixMs(1_760_000_010_002),
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

pub(super) fn fixture(
    cx: &mut gpui::TestAppContext,
    log: HostChat,
) -> (
    gpui::WindowHandle<Frame>,
    Entity<ChatSystem>,
    tempfile::TempDir,
) {
    let temp = tempfile::tempdir().unwrap();
    let db = rho_db::RhoDb::open(temp.path().join("rho.redb"));
    let (window, mail) = fixture_on(cx, log, db);
    (window, mail, temp)
}

/// A window over chat alone, on `db`, with `log` as the host
/// `EndpointId([3; 32])`'s.
pub(super) fn fixture_on(
    cx: &mut gpui::TestAppContext,
    log: HostChat,
    db: rho_db::RhoDb,
) -> (gpui::WindowHandle<Frame>, Entity<ChatSystem>) {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
        cx.bind_keys([gpui::KeyBinding::new(
            "backspace",
            editor::actions::Backspace,
            Some("Editor"),
        )]);
    });
    let mut mail = None;
    let window = cx.add_window(|window, cx| {
        let frame = FrameLink::new(window.window_handle().downcast::<Frame>().unwrap());
        let notes = cx.new(|cx| rho_notes::NoteSystem::new(frame.clone(), cx));
        let system = cx.new(|cx| {
            let mut system = ChatSystem::new(db.clone(), frame, notes, cx);
            system.logs.insert(EndpointId([3; 32]), log);
            system
        });
        let systems = vec![rho_frame::AnySystem::new(system.clone())];
        mail = Some(system);
        Frame::new(db, systems, Vec::new(), Rc::new(|| {}), window, cx)
    });
    (window, mail.unwrap())
}

#[gpui::test]
#[ignore = "performance benchmark"]
fn bench_palette_find_text(cx: &mut gpui::TestAppContext) {
    let (_window, mail, _db) = fixture(cx, corpus());
    cx.update(|cx| {
        let mail = mail.read(cx);
        let current = Address::new("frame", "desk");
        let mut bench = criterion::Criterion::default().without_plots();
        for query in ["c", "clock", "clock beh"] {
            bench.bench_function(&format!("palette/find-text-10000/{query}"), |b| {
                b.iter(|| {
                    let mut search = TextSearch::new(std::hint::black_box(query).to_owned(), 100);
                    mail.text_sources(&mut search, &current, cx);
                    let mut present = mail.text_presenter(cx).unwrap();
                    search
                        .find(&std::sync::atomic::AtomicBool::new(false))
                        .iter()
                        .filter_map(&mut present)
                        .collect::<Vec<_>>()
                })
            });
            bench.bench_function(&format!("palette/ui-10000/{query}"), |b| {
                let mut search = TextSearch::new(query.to_owned(), 100);
                mail.text_sources(&mut search, &current, cx);
                let hits = search.find(&std::sync::atomic::AtomicBool::new(false));
                b.iter(|| {
                    let mut search = TextSearch::new(std::hint::black_box(query).to_owned(), 100);
                    mail.text_sources(&mut search, &current, cx);
                    let mut present = mail.text_presenter(cx).unwrap();
                    let rows = hits.iter().filter_map(&mut present).collect::<Vec<_>>();
                    (search, rows)
                })
            });
        }
        bench.final_summary();
    });
}

#[gpui::test]
#[ignore = "performance benchmark"]
fn bench_palette_ui_keys(cx: &mut gpui::TestAppContext) {
    use std::time::Instant;

    // Opt-in: record timings for the Palette's own work independently of
    // GPUI's whole-window dispatch and paint, without printing query text.
    let _profile = std::env::var_os("RHO_PALETTE_TRACE").map(|_| {
        let reporter = tracing_subscriber::fmt()
            .with_env_filter("rho_frame::minibuffer=trace")
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_default(reporter)
    });

    let (frame, _mail, _db) = fixture(cx, corpus());
    let window = *frame;
    cx.run_until_parked();
    cx.draw_window(window);
    let mut key_ms = Vec::new();
    let mut frame_ms = Vec::new();
    for _ in 0..20 {
        cx.dispatch_action(window, rho_frame::keys::Palette);
        cx.draw_window(window);
        cx.simulate_input(window, "chat: ");
        cx.draw_window(window);
        for key in ["c", "l", "o", "c", "k", "space", "b", "e", "h", "backspace"] {
            let key = gpui::Keystroke::parse(key).unwrap();
            let start = Instant::now();
            cx.dispatch_keystroke(window, key);
            key_ms.push(start.elapsed().as_secs_f64() * 1000.);
            cx.run_until_parked();
            let draw = Instant::now();
            cx.draw_window(window);
            frame_ms.push(draw.elapsed().as_secs_f64() * 1000.);
        }
        frame
            .update(cx, |frame, _, cx| {
                assert_eq!(frame.minibuffer_mut().unwrap().input(cx), "chat: clock be");
            })
            .unwrap();
        cx.dispatch_action(window, rho_frame::keys::MinibufferCancel);
    }
    for (what, mut times) in [("dispatch", key_ms), ("whole-window draw", frame_ms)] {
        times.sort_unstable_by(f64::total_cmp);
        println!(
            "palette UI {what}, {} real keypresses (ms): median {:.3}, p95 {:.3}, max {:.3}",
            times.len(),
            times[times.len() / 2],
            times[(times.len() * 95 / 100) - 1],
            times[times.len() - 1]
        );
    }
}
