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

/// `notes` lists every note, a reply under something too, by its first
/// line, the one last written first: an older note edited last goes on
/// top, a deleted one goes, and one never typed into is not there yet. A
/// reply says where it shows, by the name `place` gives, or that it is a
/// reply when nothing names the place.
#[test]
fn notes_lists_every_note_last_written_first() {
    let mut world = World::default();
    let older = world.note("older\nand more");
    let reply = world.note("a reply");
    world.say(Fact::About {
        node: reply.address(),
        about: Some(thread(7)),
    });
    let stray = world.note("about a thread no host names");
    world.say(Fact::About {
        node: stray.address(),
        about: Some(thread(8)),
    });
    let newer = world.note("");
    let gone = world.note("gone");
    world.write(gone, "gone", true);
    let fresh = NoteId(uuid::Uuid::new_v4());
    world.say(Fact::About {
        node: fresh.address(),
        about: Some(thread(7)),
    });
    world.write(older, "older, edited last", false);
    let place = |about: &Address| (*about == thread(7)).then(|| "fix the flaky test".to_owned());
    let listed = |note: NoteId, title: &str, reply: Option<Option<&str>>| Listed {
        note: note.address(),
        title: title.to_owned(),
        reply: reply.map(|place| place.map(str::to_owned)),
    };
    assert_eq!(
        super::listed(&world.marks, place),
        [
            listed(older, "older, edited last", None),
            listed(newer, UNTITLED, None),
            listed(stray, "about a thread no host names", Some(None)),
            listed(reply, "a reply", Some(Some("fix the flaky test"))),
        ]
    );
}

/// A reply's line reads its first line, then, dim, where it shows; a note
/// about nothing is its first line alone.
#[test]
fn a_reply_says_where_it_shows() {
    let note = NoteId(uuid::Uuid::new_v4()).address();
    let row = |reply: Option<Option<&str>>| {
        list::row(&Listed {
            note: note.clone(),
            title: "verify on CI too".to_owned(),
            reply: reply.map(|place| place.map(str::to_owned)),
        })
    };
    let (text, styles) = row(Some(Some("fix the flaky test")));
    assert_eq!(text, "  verify on CI too  ↳ in fix the flaky test");
    let dim: Vec<_> = styles
        .iter()
        .filter(|(class, _)| *class == list::Class::Muted)
        .map(|(_, range)| &text[range.clone()])
        .collect();
    assert_eq!(dim, ["↳ in fix the flaky test"]);
    assert_eq!(row(Some(None)).0, "  verify on CI too  ↳ private reply");
    let (text, styles) = row(None);
    assert_eq!(text, "  verify on CI too");
    assert!(styles.iter().all(|(class, _)| *class != list::Class::Muted));
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
