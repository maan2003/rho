//! Which pages are on their way out.
//!
//! A page is not the GUI's to keep. The browser owns it, and a page no
//! surface shows is closed after [`GRACE`], long enough that a tab the
//! reader stepped away from is still there when they come back, short
//! enough that a day's browsing does not accumulate. Every tab the browser
//! has open and no surface shows waits out exactly one grace period:
//! one the reader closed, one the browser restored from its last session,
//! one a page opened by itself. Showing a page again takes its timer back.
//!
//! The timers are the system's, the one thing that outlives every surface;
//! this is the bookkeeping that says when one starts and stops, kept
//! apart from gpui so it can be read and tested on its own. A timer is
//! held as `T`: dropping it is what cancels it.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::PageId;

/// How long a page no surface shows is kept before it is closed.
pub(crate) const GRACE: Duration = Duration::from_secs(10 * 60);

pub(crate) struct Pages<T> {
    /// The tabs the browser said it has open.
    live: HashSet<PageId>,
    /// The pages a surface shows.
    shown: HashSet<PageId>,
    /// Live pages no surface shows, each with the timer that closes it.
    closing: HashMap<PageId, T>,
}

/// What a change in the browser's tabs asks of the system.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Moved {
    /// Live pages nothing shows that have no timer yet.
    pub start: Vec<PageId>,
    /// Pages a surface shows that the browser no longer has: the tab
    /// closed itself, or was closed in the browser.
    pub gone: Vec<PageId>,
}

impl<T> Default for Pages<T> {
    fn default() -> Self {
        Self {
            live: HashSet::new(),
            shown: HashSet::new(),
            closing: HashMap::new(),
        }
    }
}

impl<T> Pages<T> {
    /// A surface shows `page`: whatever timer it had is dropped.
    pub fn show(&mut self, page: PageId) {
        self.shown.insert(page);
        self.closing.remove(&page);
    }

    /// The surface showing `page` went. Answers whether a timer starts.
    pub fn hide(&mut self, page: PageId) -> bool {
        self.shown.remove(&page);
        self.live.contains(&page) && !self.closing.contains_key(&page)
    }

    /// The browser's tabs are now `live`. A page that is gone takes its
    /// timer with it.
    pub fn live(&mut self, live: HashSet<PageId>) -> Moved {
        let mut gone: Vec<PageId> = self
            .live
            .difference(&live)
            .filter(|page| self.shown.contains(page))
            .copied()
            .collect();
        self.closing.retain(|page, _| live.contains(page));
        let mut start: Vec<PageId> = live
            .iter()
            .filter(|page| !self.shown.contains(page) && !self.closing.contains_key(page))
            .copied()
            .collect();
        self.live = live;
        start.sort();
        gone.sort();
        Moved { start, gone }
    }

    /// Holds the timer that will close `page`.
    pub fn closing(&mut self, page: PageId, timer: T) {
        self.closing.insert(page, timer);
    }

    /// `page`'s timer fired. Answers whether it still held the page: one
    /// taken back since is not to close anything.
    pub fn fired(&mut self, page: PageId) -> bool {
        self.closing.remove(&page).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(n: u128) -> PageId {
        PageId(uuid::Uuid::from_u128(n))
    }

    fn live(pages: &[PageId]) -> HashSet<PageId> {
        pages.iter().copied().collect()
    }

    /// The timers held, by the page they close.
    fn timers(pages: &Pages<u32>) -> Vec<PageId> {
        let mut held: Vec<PageId> = pages.closing.keys().copied().collect();
        held.sort();
        held
    }

    #[test]
    fn a_shown_page_never_waits_and_the_rest_wait_once() {
        let (shown, restored) = (page(1), page(2));
        let mut pages = Pages::<u32>::default();
        pages.show(shown);
        let moved = pages.live(live(&[shown, restored]));
        assert_eq!(moved.start, vec![restored]);
        assert!(moved.gone.is_empty());
        pages.closing(restored, 1);
        // A second word from the browser starts nothing twice.
        assert_eq!(pages.live(live(&[shown, restored])), Moved::default());
        assert_eq!(timers(&pages), vec![restored]);
    }

    #[test]
    fn showing_a_page_again_takes_its_timer_back() {
        let tab = page(1);
        let mut pages = Pages::<u32>::default();
        pages.live(live(&[tab]));
        pages.closing(tab, 1);
        pages.show(tab);
        assert!(timers(&pages).is_empty());
        // A timer taken back closes nothing if it fires anyway.
        assert!(!pages.fired(tab));
        // Hidden again, it starts over.
        assert!(pages.hide(tab));
        pages.closing(tab, 2);
        assert!(pages.fired(tab));
    }

    #[test]
    fn a_page_the_browser_never_had_starts_no_timer() {
        // A surface over a page id the browser does not know (a stale
        // card) closes without leaving a timer for nothing.
        let stale = page(9);
        let mut pages = Pages::<u32>::default();
        pages.show(stale);
        assert!(!pages.hide(stale));
    }

    #[test]
    fn a_page_the_browser_closed_takes_its_timer_and_its_surface() {
        let (shown, waiting, kept) = (page(1), page(2), page(3));
        let mut pages = Pages::<u32>::default();
        pages.show(shown);
        pages.live(live(&[shown, waiting, kept]));
        pages.closing(waiting, 1);
        pages.closing(kept, 2);
        let moved = pages.live(live(&[kept]));
        assert_eq!(moved.gone, vec![shown]);
        assert!(moved.start.is_empty());
        assert_eq!(timers(&pages), vec![kept]);
        // A page that was never live is not gone when it is still missing.
        let fresh = page(4);
        pages.show(fresh);
        assert!(pages.live(live(&[kept])).gone.is_empty());
    }
}
