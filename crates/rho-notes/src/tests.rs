use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, Zoned};
use rho_dealer::Until;
use rho_dealer::facts::{Device, Entry, Seen};
use rho_frame::when::{SnoozeUnit, snooze_target};

use super::*;

const LAPTOP: Device = Device([1; 16]);

/// The user's marks, said one at a time a minute apart.
#[derive(Default)]
struct World {
    marks: Marks,
    minute: i64,
}

impl World {
    fn at(&mut self) -> jiff::Zoned {
        self.minute += 1;
        Timestamp::from_second(1_800_000_000 + self.minute * 60)
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    fn say(&mut self, fact: Fact) {
        let at = self.at();
        self.marks.apply([Entry {
            device: LAPTOP,
            at,
            fact,
        }]);
    }

    fn note(&mut self, body: &str) -> NoteId {
        let note = NoteId(uuid::Uuid::new_v4());
        self.write(note, body, false);
        note
    }

    fn write(&mut self, note: NoteId, body: &str, deleted: bool) {
        let created = self.marks.get(&note.address()).created_ms.map_or_else(
            || self.at().timestamp(),
            |ms| Timestamp::from_millisecond(ms).unwrap(),
        );
        self.say(Fact::Wrote {
            note: note.0,
            text: body.to_owned(),
            created,
        });
        if deleted {
            self.say(Fact::Deleted { note: note.0 });
        }
    }
}

/// The instant `minute` minutes after the test worlds' start, in UTC.
fn minute(minute: i64) -> Zoned {
    Timestamp::from_second(1_800_000_000 + minute * 60)
        .unwrap()
        .to_zoned(TimeZone::UTC)
}

/// The hand at `now`, as the notes claim it: `title · label`, top first.
fn hand(marks: &Marks, now: &Zoned) -> Vec<String> {
    let claims = reminder_claims(marks, |note| note_title(marks, &note.address()));
    let skips = rho_dealer::Skips::default();
    let sources = rho_dealer::rank::Sources {
        claims: &claims,
        marks,
        skips: &skips,
    };
    rho_dealer::rank(&sources, now)
        .cards
        .iter()
        .map(|card| format!("{} · {}", card.heading(), card.label))
        .collect()
}

fn snooze(note: NoteId, until: Until) -> Fact {
    Fact::Snooze {
        node: note.address(),
        until,
    }
}

fn thread(n: u32) -> Address {
    Address::new("thread", format!("00000000000000ff/{n}"))
}

#[test]
fn a_note_has_one_address() {
    let id = uuid::Uuid::new_v4();
    let note = NoteId(id);
    assert_eq!(note.address(), Address::note(id));
    assert_eq!(NoteId::from_address(&Address::note(id)), Some(note));
    // The same id written another way is not the note's address.
    let hyphenated = Address::new(Address::NOTE, id.hyphenated());
    assert_eq!(NoteId::from_address(&hyphenated), None);
    assert_eq!(
        NoteId::from_address(&Address::new(
            Address::NOTE,
            id.simple().to_string().to_uppercase()
        )),
        None
    );
    // A label an older build minted under the same id is no note.
    assert_eq!(
        NoteId::from_address(&Address::new("label", id.simple())),
        None
    );
    assert_eq!(NoteId::from_address(&thread(1)), None);
}

#[test]
fn only_a_text_that_moved_is_written() {
    let mut world = World::default();
    let note = world.note("milk\neggs");
    let marks = &world.marks;
    assert!(write_for(marks, note, "milk\neggs".into()).is_none());
    match write_for(marks, note, "milk\neggs\nbread".into()) {
        Some(Write::Note {
            note: id,
            body: Some(body),
            deleted: None,
        }) => {
            assert_eq!(id, note.0);
            assert_eq!(body, "milk\neggs\nbread");
        }
        other => panic!("not a revision: {other:?}"),
    }
    // A note nobody wrote yet, left empty, says nothing.
    let fresh = NoteId(uuid::Uuid::new_v4());
    assert!(write_for(marks, fresh, String::new()).is_none());
}

/// The notes about a thing are its private replies, oldest first; a
/// deleted one is not.
#[test]
fn notes_about_a_thing_are_its_private_replies() {
    let mut world = World::default();
    let first = world.note("about seven");
    let other = world.note("about eight");
    let later = world.note("about seven, later");
    for (note, thread) in [(first, 7), (other, 8), (later, 7)] {
        world.say(Fact::About {
            node: note.address(),
            about: Some(self::thread(thread)),
        });
    }
    let replies = |world: &World, n| -> Vec<Address> {
        world
            .marks
            .replies_to(&thread(n))
            .into_iter()
            .map(|(address, _)| address.clone())
            .collect()
    };
    assert_eq!(replies(&world, 7), [first.address(), later.address()]);
    assert_eq!(replies(&world, 9), []);
    world.write(first, "about seven", true);
    assert_eq!(replies(&world, 7), [later.address()]);
}

/// A reply says what it is about and nothing else; a new note says
/// nothing at all, so once typed into it is a note about nothing.
#[test]
fn a_new_note_is_about_where_it_was_made_or_nothing() {
    let note = NoteId(uuid::Uuid::new_v4());
    assert!(new_note_marks(note, None).is_empty());
    match new_note_marks(note, Some(&thread(7))).as_slice() {
        [Write::Fact(fact)] => assert_eq!(
            *fact,
            Fact::About {
                node: note.address(),
                about: Some(thread(7)),
            }
        ),
        other => panic!("not one subject: {other:?}"),
    }
}

/// Text far beyond the preview still narrows other matches; two terms
/// can be on different lines, case and order do not matter, and the
/// private mail's words are not part of the user's text.
#[test]
fn notes_searches_the_whole_body_in_last_written_order() {
    let mut world = World::default();
    let older = world
        .note("Renew lease\nbefore Friday bring the thick red folder and the signed copy\nCAFÉ");
    let both = world.note("Signed receipt\nTake it to the Café");
    let one = world.note("A later note\nOnly the signed contract");
    let legacy_empty = world.note("");
    let deleted = world.note("signed CAFÉ");
    world.write(deleted, "signed CAFÉ", true);
    let fresh = NoteId(uuid::Uuid::new_v4());
    for note in [one, fresh] {
        world.say(Fact::About {
            node: note.address(),
            about: Some(thread(7)),
        });
    }
    world.write(
        older,
        "Renew lease\nbefore Friday bring the thick red folder and the signed copy\nCAFÉ",
        false,
    );
    let context = |_: &Address| ReplyContext {
        mail: Some("Meet at a café".into()),
        thread: Some("Pick up the keys".into()),
    };
    let found = |query: &str| listed(&world.marks, query, context);
    let all = found("");
    assert_eq!(
        all.iter().map(|listed| listed.note).collect::<Vec<_>>(),
        [older, one, both],
        "{legacy_empty:?} was a stored empty revision, not a visible note"
    );
    assert_eq!(all[0].preview, "before Friday bring the thick red folder…");
    assert_eq!(
        found("café SIGN")
            .iter()
            .map(|listed| listed.note)
            .collect::<Vec<_>>(),
        [older, both]
    );
    assert_eq!(found("signed copy")[0].note, older);
    assert!(found("pick up").is_empty());
    assert!(found("unmatched").is_empty());
}

#[test]
fn a_cleared_reply_disappears_and_restores_at_the_same_identity() {
    let mut world = World::default();
    let text = "keep my old words\nà part";
    let note = world.note(text);
    let about = thread(31);
    world.say(Fact::About {
        node: note.address(),
        about: Some(about.clone()),
    });
    assert!(matches!(
        write_for(&world.marks, note, String::new()),
        Some(Write::Note { note: id, body: None, deleted: Some(true) }) if id == note.0
    ));
    world.say(Fact::Deleted { note: note.0 });
    assert_eq!(world.marks.get(&note.address()).body.as_deref(), Some(text));
    assert_eq!(text_of(&world.marks, note), "");
    assert!(world.marks.replies_to(&about).is_empty());
    assert!(write_for(&world.marks, note, String::new()).is_none());
    // Text undo says the same body again, even though it is still held
    // under the deletion, and supersedes the tombstone.
    assert!(matches!(
        write_for(&world.marks, note, text.into()),
        Some(Write::Note { note: id, body: Some(body), deleted: None })
            if id == note.0 && body == text
    ));
    world.write(note, text, false);
    assert_eq!(text_of(&world.marks, note), text);
    assert_eq!(world.marks.replies_to(&about)[0].0, &note.address());
}

/// `Places` lists every note that has text, by its first line, a
/// private reply as one, with a later revision newer; a deleted note is
/// not there.
#[test]
fn places_find_every_note_by_its_first_line() {
    let mut world = World::default();
    let older = world.note("older\nand more");
    let reply = world.note("a reply");
    world.say(Fact::About {
        node: reply.address(),
        about: Some(thread(7)),
    });
    let untitled = world.note("\nits first line is intentionally empty");
    let gone = world.note("gone");
    world.write(gone, "gone", true);
    world.write(older, "older, edited last", false);
    let mut found = super::found(&world.marks, |note| {
        note_title(&world.marks, &note.address())
    });
    found.sort_by_key(|found| std::cmp::Reverse(found.recency));
    let seen: Vec<(Address, &str, &str)> = found
        .iter()
        .map(|found| {
            (
                found.address.clone(),
                found.title.as_str(),
                found.description.as_str(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (older.address(), "older, edited last", "note"),
            (untitled.address(), UNTITLED, "note"),
            (reply.address(), "a reply", "private reply"),
        ]
    );
}

#[test]
fn a_reply_shows_its_mails_words_and_its_notes_age_not_just_the_thread() {
    let now = Timestamp::from_second(1_800_000_000)
        .unwrap()
        .to_zoned(TimeZone::UTC);
    let written = Timestamp::from_second(1_800_000_000 - 7 * 60).unwrap();
    let mut listed = Listed {
        note: NoteId(uuid::Uuid::new_v4()),
        title: "verify on CI too".into(),
        preview: "it failed last night".into(),
        written,
        reply: Some(ReplyContext {
            mail: Some("Can this ship before noon on Friday?".into()),
            thread: Some("Fix the flaky test".into()),
        }),
        reminder: None,
    };
    let (text, styles) = list::row(&listed, &now, 0, &chars);
    assert_eq!(
        text,
        "verify on CI too  it failed last night  ↳ Can this ship before noon on Friday? · Fix the flaky test  7m"
    );
    assert_eq!(&text[styles[0].1.clone()], "verify on CI too");
    assert_eq!(styles[1].0, list::Class::Muted);
    assert_eq!(
        &text[styles[1].1.clone()],
        &text["verify on CI too".len()..]
    );

    listed.reply = Some(ReplyContext::default());
    assert!(
        list::row(&listed, &now, 0, &chars)
            .0
            .contains("↳ private reply")
    );
    listed.reply = None;
    assert_eq!(
        list::row(&listed, &now, 0, &chars).0,
        "verify on CI too  it failed last night  7m"
    );
}

/// On the sheet a row's age sits at its right edge, and a long row is cut
/// before it rather than running into it or wrapping under it.
#[test]
fn a_rows_age_is_flush_right_and_a_long_row_is_cut_before_it() {
    let now = Timestamp::from_second(1_800_000_000)
        .unwrap()
        .to_zoned(TimeZone::UTC);
    let mut listed = Listed {
        note: NoteId(uuid::Uuid::new_v4()),
        title: "verify on CI too".into(),
        preview: "it failed last night".into(),
        written: Timestamp::from_second(1_800_000_000 - 7 * 60).unwrap(),
        reply: None,
        reminder: None,
    };
    let (text, _) = list::row(&listed, &now, 60, &chars);
    assert_eq!(text.chars().count(), 60);
    assert!(text.starts_with("verify on CI too  it failed last night  "));
    assert!(text.ends_with(" 7m"), "{text:?}");

    listed.preview = "words ".repeat(20);
    let (text, styles) = list::row(&listed, &now, 40, &chars);
    assert_eq!(text.chars().count(), 40, "{text:?}");
    assert!(text.ends_with("…  7m"), "{text:?}");
    assert_eq!(&text[styles[0].1.clone()], "verify on CI too");

    // Cut inside the title, the title's colour stops at the cut.
    listed.title = "é".repeat(30);
    let (text, styles) = list::row(&listed, &now, 20, &chars);
    assert_eq!(text.chars().count(), 20, "{text:?}");
    assert_eq!(&text[styles[0].1.clone()], "é".repeat(15));
}

/// Every character one column wide.
fn chars(text: &str) -> f32 {
    text.chars().count() as f32
}

/// The list's font is not monospace: a row is padded and cut by how wide
/// its letters are, not by how many there are.
#[test]
fn a_row_of_narrow_letters_is_padded_and_cut_by_their_width() {
    let now = Timestamp::from_second(1_800_000_000)
        .unwrap()
        .to_zoned(TimeZone::UTC);
    let narrow = |text: &str| -> f32 {
        text.chars()
            .map(|character| if character == 'i' { 0.5 } else { 1. })
            .sum()
    };
    let mut listed = Listed {
        note: NoteId(uuid::Uuid::new_v4()),
        title: "iiii".into(),
        preview: String::new(),
        written: Timestamp::from_second(1_800_000_000 - 7 * 60).unwrap(),
        reply: None,
        reminder: None,
    };
    let (text, _) = list::row(&listed, &now, 20, &narrow);
    assert_eq!(narrow(&text), 20., "{text:?}");
    assert_eq!(text.chars().count(), 22, "{text:?}");

    listed.title = "i".repeat(60);
    let (text, styles) = list::row(&listed, &now, 20, &narrow);
    assert_eq!(narrow(&text), 20., "{text:?}");
    assert_eq!(&text[styles[0].1.clone()], "i".repeat(30));
}

/// A row says when its reminder comes due, on the user's clock: the hour
/// today, the day and hour after, and that it is due once it is.
#[test]
fn a_row_says_when_its_reminder_is_due() {
    // Friday 15 January 2027, 08:00 UTC.
    let now = Timestamp::from_second(1_800_000_000)
        .unwrap()
        .to_zoned(TimeZone::UTC);
    let mut listed = Listed {
        note: NoteId(uuid::Uuid::new_v4()),
        title: "Call the dentist".into(),
        preview: String::new(),
        written: now.timestamp() - jiff::SignedDuration::from_mins(7),
        reply: None,
        reminder: None,
    };
    let row = |listed: &Listed| list::row(listed, &now, 0, &chars).0;
    let in_mins = |mins| Some(now.timestamp() + jiff::SignedDuration::from_mins(mins));
    listed.reminder = in_mins(90);
    assert_eq!(row(&listed), "Call the dentist  reminder 09:30  7m");
    listed.reminder = in_mins(25 * 60);
    assert_eq!(
        row(&listed),
        "Call the dentist  reminder Sat 16 Jan 09:00  7m"
    );
    listed.reminder = in_mins(-1);
    assert_eq!(row(&listed), "Call the dentist  reminder due  7m");
    let styles = list::row(&listed, &now, 0, &chars).1;
    assert_eq!(styles[1].0, list::Class::Muted, "the reminder is quiet");
}

/// The title follows the first line as it is typed, before it is written.
#[test]
fn a_note_is_called_by_its_first_line() {
    assert_eq!(
        title_of("Ask Sam about the freeze\nbefore Friday"),
        "Ask Sam about the freeze"
    );
    assert_eq!(title_of("  padded  \nrest"), "padded");
    assert_eq!(title_of(""), UNTITLED);
    assert_eq!(title_of("\nsecond line only"), UNTITLED);
}

#[test]
fn previews_stop_at_word_or_character_limits_without_splitting_utf8() {
    let long = "é".repeat(52);
    assert_eq!(excerpt(long.split_whitespace()), long);
    assert_eq!(
        excerpt(format!("{long} after").split_whitespace()),
        format!("{}…", "é".repeat(51))
    );
    assert_eq!(
        excerpt("1 2 3 4 5 6 7 8".split_whitespace()),
        "1 2 3 4 5 6 7…"
    );
}

/// A window over the notes alone.
fn notes_window(cx: &mut gpui::TestAppContext) -> gpui::WindowHandle<Frame> {
    use std::rc::Rc;

    use gpui::AppContext as _;
    use rho_frame::AnySystem;

    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
    });
    cx.add_window(|window, cx| {
        let frame = FrameLink::new(window.window_handle().downcast::<Frame>().unwrap());
        let notes = cx.new(|cx| NoteSystem::new(frame, cx));
        let db = rho_db::RhoDb::in_memory();
        Frame::new(
            db,
            vec![AnySystem::new(notes)],
            Vec::new(),
            Rc::new(|| {}),
            window,
            cx,
        )
    })
}

/// `space s` on an open note sets its reminder and leaves the note on
/// screen, its status line saying when; it is no card until then.
#[gpui::test]
fn a_reminder_set_on_an_open_note_shows_on_its_status_line(cx: &mut gpui::TestAppContext) {
    let window = notes_window(cx);
    cx.run_until_parked();
    let note = window
        .update(cx, |frame, window, cx| {
            start_note(frame, "Call the dentist".into(), window, cx);
            NoteId::from_address(frame.current_address()).expect("a note in view")
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |frame, window, cx| {
            assert_eq!(frame.current_surface().status(cx), None, "no reminder yet");
            frame.snooze(&note.address(), SnoozeUnit::Hours, Some(2), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |frame, _, cx| {
            assert_eq!(
                frame.current_address(),
                &note.address(),
                "still on the note"
            );
            let status = frame.current_surface().status(cx).expect("the reminder");
            assert!(status.starts_with("reminder "), "{status}");
            assert!(frame.hand().cards.is_empty(), "not yet due");
        })
        .unwrap();
}

/// The find line takes the prompt's editing keys: `ctrl-w` takes only the
/// last word, `ctrl-a` goes to the start without changing the text, and
/// `ctrl-u` clears it.
#[gpui::test]
fn the_find_line_edits_like_a_prompt(cx: &mut gpui::TestAppContext) {
    use gpui::{Focusable as _, VisualTestContext};

    let window = notes_window(cx);
    cx.run_until_parked();
    let list = window
        .update(cx, |frame, window, cx| {
            frame.open(notes_address(), window, cx);
            frame
                .current_surface()
                .view()
                .downcast::<NotesView>()
                .expect("the list of notes")
        })
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let query = list.read_with(&cx, |list, _| list.query.clone());
    cx.update(|window, cx| {
        query.update(cx, |query, cx| query.set_text("find the  milk", window, cx));
        let find = list.read(cx).query.focus_handle(cx);
        window.focus(&find, cx);
        query.update(cx, |query, cx| {
            query.move_to_end(&editor::actions::MoveToEnd, window, cx)
        });
    });
    let text = |cx: &mut VisualTestContext| query.read_with(cx, |query, cx| query.text(cx));

    cx.simulate_keystrokes("ctrl-w");
    assert_eq!(text(&mut cx), "find the  ");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("now ");
    assert_eq!(text(&mut cx), "now find the  ", "ctrl-a moved to the start");
    cx.simulate_keystrokes("ctrl-u");
    assert_eq!(text(&mut cx), "");
}

/// A note with a reminder is no card until the reminder is due; then it
/// is one card, waiting from then, until done, after which it stays off
/// the desk. A note without one, or deleted, never deals.
#[test]
fn a_reminder_deals_once_when_due_and_done_keeps_it_off_the_desk() {
    let mut world = World::default();
    world.note("Buy milk");
    let gone = world.note("Gone");
    let note = world.note("Call the dentist\nask about Friday");
    world.say(snooze(note, Until::In(SignedDuration::from_hours(2))));
    let set = world.minute;
    world.say(snooze(gone, Until::In(SignedDuration::from_hours(1))));
    world.write(gone, "Gone", true);
    assert_eq!(hand(&world.marks, &minute(set + 119)), [] as [&str; 0]);
    assert_eq!(
        hand(&world.marks, &minute(set + 120)),
        ["Call the dentist · due · now"]
    );
    assert_eq!(
        hand(&world.marks, &minute(set + 125)),
        ["Call the dentist · due · 5m"],
        "one card, however often the hand is ranked"
    );

    world.minute = set + 130;
    world.say(Fact::Settled {
        node: note.address(),
        seen: Seen::Whole,
    });
    for later in [set + 131, set + 60 * 24 * 30] {
        assert_eq!(hand(&world.marks, &minute(later)), [] as [&str; 0]);
    }
    assert_eq!(
        listed(&world.marks, "dentist", |_| ReplyContext::default())[0].reminder,
        None
    );
}

/// Snoozed again, the reminder waits for the new time: `tomorrow`, as
/// the deal's snooze names it, is 09:00 the next day on the user's clock.
#[test]
fn a_reminder_snoozed_again_moves_to_the_new_time() {
    let mut world = World::default();
    let note = world.note("Call the dentist");
    world.say(snooze(note, Until::In(SignedDuration::from_hours(1))));
    let set = world.minute;
    assert_eq!(hand(&world.marks, &minute(set + 60)).len(), 1);

    world.minute = set + 61;
    let (tomorrow, said) = snooze_target(SnoozeUnit::Days, 1, &minute(set + 62));
    assert_eq!(said, "tomorrow · 09:00");
    world.say(snooze(note, tomorrow));
    // Saturday 16 January 2027, 09:00 UTC.
    let nine = Timestamp::from_second(1_800_000_000 + 25 * 3600).unwrap();
    let at = |delta: i64| (nine + SignedDuration::from_secs(delta)).to_zoned(TimeZone::UTC);
    assert_eq!(hand(&world.marks, &minute(set + 62)), [] as [&str; 0]);
    assert_eq!(hand(&world.marks, &at(-1)), [] as [&str; 0]);
    assert_eq!(hand(&world.marks, &at(0)), ["Call the dentist · due · now"]);
    assert_eq!(
        listed(&world.marks, "", |_| ReplyContext::default())[0].reminder,
        Some(nine)
    );
}

/// The phone holds what the laptop said once their sealed logs meet: a
/// reminder set on the laptop is due on the phone, and done on the phone
/// takes it off the laptop's desk. What passes between them holds neither
/// the note's words nor the reminder in the clear.
#[test]
fn a_reminder_set_on_one_device_is_due_on_the_other() {
    use rho_db::RhoDb;
    use rho_ledger::{Channel, Ledger, Secret};

    futures_lite::future::block_on(async {
        let scratch = tempfile::tempdir().unwrap();
        let laptop = Ledger::open(RhoDb::open(scratch.path().join("laptop.redb"))).await;
        let phone = Ledger::open(RhoDb::open(scratch.path().join("phone.redb"))).await;
        let secret = Secret::generate();
        laptop.set_secret(secret).await.unwrap();
        phone.set_secret(secret).await.unwrap();
        // Everything `from` holds that `to` does not, as a host carries it.
        let carry = async |from: &Ledger, to: &Ledger, clear: &[Vec<u8>]| {
            for (log, length) in from.lengths() {
                let held = to.lengths().get(&log).copied().unwrap_or(0);
                let bytes = from.bytes_after(log, held, usize::MAX);
                for plain in clear {
                    assert!(
                        !bytes.windows(plain.len()).any(|window| window == plain),
                        "sealed"
                    );
                }
                to.receive(log, held, bytes).await;
                assert_eq!(to.lengths().get(&log), Some(&length));
            }
        };
        let marks = |ledger: &Ledger| {
            let mut marks = Marks::default();
            marks.apply(
                ledger
                    .items(Channel::Facts)
                    .iter()
                    .filter_map(|item| Entry::decode(&item.bytes)),
            );
            marks
        };

        let note = NoteId(uuid::Uuid::new_v4());
        let said = [
            Entry {
                device: Device(laptop.device().0),
                at: minute(0),
                fact: Fact::Wrote {
                    note: note.0,
                    text: "Call the dentist".into(),
                    created: minute(0).timestamp(),
                },
            },
            Entry {
                device: Device(laptop.device().0),
                at: minute(1),
                fact: snooze(note, Until::In(SignedDuration::from_hours(2))),
            },
        ];
        let encoded: Vec<_> = said.iter().map(Entry::encode).collect();
        laptop.append(Channel::Facts, encoded.clone()).await;
        let mut clear = encoded;
        clear.push(b"Call the dentist".to_vec());
        carry(&laptop, &phone, &clear).await;
        assert_eq!(hand(&marks(&phone), &minute(120)), [] as [&str; 0]);
        assert_eq!(
            hand(&marks(&phone), &minute(121)),
            ["Call the dentist · due · now"]
        );

        let done = Entry {
            device: Device(phone.device().0),
            at: minute(125),
            fact: Fact::Settled {
                node: note.address(),
                seen: Seen::Whole,
            },
        };
        phone.append(Channel::Facts, vec![done.encode()]).await;
        carry(&phone, &laptop, &[done.encode()]).await;
        assert_eq!(hand(&marks(&laptop), &minute(126)), [] as [&str; 0]);
    });
}
