//! The browser pages the desk holds open.
//!
//! A page is not the desk's to keep. The browser owns it; the desk only
//! refers to it from a node on the map, and a page nothing refers to is
//! closed again after a grace period — long enough that deleting a row and
//! putting it back does not cost the reader their tab, short enough that a
//! day's browsing does not accumulate.
//!
//! What is held here is that bookkeeping and the page shown beside the
//! dashboard. The timers themselves are spawned by the host, because a
//! task that outlives this state has to be held by something that outlives
//! it too.

use std::collections::{HashMap, HashSet};

use gpui::{Entity, Subscription, Task};
use rho_browser::{PageId, PageView};

/// How long a page nothing refers to is kept before it is closed.
pub(crate) const GRACE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The page shown beside the dashboard, and the view drawing it.
pub(crate) struct Preview {
    pub(crate) id: PageId,
    pub(crate) view: Entity<PageView>,
}

/// Which pages the desk knows about, which are waiting to be closed, and
/// which one is being previewed.
#[derive(Default)]
pub(crate) struct Pages {
    /// What the map referred to at the last reconcile, kept only to notice
    /// that it has changed. Whether a page is referred to *now* is the
    /// map's answer, not this set's.
    known: HashSet<PageId>,
    /// Pages with nothing referring to them, each with the timer that will
    /// close it.
    closing: HashMap<PageId, Task<()>>,
    /// One subscription for the whole browser: the model polls the
    /// metadata revision, so a burst of new tabs arrives as one event.
    metadata: Option<Subscription>,
    preview: Option<Preview>,
}

impl Pages {
    /// Takes the set the map refers to now. Answers with the pages that
    /// have just lost their last reference, or `None` if nothing changed —
    /// the common case, since this runs on every dashboard refresh.
    pub(crate) fn reconcile(&mut self, current: HashSet<PageId>) -> Option<Vec<PageId>> {
        if current == self.known {
            return None;
        }
        // A page that has come back is no longer on its way out.
        for page in &current {
            self.closing.remove(page);
        }
        let unreferenced = self.known.difference(&current).copied().collect();
        self.known = current;
        Some(unreferenced)
    }

    /// Whether a page is already waiting out its grace period, so a second
    /// look at it does not start a second timer.
    pub(crate) fn is_closing(&self, page: PageId) -> bool {
        self.closing.contains_key(&page)
    }

    /// Holds the timer that will close `page`.
    pub(crate) fn closing(&mut self, page: PageId, timer: Task<()>) {
        self.closing.insert(page, timer);
    }

    /// Forgets a page's timer, either because it has just fired or because
    /// something refers to the page again.
    pub(crate) fn not_closing(&mut self, page: PageId) {
        self.closing.remove(&page);
    }

    /// Whether the browser is already being listened to.
    pub(crate) fn observed(&self) -> bool {
        self.metadata.is_some()
    }

    /// Keeps the subscription that says when the browser has moved.
    pub(crate) fn observe(&mut self, subscription: Subscription) {
        self.metadata = Some(subscription);
    }

    /// The page beside the dashboard, if one is being previewed.
    pub(crate) fn preview(&self) -> Option<&Preview> {
        self.preview.as_ref()
    }

    /// Whether `page` is the one already being previewed, so showing it
    /// again is not a change.
    pub(crate) fn previewing(&self, page: PageId) -> bool {
        self.preview.as_ref().is_some_and(|it| it.id == page)
    }

    /// Shows `page` beside the dashboard.
    pub(crate) fn preview_page(&mut self, id: PageId, view: Entity<PageView>) {
        self.preview = Some(Preview { id, view });
    }

    /// Stops previewing.
    pub(crate) fn clear_preview(&mut self) {
        self.preview = None;
    }
}
