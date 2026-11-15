//! The browser pages the GUI holds open.
//!
//! A page is not the GUI's to keep. The browser owns it, and a page is
//! closed again after a grace period, long enough that a tab the reader is
//! still using is not lost, short enough that a day's browsing does not
//! accumulate.
//!
//! What is held here is that bookkeeping. The timers themselves are spawned by
//! the host, because a task that outlives this state has to be held by
//! something that outlives it too.

use std::collections::HashMap;

use gpui::Task;
use rho_browser::PageId;

/// How long a page is kept before it is closed.
pub(crate) const GRACE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Which pages are waiting to be closed.
#[derive(Default)]
pub(crate) struct Pages {
    /// Pages on their way out, each with the timer that will close it.
    closing: HashMap<PageId, Task<()>>,
}

impl Pages {
    /// Whether a page is already waiting out its grace period, so a second
    /// look at it does not start a second timer.
    pub(crate) fn is_closing(&self, page: PageId) -> bool {
        self.closing.contains_key(&page)
    }

    /// Holds the timer that will close `page`.
    pub(crate) fn closing(&mut self, page: PageId, timer: Task<()>) {
        self.closing.insert(page, timer);
    }

    /// Forgets a page's timer once it has fired.
    pub(crate) fn not_closing(&mut self, page: PageId) {
        self.closing.remove(&page);
    }
}
