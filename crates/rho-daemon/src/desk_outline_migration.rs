//! One-shot recovery of the pre-cutover outline as labels.

use std::collections::{BTreeMap, BTreeSet};

use rho_core::AgentId;
use rho_desk::cells::{BodySnapshot, Facts, Id, Project, Property, Snapshot, State, Store, Uuid};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) before: BTreeMap<&'static str, usize>,
    pub(crate) after: BTreeMap<&'static str, usize>,
    pub(crate) facts: usize,
    pub(crate) stamps: usize,
    pub(crate) archives: usize,
    pub(crate) archive_descendants: usize,
    pub(crate) outside_headings: usize,
    pub(crate) inside_headings: usize,
    pub(crate) labels_minted: usize,
    pub(crate) labels_reused: usize,
    pub(crate) files: usize,
    pub(crate) projects_copied: usize,
    pub(crate) agent_items: usize,
    pub(crate) agents_inherited: usize,
    pub(crate) names_written: usize,
    pub(crate) about: usize,
    pub(crate) bookmarks: usize,
    pub(crate) parents_cleared: usize,
    pub(crate) snoozes_cleared: usize,
    pub(crate) zero_created_at: usize,
    pub(crate) empty_notes: usize,
    pub(crate) orphan_bodies: usize,
}

impl Report {
    pub(crate) fn line(&self) -> String {
        format!(
            "desk outline converted: {} facts; {} stamps, {} archives/{} descendants, \
             {}+{} headings, {} labels minted/{} reused, {} files/{} projects, \
             {} agent items/{} agents/{} names, {} about, {} bookmarks, \
             {} parents, {} snoozes, {} zero times, {} empty notes, {} orphan bodies",
            self.facts,
            self.stamps,
            self.archives,
            self.archive_descendants,
            self.outside_headings,
            self.inside_headings,
            self.labels_minted,
            self.labels_reused,
            self.files,
            self.projects_copied,
            self.agent_items,
            self.agents_inherited,
            self.names_written,
            self.about,
            self.bookmarks,
            self.parents_cleared,
            self.snoozes_cleared,
            self.zero_created_at,
            self.empty_notes,
            self.orphan_bodies,
        )
    }
}

/// Pure in-memory conversion. Persistence and its durable marker belong to
/// `desk_cells`; this function knows only facts, bodies, and registry titles.
pub(crate) fn convert(
    store: &mut Store,
    bodies: Vec<BodySnapshot>,
    agent_titles: &BTreeMap<AgentId, Option<String>>,
) -> Result<(Snapshot, Vec<BodySnapshot>, Report), String> {
    let original = store.all_facts();
    let mut report = Report {
        before: kind_counts(&original),
        facts: store.snapshot().cells.len(),
        ..Report::default()
    };
    let facts = original.iter().cloned().collect::<BTreeMap<_, _>>();
    let mut texts = BTreeMap::new();
    let mut body_rows = BTreeMap::new();
    for (index, body) in bodies.into_iter().enumerate() {
        let id = body.id.clone();
        let buffer_id = text::BufferId::new(index as u64 + 1).map_err(|error| error.to_string())?;
        let text = body
            .buffer(text::ReplicaId::REMOTE_SERVER.as_u16(), buffer_id)?
            .text();
        texts.insert(id.clone(), text);
        body_rows.insert(id, body);
    }
    report.orphan_bodies = body_rows
        .keys()
        .filter(|id| !facts.contains_key(*id))
        .count();
    body_rows.retain(|id, _| facts.contains_key(id));

    let mut children: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
    for (id, row) in &facts {
        if let Some(parent) = &row.parent {
            children.entry(parent.clone()).or_default().push(id.clone());
        }
    }
    let title = |id: &Id| -> String {
        facts
            .get(id)
            .and_then(|row| row.name.clone())
            .or_else(|| {
                texts
                    .get(id)
                    .and_then(|text| text.lines().next().map(str::to_owned))
            })
            .unwrap_or_default()
    };
    let stamps = facts
        .keys()
        .filter(|id| matches!(id, Id::Note(_)) && is_stamp(&title(id)))
        .cloned()
        .collect::<BTreeSet<_>>();
    report.stamps = stamps.len();
    for stamp in &stamps {
        if let Some(parent) = facts.get(stamp).and_then(|row| row.parent.clone()) {
            store.write(parent, Property::State(State::Done))?;
        }
    }

    let archives = facts
        .keys()
        .filter(|id| matches!(id, Id::Note(_)) && title(id).eq_ignore_ascii_case("archive"))
        .cloned()
        .collect::<BTreeSet<_>>();
    report.archives = archives.len();
    let archive_descendants = archives
        .iter()
        .flat_map(|id| descendants(id, &children))
        .collect::<BTreeSet<_>>();
    report.archive_descendants = archive_descendants.len();
    for id in &archive_descendants {
        store.write(id.clone(), Property::State(State::Done))?;
    }

    let non_stamp_children = |id: &Id| {
        children
            .get(id)
            .into_iter()
            .flatten()
            .filter(|child| !stamps.contains(*child))
            .cloned()
            .collect::<Vec<_>>()
    };
    let agent_items = facts
        .keys()
        .filter(|id| matches!(id, Id::Note(_)) && !archives.contains(*id) && !stamps.contains(*id))
        .filter(|id| {
            let children = non_stamp_children(id);
            !children.is_empty() && children.iter().all(|child| matches!(child, Id::Agent(_))) && {
                let body = texts.get(*id).map(String::as_str).unwrap_or("").trim();
                body.is_empty() || body == title(id).trim()
            }
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let headings = facts
        .keys()
        .filter(|id| matches!(id, Id::Note(_)))
        .filter(|id| {
            !archives.contains(*id)
                && !agent_items.contains(*id)
                && !stamps.contains(*id)
                && !children
                    .get(*id)
                    .is_some_and(|kids| kids.iter().any(|kid| matches!(kid, Id::Page(_))))
        })
        .filter(|id| non_stamp_children(id).len() >= 2)
        .cloned()
        .collect::<BTreeSet<_>>();
    let outside_headings = headings
        .difference(&archive_descendants)
        .cloned()
        .collect::<BTreeSet<_>>();
    let inside_headings = headings
        .intersection(&archive_descendants)
        .cloned()
        .collect::<BTreeSet<_>>();
    report.outside_headings = outside_headings.len();
    report.inside_headings = inside_headings.len();

    let archive_labels = facts
        .iter()
        .filter(|(id, row)| {
            matches!(id, Id::Label(_))
                && row
                    .name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case("archive"))
        })
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    let mut labels = facts
        .iter()
        .filter_map(|(id, row)| {
            matches!(id, Id::Label(_))
                .then(|| row.name.clone().map(|name| (name, id.clone())))
                .flatten()
        })
        .collect::<BTreeMap<_, _>>();
    let preexisting_labels = labels.values().cloned().collect::<BTreeSet<_>>();
    let mut heading_label = BTreeMap::new();
    for heading in &outside_headings {
        let name = heading_name(&title(heading));
        let label = if let Some(label) = labels.get(&name) {
            label.clone()
        } else {
            let label = Id::Label(label_uuid(&name));
            store.write(label.clone(), Property::Name(name.clone()))?;
            labels.insert(name.clone(), label.clone());
            report.labels_minted += 1;
            label
        };
        heading_label.insert(heading.clone(), label);
    }
    report.labels_reused = heading_label
        .values()
        .filter(|label| preexisting_labels.contains(*label))
        .collect::<BTreeSet<_>>()
        .len();
    // The outermost equal-name heading owns the one label's Parent.
    let mut owners: BTreeMap<Id, (usize, Id)> = BTreeMap::new();
    for (heading, label) in &heading_label {
        let depth = ancestor_headings(heading, &facts, &outside_headings).len();
        if owners.get(label).is_none_or(|(old, _)| depth < *old) {
            owners.insert(label.clone(), (depth, heading.clone()));
        }
    }
    let mut label_parents = BTreeMap::new();
    for (label, (_, owner)) in owners {
        let parent = ancestor_headings(&owner, &facts, &outside_headings)
            .first()
            .and_then(|heading| heading_label.get(heading))
            .filter(|parent| **parent != label)
            .cloned();
        label_parents.insert(label.clone(), parent.clone());
        store.write(label, Property::Parent(parent))?;
    }

    let files = facts
        .keys()
        .filter(|id| matches!(id, Id::File { .. }))
        .cloned()
        .collect::<BTreeSet<_>>();
    report.files = files.len();
    for file in &files {
        let Id::File { host, path } = file else {
            unreachable!()
        };
        let Some(name) = path.file_name() else {
            continue;
        };
        let label = match labels.get(name) {
            Some(label) => label.clone(),
            None => {
                let label = Id::Label(label_uuid(name));
                store.write(label.clone(), Property::Name(name.to_owned()))?;
                store.write(label.clone(), Property::Parent(None))?;
                labels.insert(name.to_owned(), label.clone());
                report.labels_minted += 1;
                label
            }
        };
        if store.facts(&label).project.is_none() {
            store.write(
                label,
                Property::Project(Some(Project {
                    host: *host,
                    path: path.clone(),
                })),
            )?;
            report.projects_copied += 1;
        }
    }

    for (id, row) in &facts {
        for archive in &archive_labels {
            if row.labels.contains(archive) {
                store.write(
                    id.clone(),
                    Property::Labeled {
                        label: archive.clone(),
                        present: false,
                    },
                )?;
            }
        }
    }

    let context_labels = |id: &Id| {
        let ancestors = ancestor_headings(id, &facts, &outside_headings);
        let mut result = Vec::new();
        for heading in ancestors {
            if let Some(label) = heading_label.get(&heading) {
                if result.is_empty() {
                    result.push(label.clone());
                } else if label_parents
                    .get(result.last().unwrap())
                    .and_then(Option::as_ref)
                    != Some(label)
                    && !result.contains(label)
                {
                    // Equal-name reuse can put the nearer label somewhere
                    // other than this outline edge. Preserve that edge by
                    // carrying the enclosing label as well.
                    result.push(label.clone());
                } else {
                    break;
                }
            }
        }
        result
    };
    for id in facts.keys() {
        if matches!(id, Id::Label(_)) {
            continue;
        }
        for label in context_labels(id) {
            store.write(
                id.clone(),
                Property::Labeled {
                    label,
                    present: true,
                },
            )?;
        }
    }

    report.agent_items = agent_items.len();
    for item in &agent_items {
        let item_facts = store.facts(item);
        let item_done = item_facts.state == State::Done || archive_descendants.contains(item);
        let inherited = item_facts.labels;
        for child in non_stamp_children(item) {
            let Id::Agent(agent) = child else { continue };
            report.agents_inherited += 1;
            for label in &inherited {
                store.write(
                    Id::Agent(agent),
                    Property::Labeled {
                        label: label.clone(),
                        present: true,
                    },
                )?;
            }
            if item_done {
                store.write(Id::Agent(agent), Property::State(State::Done))?;
            }
            let wanted = title(item);
            if agent_titles.get(&agent).and_then(|name| name.as_deref()) != Some(wanted.as_str()) {
                store.write(Id::Agent(agent), Property::Name(wanted))?;
                report.names_written += 1;
            }
        }
    }

    let about_candidates = facts
        .iter()
        .filter_map(|(id, row)| match (id, row.parent.as_ref()) {
            (Id::Note(_), Some(Id::Agent(_))) => Some((id.clone(), row.parent.clone().unwrap())),
            _ => None,
        })
        .collect::<Vec<_>>();

    let bookmarks = facts
        .keys()
        .filter(|id| {
            matches!(id, Id::Note(_))
                && children
                    .get(*id)
                    .is_some_and(|kids| kids.iter().any(|kid| matches!(kid, Id::Page(_))))
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    report.bookmarks = bookmarks.len();
    let pages = bookmarks
        .iter()
        .flat_map(|id| children.get(id).into_iter().flatten())
        .filter(|id| matches!(id, Id::Page(_)))
        .cloned()
        .collect::<BTreeSet<_>>();

    let real_heading_notes = outside_headings
        .iter()
        .filter(|id| has_real_body(texts.get(*id).map(String::as_str).unwrap_or(""), &title(id)))
        .cloned()
        .collect::<BTreeSet<_>>();
    for heading in &real_heading_notes {
        if let Some(label) = heading_label.get(heading) {
            store.write(
                heading.clone(),
                Property::Labeled {
                    label: label.clone(),
                    present: true,
                },
            )?;
        }
    }
    let mut dropped = stamps
        .union(&archives)
        .cloned()
        .chain(headings.difference(&real_heading_notes).cloned())
        .chain(agent_items.iter().cloned())
        .chain(bookmarks.iter().cloned())
        .chain(pages.iter().cloned())
        .chain(files.iter().cloned())
        .chain(archive_labels.iter().cloned())
        .collect::<BTreeSet<_>>();
    for (note, about) in about_candidates {
        let empty = title(&note).trim().is_empty()
            && texts.get(&note).is_none_or(|body| body.trim().is_empty());
        if empty {
            dropped.insert(note);
            report.empty_notes += 1;
        } else {
            store.write(note, Property::About(about))?;
            report.about += 1;
        }
    }
    for id in facts.keys().filter(|id| matches!(id, Id::Note(_))) {
        if !dropped.contains(id)
            && title(id).trim().is_empty()
            && texts.get(id).is_none_or(|body| body.trim().is_empty())
        {
            dropped.insert(id.clone());
            report.empty_notes += 1;
        }
    }
    for id in &dropped {
        store.write(id.clone(), Property::Deleted(true))?;
        body_rows.remove(id);
    }

    for (id, row) in &facts {
        if !matches!(id, Id::Label(_)) && row.parent.is_some() && !dropped.contains(id) {
            store.write(id.clone(), Property::Parent(None))?;
            report.parents_cleared += 1;
        }
        if matches!(id, Id::Agent(_)) && row.defer_until.is_some() {
            store.write(id.clone(), Property::DeferUntil(None))?;
            report.snoozes_cleared += 1;
        }
        if row.created_at.is_some_and(|at| at.unix_ms == 0) {
            report.zero_created_at += 1;
        }
    }
    let mut snapshot = store.snapshot();
    snapshot
        .cells
        .retain(|cell| !matches!(cell.property, Property::CreatedAt(at) if at.unix_ms == 0));
    let result =
        Store::from_snapshot(rho_desk::cells::DeviceId([0; 16]), snapshot.clone())?.all_facts();
    report.after = kind_counts(&result);
    Ok((snapshot, body_rows.into_values().collect(), report))
}

fn is_stamp(title: &str) -> bool {
    title
        .get(..11)
        .filter(|prefix| prefix.eq_ignore_ascii_case(":archived: "))
        .and_then(|_| title[11..].split_whitespace().next())
        .is_some_and(|date| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok())
}

fn heading_name(title: &str) -> String {
    title
        .trim()
        .strip_suffix(':')
        .unwrap_or(title.trim())
        .trim_end()
        .to_owned()
}

fn has_real_body(body: &str, title: &str) -> bool {
    let mut lines = body.lines();
    if lines.next().is_some_and(|line| line == title) {
        lines.any(|line| !line.trim().is_empty())
    } else {
        !body.trim().is_empty() && body.trim() != title.trim()
    }
}

fn descendants(root: &Id, children: &BTreeMap<Id, Vec<Id>>) -> BTreeSet<Id> {
    let mut result = BTreeSet::new();
    let mut pending = children.get(root).cloned().unwrap_or_default();
    while let Some(id) = pending.pop() {
        if result.insert(id.clone()) {
            pending.extend(children.get(&id).into_iter().flatten().cloned());
        }
    }
    result
}

/// Nearest first.
fn ancestor_headings(id: &Id, facts: &BTreeMap<Id, Facts>, headings: &BTreeSet<Id>) -> Vec<Id> {
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    let mut parent = facts.get(id).and_then(|row| row.parent.clone());
    while let Some(id) = parent.filter(|id| seen.insert(id.clone())) {
        if headings.contains(&id) {
            result.push(id.clone());
        }
        parent = facts.get(&id).and_then(|row| row.parent.clone());
    }
    result
}

fn label_uuid(name: &str) -> Uuid {
    let mut bytes = [0; 16];
    for half in 0..2 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ (half as u64);
        for byte in b"outline-label\0".iter().chain(name.as_bytes()) {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        bytes[half * 8..half * 8 + 8].copy_from_slice(&hash.to_le_bytes());
    }
    Uuid(bytes)
}

fn kind_counts(rows: &[(Id, Facts)]) -> BTreeMap<&'static str, usize> {
    let mut result = BTreeMap::new();
    for (id, _) in rows {
        let kind = match id {
            Id::Note(_) => "notes",
            Id::Label(_) => "labels",
            Id::Agent(_) => "agents",
            Id::Host(_) => "hosts",
            Id::Page(_) => "pages",
            Id::Slack(_) => "slack",
            Id::PullRequest { .. } => "pull requests",
            Id::File { .. } => "files",
        };
        *result.entry(kind).or_default() += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use rho_core::AgentIdDomain;
    use rho_desk::cells::{DeviceId, StoryPos, Timestamp, TimestampPrecision};

    use super::*;

    fn note(byte: u8) -> Id {
        Id::Note(Uuid([byte; 16]))
    }
    fn label(byte: u8) -> Id {
        Id::Label(Uuid([byte; 16]))
    }
    fn agent(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &AgentIdDomain(7)).unwrap()
    }
    fn body(id: Id, text_value: &str, replica: u16) -> BodySnapshot {
        let mut buffer = text::Buffer::new(
            text::ReplicaId::new(replica),
            text::BufferId::new(replica as u64).unwrap(),
            "",
        );
        let operation = rho_desk::TextOperation::from_text(&buffer.edit([(0..0, text_value)]));
        BodySnapshot {
            id,
            operations: vec![operation],
            transactions: Vec::new(),
        }
    }
    fn put(store: &mut Store, id: &Id, property: Property) {
        store.write(id.clone(), property).unwrap();
    }
    fn parent(store: &mut Store, child: &Id, parent: &Id) {
        put(store, child, Property::Parent(Some(parent.clone())));
        put(
            store,
            child,
            Property::CreatedAt(Timestamp {
                unix_ms: 1,
                precision: TimestampPrecision::Millisecond,
            }),
        );
    }

    #[test]
    fn every_outline_rule_is_one_pure_conversion_over_a_generated_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let mut seeded = Store::new(DeviceId([9; 16]));
        let rho = label(1);
        let archive_label = label(2);
        let foo = label(3);
        for (id, name) in [(&rho, "rho"), (&archive_label, "archive"), (&foo, "foo")] {
            put(&mut seeded, id, Property::Name(name.into()));
            put(&mut seeded, id, Property::Parent(None));
        }

        let root_browser = note(10);
        put(&mut seeded, &root_browser, Property::Parent(None));
        let root_leaf = note(11);
        let zero_leaf = note(12);
        parent(&mut seeded, &root_leaf, &root_browser);
        parent(&mut seeded, &zero_leaf, &root_browser);
        put(
            &mut seeded,
            &zero_leaf,
            Property::CreatedAt(Timestamp {
                unix_ms: 0,
                precision: TimestampPrecision::Millisecond,
            }),
        );

        let rho_heading = note(20);
        put(&mut seeded, &rho_heading, Property::Parent(None));
        let nested_browser = note(21);
        let nested_leaf = note(22);
        let item = note(23);
        parent(&mut seeded, &nested_browser, &rho_heading);
        parent(&mut seeded, &item, &rho_heading);
        parent(&mut seeded, &nested_leaf, &nested_browser);
        let nested_other = note(24);
        parent(&mut seeded, &nested_other, &nested_browser);
        let a1 = agent(1);
        let a2 = agent(2);
        let a1_id = Id::Agent(a1);
        let a2_id = Id::Agent(a2);
        parent(&mut seeded, &a1_id, &item);
        parent(&mut seeded, &a2_id, &item);
        put(
            &mut seeded,
            &a1_id,
            Property::DeferUntil(Some(Timestamp {
                unix_ms: 9,
                precision: TimestampPrecision::Day,
            })),
        );
        put(
            &mut seeded,
            &a1_id,
            Property::AgentHandledThrough(StoryPos(4)),
        );
        let stamp = note(25);
        parent(&mut seeded, &stamp, &item);
        let stamp_child = Id::Agent(agent(3));
        parent(&mut seeded, &stamp_child, &stamp);

        let detail = note(26);
        let empty_about = note(27);
        parent(&mut seeded, &detail, &a1_id);
        parent(&mut seeded, &empty_about, &a1_id);

        let archive = note(40);
        put(&mut seeded, &archive, Property::Parent(None));
        let inside = note(41);
        let archived_leaf = note(42);
        let file = Id::File {
            host: 7,
            path: Utf8PathBuf::from("foo"),
        };
        parent(&mut seeded, &inside, &archive);
        parent(&mut seeded, &archived_leaf, &inside);
        parent(&mut seeded, &file, &inside);
        put(
            &mut seeded,
            &archived_leaf,
            Property::Labeled {
                label: archive_label.clone(),
                present: true,
            },
        );

        let bookmark = note(50);
        put(&mut seeded, &bookmark, Property::Parent(None));
        let page = Id::Page(rho_desk::PageId([5; 16]));
        let page_two = Id::Page(rho_desk::PageId([6; 16]));
        parent(&mut seeded, &page, &bookmark);
        parent(&mut seeded, &page_two, &bookmark);
        let empty_root = note(60);
        put(&mut seeded, &empty_root, Property::Parent(None));
        let orphan = note(99);

        let bodies = vec![
            body(root_browser.clone(), "browser\nreal body", 1),
            body(root_leaf.clone(), "one", 2),
            body(zero_leaf.clone(), "two", 3),
            body(rho_heading.clone(), "rho", 4),
            body(nested_browser.clone(), "browser", 5),
            body(nested_leaf.clone(), "nested one", 6),
            body(nested_other.clone(), "nested two", 7),
            body(item.clone(), "work", 8),
            body(stamp.clone(), ":archived: 2026-01-01", 9),
            body(detail.clone(), "detail", 10),
            body(archive.clone(), "Archive", 11),
            body(inside.clone(), "inside:", 12),
            body(archived_leaf.clone(), "old", 13),
            body(bookmark.clone(), "bookmark", 14),
            body(orphan.clone(), "orphan", 15),
        ];
        let packed = senax_encoder::pack(&(seeded.snapshot(), bodies)).unwrap();
        let path = directory.path().join("generated-desk.senax");
        std::fs::write(&path, packed).unwrap();
        let mut bytes = bytes::Bytes::from(std::fs::read(path).unwrap());
        let (snapshot, bodies): (Snapshot, Vec<BodySnapshot>) =
            senax_encoder::unpack(&mut bytes).unwrap();
        let mut input = Store::from_snapshot(DeviceId([8; 16]), snapshot).unwrap();
        let titles = BTreeMap::from([(a1, Some("work".into())), (a2, Some("old".into()))]);
        let (snapshot, bodies, report) = convert(&mut input, bodies, &titles).unwrap();
        let output = Store::from_snapshot(DeviceId([7; 16]), snapshot).unwrap();

        let browser = output
            .all_facts()
            .into_iter()
            .find(|(id, row)| matches!(id, Id::Label(_)) && row.name.as_deref() == Some("browser"))
            .map(|(id, _)| id)
            .unwrap();
        assert_eq!(output.facts(&browser).parent, None); // outer duplicate wins
        assert!(output.facts(&nested_leaf).labels.contains(&browser));
        assert!(output.facts(&nested_leaf).labels.contains(&rho));
        assert!(!output.facts(&root_browser).deleted); // real heading body survives
        assert!(output.facts(&root_browser).labels.contains(&browser));

        assert!(output.facts(&item).deleted); // all-agent precedence over heading
        assert_eq!(output.facts(&a1_id).state, State::Done);
        assert_eq!(output.facts(&a2_id).state, State::Done);
        assert_eq!(output.facts(&a1_id).name, None); // registry name already agrees
        assert_eq!(output.facts(&a2_id).name.as_deref(), Some("work"));
        assert_eq!(output.facts(&a1_id).defer_until, None);
        assert_eq!(
            output.facts(&a1_id).agent_handled_through,
            Some(StoryPos(4))
        );
        assert_eq!(output.facts(&detail).about, Some(a1_id.clone()));
        assert!(output.facts(&empty_about).deleted);
        assert!(output.facts(&empty_root).deleted);

        assert!(output.facts(&archive).deleted);
        assert!(output.facts(&inside).deleted);
        assert_eq!(output.facts(&archived_leaf).state, State::Done);
        assert!(!output.facts(&archived_leaf).labels.contains(&archive_label));
        assert!(output.facts(&archive_label).deleted);
        assert!(output.facts(&file).deleted);
        assert_eq!(
            output.facts(&foo).project,
            Some(Project {
                host: 7,
                path: Utf8PathBuf::from("foo"),
            })
        );
        assert!(output.facts(&bookmark).deleted);
        assert!(output.facts(&page).deleted);
        assert!(output.facts(&page_two).deleted);
        assert!(
            output
                .all_facts()
                .iter()
                .all(|(_, row)| row.name.as_deref() != Some("bookmark"))
        );
        assert_eq!(output.facts(&zero_leaf).created_at, None);
        assert!(output.facts(&nested_leaf).parent.is_none());
        let kept_bodies = bodies
            .into_iter()
            .map(|body| body.id)
            .collect::<BTreeSet<_>>();
        assert!(kept_bodies.contains(&detail));
        assert!(!kept_bodies.contains(&stamp));
        assert!(!kept_bodies.contains(&bookmark));
        assert!(!kept_bodies.contains(&orphan));
        assert_eq!(report.stamps, 1);
        assert_eq!(report.archives, 1);
        assert_eq!(report.agent_items, 1);
        assert_eq!(output.facts(&stamp_child).name, None);
        assert_eq!(report.agents_inherited, 2);
        assert_eq!(report.names_written, 1);
        assert_eq!(report.about, 1);
        assert_eq!(report.empty_notes, 2);
        assert_eq!(report.orphan_bodies, 1);
        assert_eq!(report.files, 1);
        assert_eq!(report.projects_copied, 1);
        assert_eq!(report.bookmarks, 1);
        assert_eq!(report.snoozes_cleared, 1);
        assert_eq!(report.zero_created_at, 1);
    }

    #[test]
    fn a_unicode_title_is_not_sliced_while_testing_for_an_archive_stamp() {
        assert!(!is_stamp("éééééé title"));
        assert!(!is_stamp(":archived: "));
        assert!(is_stamp(":ARCHIVED: 2026-01-01"));
    }
}
