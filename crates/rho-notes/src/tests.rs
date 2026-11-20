use jiff::Timestamp;
use jiff::tz::TimeZone;
use rho_dealer::facts::{Device, Entry};

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

/// The palette lists every note that has text, by its first line, a
/// private reply as one, with a later revision newer; a deleted note is
/// not there.
#[test]
fn the_palette_finds_every_note_by_its_first_line() {
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
    let now = Timestamp::from_second(1_800_000_000).unwrap();
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
    };
    let (text, styles) = list::row(&listed, now);
    assert_eq!(
        text,
        "  verify on CI too  it failed last night  ↳ Can this ship before noon on Friday? · Fix the flaky test  · 7m"
    );
    assert_eq!(&text[styles[0].1.clone()], "verify on CI too");
    assert_eq!(styles[1].0, list::Class::Muted);
    assert_eq!(
        &text[styles[1].1.clone()],
        &text["  verify on CI too".len()..]
    );

    listed.reply = Some(ReplyContext::default());
    assert!(list::row(&listed, now).0.contains("↳ private reply"));
    listed.reply = None;
    assert_eq!(
        list::row(&listed, now).0,
        "  verify on CI too  it failed last night  · 7m"
    );
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
