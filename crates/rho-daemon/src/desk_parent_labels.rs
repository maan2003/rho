//! One-shot conversion of the parents things still carry into labels.
//!
//! A thing is placed by the labels it carries and carries no parent; rho
//! stopped writing parents on the filing paths, and this reads the ones
//! already in the store and says the same thing as a label. `Parent` is
//! left alone on a label, where it is what nests one label under another.

use std::collections::{BTreeMap, BTreeSet};

use rho_core::AgentId;
use rho_desk::cells::{BodySnapshot, Facts, Id, Property, Store, Uuid};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) parented: usize,
    pub(crate) labels_minted: usize,
    pub(crate) labels_reused: usize,
    pub(crate) labelled: usize,
    pub(crate) shallower_dropped: usize,
    pub(crate) parents_cleared: usize,
    pub(crate) unnamed: usize,
}

impl Report {
    pub(crate) fn line(&self) -> String {
        format!(
            "desk parents converted: {} things carried a parent; \
             {} labels minted/{} reused, {} labelled, {} shallower labels dropped, \
             {} parents cleared, {} left for want of a name",
            self.parented,
            self.labels_minted,
            self.labels_reused,
            self.labelled,
            self.shallower_dropped,
            self.parents_cleared,
            self.unnamed,
        )
    }
}

/// Pure in-memory conversion, as the outline one was: persistence and the
/// durable marker belong to `desk_cells`.
pub(crate) fn convert(
    store: &mut Store,
    bodies: &[BodySnapshot],
    agent_titles: &BTreeMap<AgentId, Option<String>>,
) -> Result<Report, String> {
    let facts = store
        .all_facts()
        .into_iter()
        .collect::<BTreeMap<Id, Facts>>();
    let mut report = Report::default();
    let mut names = BTreeMap::new();
    for (index, body) in bodies.iter().enumerate() {
        if !facts.contains_key(&body.id) {
            continue;
        }
        let buffer_id = text::BufferId::new(index as u64 + 1).map_err(|error| error.to_string())?;
        let text = body
            .buffer(text::ReplicaId::REMOTE_SERVER.as_u16(), buffer_id)?
            .text();
        if let Some(line) = text
            .lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            names.insert(body.id.clone(), line.to_owned());
        }
    }
    // What a label made from a thing is called: what the thing itself is
    // called, then the first line of what is written in it, then — for an
    // agent, which has neither — what the agent log calls it.
    let name_of = |id: &Id| -> Option<String> {
        facts
            .get(id)
            .and_then(|row| row.name.clone())
            .or_else(|| names.get(id).cloned())
            .or_else(|| match id {
                Id::Agent(agent) => agent_titles.get(agent).cloned().flatten(),
                _ => None,
            })
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
    };
    // A label already in the store is reused rather than minted twice, and
    // two labels of one name in different places are two labels, so what
    // identifies one is its name under its own parent.
    let mut by_path = BTreeMap::new();
    for (id, row) in &facts {
        if matches!(id, Id::Label(_))
            && let Some(name) = row.name.clone()
        {
            by_path.insert((row.parent.clone(), name), id.clone());
        }
    }
    // The label a parent stands for, minted on the spot if it is new and
    // nested under the label its own parent stands for, so the shape the
    // user built is the shape of the labels.
    let mut resolved: BTreeMap<Id, Option<Id>> = BTreeMap::new();
    let mut writes = Vec::new();
    for (id, row) in &facts {
        if matches!(id, Id::Label(_)) || row.parent.is_none() {
            continue;
        }
        report.parented += 1;
        let parent = row.parent.clone().expect("a parent was just read");
        let Some(label) = label_for(
            &parent,
            &facts,
            &name_of,
            &mut by_path,
            &mut resolved,
            &mut writes,
            &mut report,
            &mut BTreeSet::new(),
        ) else {
            report.unnamed += 1;
            continue;
        };
        // The smallest set that says where the thing is: what it carries
        // already may be the same label, or a deeper one, or one this new
        // label is nested under and which now says nothing.
        let above = ancestry(&label, &by_path_parents(&facts, &writes));
        if !row.labels.contains(&label) {
            let deeper = row
                .labels
                .iter()
                .any(|held| ancestry(held, &by_path_parents(&facts, &writes)).contains(&label));
            if !deeper {
                writes.push((
                    id.clone(),
                    Property::Labeled {
                        label: label.clone(),
                        present: true,
                    },
                ));
                report.labelled += 1;
            }
        }
        for held in &row.labels {
            if *held != label && above.contains(held) {
                writes.push((
                    id.clone(),
                    Property::Labeled {
                        label: held.clone(),
                        present: false,
                    },
                ));
                report.shallower_dropped += 1;
            }
        }
        writes.push((id.clone(), Property::Parent(None)));
        report.parents_cleared += 1;
    }
    for (id, property) in writes {
        store.write(id, property)?;
    }
    Ok(report)
}

/// The label a thing stands for. Nearest parent first, so the whole chain
/// above it is minted too and the labels nest as the things did.
#[allow(clippy::too_many_arguments)]
fn label_for(
    id: &Id,
    facts: &BTreeMap<Id, Facts>,
    name_of: &impl Fn(&Id) -> Option<String>,
    by_path: &mut BTreeMap<(Option<Id>, String), Id>,
    resolved: &mut BTreeMap<Id, Option<Id>>,
    writes: &mut Vec<(Id, Property)>,
    report: &mut Report,
    walking: &mut BTreeSet<Id>,
) -> Option<Id> {
    // A thing that is already a label is the label: nothing is minted for
    // something the user has already named as one.
    if matches!(id, Id::Label(_)) {
        return Some(id.clone());
    }
    if let Some(label) = resolved.get(id) {
        return label.clone();
    }
    // A cycle in the parents is the user's data too: it stops the walk
    // rather than the daemon.
    if !walking.insert(id.clone()) {
        return None;
    }
    let name = name_of(id);
    let above = facts
        .get(id)
        .and_then(|row| row.parent.clone())
        .and_then(|parent| {
            label_for(
                &parent, facts, name_of, by_path, resolved, writes, report, walking,
            )
        });
    walking.remove(id);
    let label = name.map(|name| match by_path.get(&(above.clone(), name.clone())) {
        Some(label) => {
            report.labels_reused += 1;
            label.clone()
        }
        None => {
            let label = Id::Label(label_uuid(above.as_ref(), &name));
            writes.push((label.clone(), Property::Name(name.clone())));
            writes.push((label.clone(), Property::Parent(above.clone())));
            by_path.insert((above.clone(), name), label.clone());
            report.labels_minted += 1;
            label
        }
    });
    resolved.insert(id.clone(), label.clone());
    label
}

/// Every label a label is nested under, itself included.
fn ancestry(label: &Id, parents: &BTreeMap<Id, Option<Id>>) -> BTreeSet<Id> {
    let mut result = BTreeSet::new();
    let mut next = Some(label.clone());
    while let Some(id) = next.filter(|id| result.insert(id.clone())) {
        next = parents.get(&id).cloned().flatten();
    }
    result
}

/// What each label is nested under, reading the store and the nesting this
/// conversion has already decided on but not yet written.
fn by_path_parents(
    facts: &BTreeMap<Id, Facts>,
    writes: &[(Id, Property)],
) -> BTreeMap<Id, Option<Id>> {
    let mut result = facts
        .iter()
        .filter(|(id, _)| matches!(id, Id::Label(_)))
        .map(|(id, row)| (id.clone(), row.parent.clone()))
        .collect::<BTreeMap<_, _>>();
    for (id, property) in writes {
        if matches!(id, Id::Label(_))
            && let Property::Parent(parent) = property
        {
            result.insert(id.clone(), parent.clone());
        }
    }
    result
}

/// The same name under the same label is the same label on every run, so
/// two conversions of one store agree cell for cell.
fn label_uuid(parent: Option<&Id>, name: &str) -> Uuid {
    let mut bytes = [0; 16];
    let parent = parent.map(|id| format!("{id:?}")).unwrap_or_default();
    for half in 0..2 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ (half as u64);
        for byte in b"parent-label\0"
            .iter()
            .chain(parent.as_bytes())
            .chain(b"\0".iter())
            .chain(name.as_bytes())
        {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        bytes[half * 8..half * 8 + 8].copy_from_slice(&hash.to_le_bytes());
    }
    Uuid(bytes)
}

#[cfg(test)]
mod tests {
    use rho_desk::cells::DeviceId;

    use super::*;

    fn store() -> Store {
        Store::new(DeviceId([1; 16]))
    }

    /// The shape the user's things are in: a note under a note under a
    /// note. Each parent becomes a label of its own name, the labels nest
    /// as the notes did, and every thing ends up carrying the one label
    /// nearest to it and no parent at all.
    #[test]
    fn a_chain_of_parents_becomes_a_chain_of_labels() {
        let mut store = store();
        let rho = Id::Note(Uuid([1; 16]));
        let agent = Id::Note(Uuid([2; 16]));
        let thing = Id::Note(Uuid([3; 16]));
        store
            .write(rho.clone(), Property::Name("rho".into()))
            .unwrap();
        store
            .write(agent.clone(), Property::Name("agent".into()))
            .unwrap();
        store
            .write(agent.clone(), Property::Parent(Some(rho.clone())))
            .unwrap();
        store
            .write(thing.clone(), Property::Parent(Some(agent.clone())))
            .unwrap();

        let report = convert(&mut store, &[], &BTreeMap::new()).unwrap();
        assert_eq!(report.parented, 2);
        assert_eq!(report.labels_minted, 2);
        assert_eq!(report.parents_cleared, 2);
        assert_eq!(report.unnamed, 0);

        let facts = store.all_facts().into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(facts[&thing].parent, None);
        assert_eq!(facts[&agent].parent, None);
        let carried = |id: &Id| facts[id].labels.iter().cloned().collect::<Vec<_>>();
        let inner = carried(&thing);
        assert_eq!(inner.len(), 1, "the thing carries the one label nearest it");
        assert_eq!(facts[&inner[0]].name.as_deref(), Some("agent"));
        let outer = carried(&agent);
        assert_eq!(outer.len(), 1);
        assert_eq!(facts[&outer[0]].name.as_deref(), Some("rho"));
        // The labels nest as the notes did, so `rho/agent` is a path.
        assert_eq!(facts[&inner[0]].parent, Some(outer[0].clone()));
    }

    /// The smallest set that says where the thing is: a label the new one
    /// is nested under says nothing once the deeper one is on, and a label
    /// the user already made for the same name is reused rather than
    /// minted beside itself.
    #[test]
    fn the_label_already_carried_is_reused_and_the_shallower_one_dropped() {
        let mut store = store();
        let rho_label = Id::Label(Uuid([9; 16]));
        let rho = Id::Note(Uuid([1; 16]));
        let thing = Id::Note(Uuid([3; 16]));
        store
            .write(rho_label.clone(), Property::Name("rho".into()))
            .unwrap();
        store
            .write(rho_label.clone(), Property::Parent(None))
            .unwrap();
        store
            .write(rho.clone(), Property::Name("rho".into()))
            .unwrap();
        store
            .write(thing.clone(), Property::Parent(Some(rho.clone())))
            .unwrap();
        store
            .write(
                thing.clone(),
                Property::Labeled {
                    label: rho_label.clone(),
                    present: true,
                },
            )
            .unwrap();

        let report = convert(&mut store, &[], &BTreeMap::new()).unwrap();
        assert_eq!(report.labels_minted, 0, "the user's own label is the label");
        assert_eq!(report.labels_reused, 1);
        assert_eq!(report.labelled, 0, "it already carries it");
        assert_eq!(report.parents_cleared, 1);
        let facts = store.all_facts().into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(facts[&thing].parent, None);
        assert_eq!(
            facts[&thing].labels.iter().cloned().collect::<Vec<_>>(),
            vec![rho_label]
        );
    }

    /// A parent nobody named cannot become a label, so the parent stays
    /// where it is rather than the thing losing the only thing that says
    /// where it is.
    #[test]
    fn a_parent_with_no_name_keeps_its_parent() {
        let mut store = store();
        let nameless = Id::Note(Uuid([1; 16]));
        let thing = Id::Note(Uuid([3; 16]));
        store
            .write(nameless.clone(), Property::Parent(None))
            .unwrap();
        store
            .write(thing.clone(), Property::Parent(Some(nameless.clone())))
            .unwrap();

        let report = convert(&mut store, &[], &BTreeMap::new()).unwrap();
        assert_eq!(report.parented, 1);
        assert_eq!(report.unnamed, 1);
        assert_eq!(report.parents_cleared, 0);
        let facts = store.all_facts().into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(facts[&thing].parent, Some(nameless));
    }

    /// A label's parent is what nests one label under another, so the
    /// conversion leaves it alone.
    #[test]
    fn a_labels_own_parent_is_left_alone() {
        let mut store = store();
        let outer = Id::Label(Uuid([8; 16]));
        let inner = Id::Label(Uuid([9; 16]));
        store
            .write(outer.clone(), Property::Name("rho".into()))
            .unwrap();
        store
            .write(inner.clone(), Property::Name("agent".into()))
            .unwrap();
        store
            .write(inner.clone(), Property::Parent(Some(outer.clone())))
            .unwrap();

        let report = convert(&mut store, &[], &BTreeMap::new()).unwrap();
        assert_eq!(report, Report::default());
        let facts = store.all_facts().into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(facts[&inner].parent, Some(outer));
    }
}
