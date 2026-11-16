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

    fn label(&mut self, name: &str, parent: Option<LabelId>) -> LabelId {
        let label = LabelId(uuid::Uuid::new_v4());
        self.say(Fact::Label {
            label: label.0,
            name: name.to_owned(),
            parent: parent.map(|parent| parent.0),
        });
        label
    }

    fn file(&mut self, node: &Address, label: LabelId) {
        self.say(Fact::Labeled {
            node: node.clone(),
            label: label.0,
            present: true,
        });
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
    // A label's id is no note.
    assert_eq!(NoteId::from_address(&Address::label(id)), None);
    assert_eq!(
        Thing::from_address(&Address::label(id)),
        Some(Thing::Label(LabelId(id)))
    );
    assert_eq!(Thing::from_address(&thread(1)), None);
}

#[test]
fn only_a_text_that_moved_is_written() {
    let mut world = World::default();
    let note = world.note("milk\neggs");
    let marks = &world.marks;
    assert!(matches!(
        write_for(marks, Thing::Note(note), "milk\neggs".into()),
        Ok(None)
    ));
    match write_for(marks, Thing::Note(note), "milk\neggs\nbread".into()) {
        Ok(Some(Write::Note {
            note: id,
            body: Some(body),
            deleted: None,
        })) => {
            assert_eq!(id, note.0);
            assert_eq!(body, "milk\neggs\nbread");
        }
        other => panic!("not a revision: {other:?}"),
    }
    // A note nobody wrote yet, left empty, says nothing.
    let fresh = NoteId(uuid::Uuid::new_v4());
    assert!(matches!(
        write_for(marks, Thing::Note(fresh), String::new()),
        Ok(None)
    ));
}

#[test]
fn a_label_is_renamed_by_its_first_line_and_keeps_its_place() {
    let mut world = World::default();
    let rho = world.label("rho", None);
    let gui = world.label("gui", Some(rho));
    let marks = &world.marks;
    match write_for(marks, Thing::Label(gui), "  desk \nignored".into()) {
        Ok(Some(Write::Fact(Fact::Label {
            label,
            name,
            parent,
        }))) => {
            assert_eq!(label, gui.0);
            assert_eq!(name, "desk");
            assert_eq!(parent, Some(rho.0));
        }
        other => panic!("not a rename: {other:?}"),
    }
    assert!(write_for(marks, Thing::Label(gui), "a/b".into()).is_err());
    assert!(write_for(marks, Thing::Label(gui), "  \n".into()).is_err());
    // The same name with a second line is no rename.
    assert!(matches!(
        write_for(marks, Thing::Label(gui), "gui\nmore".into()),
        Ok(None)
    ));
}

#[test]
fn a_label_lists_its_labels_then_what_carries_it_by_title() {
    let mut world = World::default();
    let rho = world.label("rho", None);
    let gui = world.label("gui", Some(rho));
    let zebra = world.note("Zebra crossing");
    let apple = world.note("apple pie");
    let gone = world.note("gone");
    let untitled = NoteId(uuid::Uuid::new_v4());
    for note in [zebra, apple, gone, untitled] {
        world.file(&note.address(), rho);
    }
    world.write(gone, "gone", true);
    world.file(&thread(7), rho);
    world.file(&thread(8), rho);
    let name = |address: &Address| (*address == thread(7)).then(|| "Middle thread".to_owned());
    let lines: Vec<String> = label_rows(&world.marks, rho, &name)
        .into_iter()
        .map(|row| row.line)
        .collect();
    let unnamed = format!("  ◦ {}", thread(8).key());
    let eight = thread(8);
    let mut carried = vec![
        ("apple pie", "  * apple pie".to_owned()),
        ("middle thread", "  ◦ Middle thread".to_owned()),
        ("untitled note", "  * untitled note".to_owned()),
        ("zebra crossing", "  * Zebra crossing".to_owned()),
        (eight.key(), unnamed),
    ];
    carried.sort();
    let mut expected = vec!["  # gui".to_owned()];
    expected.extend(carried.into_iter().map(|(_, line)| line));
    assert_eq!(lines, expected);
    let rows = label_rows(&world.marks, rho, &name);
    assert_eq!(rows[0].address, gui.address());
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

#[test]
fn a_new_note_takes_its_place_from_where_it_was_made() {
    let mut world = World::default();
    let rho = world.label("rho", None);
    let desk = world.label("desk", None);
    world.file(&thread(7), rho);
    world.file(&thread(7), desk);
    let note = NoteId(uuid::Uuid::new_v4());
    let facts = |writes: Vec<Write>| -> Vec<Fact> {
        writes
            .into_iter()
            .map(|write| match write {
                Write::Fact(fact) => fact,
                note => panic!("a new note writes no text: {note:?}"),
            })
            .collect()
    };
    assert!(new_note_marks(&world.marks, note, None).is_empty());
    assert_eq!(
        facts(new_note_marks(&world.marks, note, Some(&rho.address()))),
        vec![Fact::Labeled {
            node: note.address(),
            label: rho.0,
            present: true
        }]
    );
    let mut from_thread = facts(new_note_marks(&world.marks, note, Some(&thread(7))));
    assert_eq!(
        from_thread.remove(0),
        Fact::About {
            node: note.address(),
            about: Some(thread(7))
        }
    );
    let mut labels: Vec<uuid::Uuid> = from_thread
        .into_iter()
        .map(|fact| match fact {
            Fact::Labeled { label, node, .. } if node == note.address() => label,
            fact => panic!("not a filing: {fact:?}"),
        })
        .collect();
    labels.sort();
    let mut expected = vec![rho.0, desk.0];
    expected.sort();
    assert_eq!(labels, expected);
}

#[test]
fn find_lists_live_notes_and_labels() {
    let mut world = World::default();
    let rho = world.label("rho", None);
    let gui = world.label("gui", Some(rho));
    let filed = world.note("filed");
    world.file(&filed.address(), gui);
    let gone = world.note("gone");
    world.write(gone, "gone", true);
    let mut found: Vec<(String, String, Address)> = found(&world.marks)
        .into_iter()
        .map(|found| (found.title, found.description, found.address))
        .collect();
    found.sort();
    assert_eq!(
        found,
        vec![
            ("filed".into(), "note · rho/gui".into(), filed.address()),
            ("rho".into(), "label".into(), rho.address()),
            ("rho/gui".into(), "label".into(), gui.address()),
        ]
    );
}

#[test]
fn a_new_note_goes_here_first_then_where_here_is_filed_and_root_last() {
    let mut world = World::default();
    let alpha = world.label("alpha", None);
    let zulu = world.label("zulu", None);
    world.file(&thread(7), zulu);
    let paths = |here: Option<&Address>| -> Vec<(String, Option<Address>)> {
        areas(&world.marks, here)
            .into_iter()
            .map(|area| (area.path, area.target))
            .collect()
    };
    assert_eq!(
        paths(Some(&thread(7))),
        vec![
            ("here".into(), Some(thread(7))),
            ("zulu".into(), Some(zulu.address())),
            ("alpha".into(), Some(alpha.address())),
            ("root".into(), None),
        ]
    );
    assert_eq!(
        paths(None),
        vec![
            ("alpha".into(), Some(alpha.address())),
            ("zulu".into(), Some(zulu.address())),
            ("root".into(), None),
        ]
    );
}

/// Home lists the notes about nothing as private rooms, the one last
/// written first; a reply under something is not a room.
#[test]
fn home_lists_the_private_rooms_newest_written_first() {
    let mut world = World::default();
    let older = world.note("older room");
    let reply = world.note("a reply");
    world.say(Fact::About {
        node: reply.address(),
        about: Some(thread(7)),
    });
    let newer = world.note("newer room");
    world.write(older, "older room, edited last", false);
    let rooms = private_rooms(&world.marks);
    let titles: Vec<&str> = rooms.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(titles, ["older room, edited last", "newer room"]);
    assert_eq!(rooms[1].address, newer.address());
}
