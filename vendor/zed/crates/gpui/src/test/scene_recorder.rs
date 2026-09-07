use crate::{Bounds, ElementId, EntityId, ScaledPixels, Scene};
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

/// One primitive in a scene produced by GPUI's test platform.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedPrimitive {
    /// Stable hash of the primitive's typed paint fields.
    pub fingerprint: u64,
    /// The primitive's clipped bounds in device-independent scaled pixels.
    pub bounds: Bounds<ScaledPixels>,
    /// The smallest owner boundary GPUI could identify for this primitive.
    pub owner: SceneOwner,
}

/// Typed identity of a scene partition.
///
/// The vertical band refines an element owner for elements such as an editor,
/// which paint many independently changing rows without child element IDs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SceneOwner {
    /// Innermost entity view which painted the primitive.
    pub view: Option<EntityId>,
    /// Nested element identity at the paint site.
    pub elements: Arc<[ElementId]>,
    /// Top device-independent pixel row touched by the primitive.
    pub vertical_band: i32,
}

/// Stable recorder-local identity for a [`SceneOwner`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubsceneId(pub usize);

/// One owner's content in one frame.
#[derive(Clone, Debug)]
pub struct FrameSubscene {
    /// Stable identity of the owner.
    pub id: SubsceneId,
    /// Hash of the owner's primitives in paint order.
    pub hash: u64,
    /// Number of primitives painted by the owner.
    pub primitive_count: usize,
    /// Union of the owner's clipped primitive bounds.
    pub bounds: Bounds<ScaledPixels>,
}

/// A primitive which changed between consecutive recorded frames.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimitiveChange {
    /// Owner whose paint changed.
    pub subscene: SubsceneId,
    /// Position in paint order.
    pub index: usize,
    /// Primitive in the preceding frame, when this was not an insertion.
    pub before: Option<RecordedPrimitive>,
    /// Primitive in this frame, when this was not a removal.
    pub after: Option<RecordedPrimitive>,
}

/// A canonical scene retained once even when many frames draw it.
#[derive(Clone, Debug)]
pub struct DistinctScene {
    /// Hash used to index the scene.
    pub hash: u64,
    /// Primitives in paint order.
    pub primitives: Arc<[RecordedPrimitive]>,
}

/// The compact index entry for one frame.
#[derive(Clone, Debug)]
pub struct SceneFrame<E> {
    /// Zero-based frame number.
    pub number: usize,
    /// The event explicitly associated with this frame.
    pub event: Option<E>,
    /// Index into [`SceneRecorder::distinct_scenes`].
    pub distinct_scene: usize,
    /// Primitives changed from the preceding frame.
    pub changes: Arc<[PrimitiveChange]>,
    /// Union of the clipped bounds of all changed primitives.
    pub change_bounds: Option<Bounds<ScaledPixels>>,
    /// Per-owner hashes used by flicker and damage oracles.
    pub subscenes: Arc<[FrameSubscene]>,
}

#[derive(Debug)]
struct Recording<E> {
    next_event: Option<E>,
    frames: Vec<SceneFrame<E>>,
    distinct_scenes: Vec<DistinctScene>,
    scenes_by_hash: HashMap<u64, Vec<usize>>,
    previous: HashMap<SceneOwner, PreviousSubscene>,
    owners: HashMap<SceneOwner, SubsceneId>,
    owner_index: Vec<SceneOwner>,
}

#[derive(Debug)]
struct PreviousSubscene {
    hash: u64,
    bounds: Bounds<ScaledPixels>,
    primitives: Vec<RecordedPrimitive>,
}

impl<E> Default for Recording<E> {
    fn default() -> Self {
        Self {
            next_event: None,
            frames: Vec::new(),
            distinct_scenes: Vec::new(),
            scenes_by_hash: HashMap::new(),
            previous: HashMap::new(),
            owners: HashMap::new(),
            owner_index: Vec::new(),
        }
    }
}

/// Records real GPUI scenes from a test window without pixels or a renderer.
///
/// `E` is deliberately supplied by the driver. A fuzzer can therefore use
/// the same typed event in its sequence, its frame index, and its regression
/// case without teaching GPUI about application-specific events.
#[derive(Clone, Debug)]
pub struct SceneRecorder<E>(Rc<RefCell<Recording<E>>>);

impl<E> Default for SceneRecorder<E> {
    fn default() -> Self {
        Self(Rc::new(RefCell::new(Recording::default())))
    }
}

impl<E: Clone + 'static> SceneRecorder<E> {
    /// Attributes the next drawn frame to `event`.
    pub fn precede(&self, event: E) {
        self.0.borrow_mut().next_event = Some(event);
    }

    /// Every frame recorded so far.
    pub fn frames(&self) -> Vec<SceneFrame<E>> {
        self.0.borrow().frames.clone()
    }

    /// Every distinct scene, in first-seen order.
    pub fn distinct_scenes(&self) -> Vec<DistinctScene> {
        self.0.borrow().distinct_scenes.clone()
    }

    /// Resolves a recorder-local sub-scene identity to its typed owner.
    pub fn owner(&self, id: SubsceneId) -> Option<SceneOwner> {
        self.0.borrow().owner_index.get(id.0).cloned()
    }

    pub(crate) fn callback(&self) -> Rc<dyn Fn(&Scene)> {
        let state = self.0.clone();
        Rc::new(move |scene| record(&mut state.borrow_mut(), scene))
    }
}

impl SceneOwner {
    /// The nearest element declaration allowing this owner to change with time.
    pub fn live_owner(&self) -> Option<&crate::LiveOwner> {
        self.elements
            .iter()
            .rev()
            .find_map(|element| match element {
                ElementId::LiveOwner(owner) => Some(owner),
                _ => None,
            })
    }
}

fn record<E: Clone>(recording: &mut Recording<E>, scene: &Scene) {
    let primitives: Arc<[RecordedPrimitive]> = scene.recorded_primitives().into();
    let hash = scene.recorded_hash();
    let distinct_scene = recording
        .scenes_by_hash
        .get(&hash)
        .into_iter()
        .flatten()
        .copied()
        .find(|index| recording.distinct_scenes[*index].primitives == primitives)
        .unwrap_or_else(|| {
            let index = recording.distinct_scenes.len();
            recording.distinct_scenes.push(DistinctScene {
                hash,
                primitives: primitives.clone(),
            });
            recording
                .scenes_by_hash
                .entry(hash)
                .or_default()
                .push(index);
            index
        });

    let mut grouped: HashMap<SceneOwner, PreviousSubscene> = HashMap::new();
    let mut owner_order = Vec::new();
    for primitive in primitives.iter() {
        let entry = grouped.entry(primitive.owner.clone()).or_insert_with(|| {
            owner_order.push(primitive.owner.clone());
            PreviousSubscene {
                hash: 0xcbf29ce484222325,
                bounds: primitive.bounds,
                primitives: Vec::new(),
            }
        });
        entry.hash ^= primitive.fingerprint;
        entry.hash = entry.hash.wrapping_mul(0x100000001b3);
        entry.bounds = entry.bounds.union(&primitive.bounds);
        entry.primitives.push(primitive.clone());
    }

    for owner in &owner_order {
        if !recording.owners.contains_key(owner) {
            let next = SubsceneId(recording.owners.len());
            recording.owners.insert(owner.clone(), next);
            recording.owner_index.push(owner.clone());
        }
    }

    let mut changes = Vec::new();
    let mut change_bounds: Option<Bounds<ScaledPixels>> = None;
    let mut changed_owners = owner_order.clone();
    changed_owners.extend(
        recording
            .previous
            .keys()
            .filter(|owner| !grouped.contains_key(*owner))
            .cloned(),
    );
    changed_owners.sort_by_key(|owner| recording.owners[owner].0);
    for owner in changed_owners {
        let before = recording.previous.get(&owner);
        let after = grouped.get(&owner);
        if before
            .zip(after)
            .is_some_and(|(before, after)| before.hash == after.hash)
        {
            continue;
        }
        let subscene = recording.owners[&owner];
        let before_primitives = before.map_or(&[][..], |value| value.primitives.as_slice());
        let after_primitives = after.map_or(&[][..], |value| value.primitives.as_slice());
        let count = before_primitives.len().max(after_primitives.len());
        for index in 0..count {
            let before = before_primitives.get(index);
            let after = after_primitives.get(index);
            if before == after {
                continue;
            }
            for primitive in before.into_iter().chain(after) {
                change_bounds = Some(match change_bounds {
                    Some(bounds) => bounds.union(&primitive.bounds),
                    None => primitive.bounds,
                });
            }
            changes.push(PrimitiveChange {
                subscene,
                index,
                before: before.cloned(),
                after: after.cloned(),
            });
        }
    }

    let mut subscenes = owner_order
        .iter()
        .map(|owner| {
            let value = &grouped[owner];
            FrameSubscene {
                id: recording.owners[owner],
                hash: value.hash,
                primitive_count: value.primitives.len(),
                bounds: value.bounds,
            }
        })
        .collect::<Vec<_>>();
    subscenes.sort_by_key(|subscene| subscene.id.0);

    recording.frames.push(SceneFrame {
        number: recording.frames.len(),
        event: recording.next_event.take(),
        distinct_scene,
        changes: changes.into(),
        change_bounds,
        subscenes: subscenes.into(),
    });
    recording.previous = grouped;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentMask, Quad, point, size};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Event {
        Initial,
        Key(char),
    }

    #[test]
    fn typed_live_owner_is_read_from_the_element_path() {
        let cadence = std::time::Duration::from_secs(1);
        let owner = SceneOwner {
            view: None,
            elements: vec![
                ElementId::from("row"),
                crate::LiveOwner::every("connection", cadence).into(),
            ]
            .into(),
            vertical_band: 0,
        };
        assert_eq!(owner.live_owner().map(|owner| owner.cadence), Some(cadence));
    }

    fn quad(top: f32) -> Quad {
        let bounds = Bounds::new(
            point(ScaledPixels(0.), ScaledPixels(top)),
            size(ScaledPixels(40.), ScaledPixels(10.)),
        );
        Quad {
            bounds,
            content_mask: ContentMask { bounds },
            ..Default::default()
        }
    }

    #[test]
    fn attributes_deduplicates_and_partitions_headless_scenes() {
        let recorder = SceneRecorder::default();
        let callback = recorder.callback();

        let mut first = Scene::default();
        first.insert_primitive(quad(0.));
        first.insert_primitive(quad(20.));
        recorder.precede(Event::Initial);
        callback(&first);

        let mut identical = Scene::default();
        identical.replay(0..first.len(), &first);
        callback(&identical);

        let mut changed = Scene::default();
        changed.insert_primitive(quad(0.));
        changed.insert_primitive(quad(24.));
        recorder.precede(Event::Key('x'));
        callback(&changed);

        let frames = recorder.frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].event, Some(Event::Initial));
        assert_eq!(frames[1].event, None);
        assert!(frames[1].changes.is_empty());
        assert_eq!(frames[0].distinct_scene, frames[1].distinct_scene);
        assert_eq!(frames[0].subscenes.len(), 2);
        assert_eq!(frames[2].event, Some(Event::Key('x')));
        assert_eq!(frames[2].changes.len(), 2);
        assert_ne!(frames[1].distinct_scene, frames[2].distinct_scene);
        assert_eq!(recorder.distinct_scenes().len(), 2);
    }
}
