//! The composition model: surfaces are the unit of content (emacs buffers),
//! panes are viewports over them arranged in a split tree (emacs windows).
//!
//! The tree is generic over the surface type and only concerns display:
//! surfaces themselves are retained by the workspace's per-context surface
//! list, so panes can shuffle, cover, or close without losing content.
//! Splitting takes the new pane's surface by value: the caller builds a
//! fresh view over the same content, so each pane owns its own cursor and
//! scroll. Each pane keeps a history of surfaces it displayed so "go back"
//! is per-viewport, like emacs window history.

use camino::Utf8PathBuf;
use rho_ui_proto::AgentId;

/// Stable identity of a surface, independent of any view entity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SurfaceKey {
    /// The new-agent compose surface.
    Draft,
    /// An agent's conversation (transcript + prompt).
    Transcript(AgentId),
    /// A file from an agent's workspace, editable over the zed channel.
    File {
        agent_id: AgentId,
        path: Utf8PathBuf,
    },
    /// A daemon-owned terminal attached over a dedicated stream.
    Terminal { agent_id: AgentId, terminal_id: u64 },
}

impl SurfaceKey {
    /// Conversation content (the draft composer, agent transcripts), as
    /// opposed to artifacts the user opened deliberately (files,
    /// terminals). Display policy routes conversations through panes
    /// already showing conversation, so switching agents never covers an
    /// artifact pane.
    pub fn is_conversation(&self) -> bool {
        matches!(self, SurfaceKey::Draft | SurfaceKey::Transcript(_))
    }
}

pub type PaneId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    /// Children side by side.
    Row,
    /// Children stacked.
    Column,
}

pub struct Pane<S> {
    pub id: PaneId,
    pub surface: S,
    /// Previously shown surfaces, most recent last.
    history: Vec<S>,
}

impl<S: PartialEq> Pane<S> {
    pub fn show(&mut self, surface: S) {
        if surface == self.surface {
            return;
        }
        let previous = std::mem::replace(&mut self.surface, surface);
        self.history.retain(|s| *s != previous);
        self.history.push(previous);
    }

    /// Returns to the previously shown surface, if any.
    pub fn back(&mut self) -> bool {
        match self.history.pop() {
            Some(previous) => {
                self.surface = previous;
                true
            }
            None => false,
        }
    }
}

enum Node<S> {
    Leaf(Pane<S>),
    Split {
        axis: SplitAxis,
        children: Vec<Node<S>>,
    },
}

/// A binary-ish split tree of panes plus a focus.
pub struct PaneTree<S> {
    root: Node<S>,
    focused: PaneId,
    next_id: PaneId,
}

impl<S: PartialEq> PaneTree<S> {
    pub fn new(initial: S) -> Self {
        Self {
            root: Node::Leaf(Pane {
                id: 0,
                surface: initial,
                history: Vec::new(),
            }),
            focused: 0,
            next_id: 1,
        }
    }

    pub fn focused_id(&self) -> PaneId {
        self.focused
    }

    pub fn focused(&self) -> &Pane<S> {
        self.pane(self.focused).expect("focused pane exists")
    }

    pub fn focused_mut(&mut self) -> &mut Pane<S> {
        let focused = self.focused;
        self.pane_mut(focused).expect("focused pane exists")
    }

    pub fn focus(&mut self, id: PaneId) {
        if self.pane(id).is_some() {
            self.focused = id;
        }
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane<S>> {
        self.panes().into_iter().find(|pane| pane.id == id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane<S>> {
        fn find<S>(node: &mut Node<S>, id: PaneId) -> Option<&mut Pane<S>> {
            match node {
                Node::Leaf(pane) => (pane.id == id).then_some(pane),
                Node::Split { children, .. } => {
                    children.iter_mut().find_map(|child| find(child, id))
                }
            }
        }
        find(&mut self.root, id)
    }

    /// All panes in visual order (depth-first).
    pub fn panes(&self) -> Vec<&Pane<S>> {
        fn walk<'a, S>(node: &'a Node<S>, out: &mut Vec<&'a Pane<S>>) {
            match node {
                Node::Leaf(pane) => out.push(pane),
                Node::Split { children, .. } => {
                    for child in children {
                        walk(child, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }

    /// The first pane whose current surface matches, if any.
    pub fn pane_showing(&self, pred: impl Fn(&S) -> bool) -> Option<PaneId> {
        self.panes()
            .iter()
            .find(|pane| pred(&pane.surface))
            .map(|pane| pane.id)
    }

    /// Splits the focused pane along `axis`; the new pane shows `surface`
    /// and takes focus. The caller builds the sibling surface (usually a
    /// fresh view over the same content — each pane owns its own cursor).
    pub fn split(&mut self, axis: SplitAxis, surface: S) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        let focused = self.focused;
        fn split_at<S>(
            node: &mut Node<S>,
            target: PaneId,
            axis: SplitAxis,
            sibling: Pane<S>,
        ) -> Result<(), Pane<S>> {
            match node {
                Node::Leaf(pane) if pane.id == target => {
                    let old = std::mem::replace(
                        node,
                        Node::Split {
                            axis,
                            children: Vec::new(),
                        },
                    );
                    let Node::Split { children, .. } = node else {
                        unreachable!()
                    };
                    children.push(old);
                    children.push(Node::Leaf(sibling));
                    Ok(())
                }
                Node::Leaf(_) => Err(sibling),
                Node::Split {
                    axis: node_axis,
                    children,
                } => {
                    // Splitting along the parent's own axis just inserts a
                    // sibling instead of nesting another level.
                    if *node_axis == axis
                        && let Some(index) = children.iter().position(
                            |child| matches!(child, Node::Leaf(pane) if pane.id == target),
                        )
                    {
                        children.insert(index + 1, Node::Leaf(sibling));
                        return Ok(());
                    }
                    let mut sibling = sibling;
                    for child in children.iter_mut() {
                        match split_at(child, target, axis, sibling) {
                            Ok(()) => return Ok(()),
                            Err(returned) => sibling = returned,
                        }
                    }
                    Err(sibling)
                }
            }
        }
        let sibling = Pane {
            id,
            surface,
            history: Vec::new(),
        };
        if split_at(&mut self.root, focused, axis, sibling).is_ok() {
            self.focused = id;
        }
        id
    }

    /// Closes the focused pane, dropping its surface and history. The last
    /// pane never closes.
    pub fn close_focused(&mut self) {
        let target = self.focused;
        fn remove<S>(node: &mut Node<S>, target: PaneId) -> bool {
            let Node::Split { children, .. } = node else {
                return false;
            };
            if let Some(index) = children
                .iter()
                .position(|child| matches!(child, Node::Leaf(pane) if pane.id == target))
            {
                children.remove(index);
                if children.len() == 1 {
                    *node = children.pop().unwrap();
                }
                return true;
            }
            for child in children.iter_mut() {
                if remove(child, target) {
                    return true;
                }
            }
            false
        }
        if remove(&mut self.root, target) {
            let panes = self.panes();
            self.focused = panes.first().map(|pane| pane.id).unwrap_or(0);
        }
    }

    /// Moves focus to the next/previous pane in visual order.
    pub fn focus_by_delta(&mut self, delta: isize) {
        let panes = self.panes();
        if panes.is_empty() {
            return;
        }
        let current = panes
            .iter()
            .position(|pane| pane.id == self.focused)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(panes.len() as isize) as usize;
        self.focused = panes[next].id;
    }

    /// Renders the tree by calling `leaf` for each pane, composing splits
    /// with the given container builders.
    pub fn layout<E>(
        &self,
        leaf: &mut dyn FnMut(&Pane<S>) -> E,
        container: &mut dyn FnMut(SplitAxis, Vec<E>) -> E,
    ) -> E {
        fn walk<S, E>(
            node: &Node<S>,
            leaf: &mut dyn FnMut(&Pane<S>) -> E,
            container: &mut dyn FnMut(SplitAxis, Vec<E>) -> E,
        ) -> E {
            match node {
                Node::Leaf(pane) => leaf(pane),
                Node::Split { axis, children } => {
                    let children = children
                        .iter()
                        .map(|child| walk(child, leaf, container))
                        .collect();
                    container(*axis, children)
                }
            }
        }
        walk(&self.root, leaf, container)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(n: u64) -> SurfaceKey {
        let id = AgentId::from_counter(n, &rho_ui_proto::AgentIdDomain(0)).unwrap();
        SurfaceKey::Transcript(id)
    }

    #[test]
    fn split_close_focus_cycle() {
        let mut tree = PaneTree::new(SurfaceKey::Draft);
        let right = tree.split(SplitAxis::Row, transcript(1));
        assert_eq!(tree.focused_id(), right);
        assert_eq!(tree.panes().len(), 2);

        // Sibling insertion instead of nesting on same-axis splits.
        tree.split(SplitAxis::Row, transcript(2));
        assert_eq!(tree.panes().len(), 3);

        tree.focus_by_delta(1);
        let focused_before = tree.focused_id();
        tree.close_focused();
        assert_eq!(tree.panes().len(), 2);
        assert!(tree.pane(focused_before).is_none());
    }

    #[test]
    fn history_back() {
        let mut tree = PaneTree::new(SurfaceKey::Draft);
        tree.focused_mut().show(transcript(1));
        tree.focused_mut().show(transcript(2));
        assert!(tree.focused_mut().back());
        assert_eq!(tree.focused().surface, transcript(1));
        assert!(tree.focused_mut().back());
        assert_eq!(tree.focused().surface, SurfaceKey::Draft);
        assert!(!tree.focused_mut().back());
    }
}
