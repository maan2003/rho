use std::collections::HashSet;

#[derive(Default)]
pub(crate) struct MainToolActivity {
    completed: u64,
    total: u64,
    visible: bool,
    backgrounded_tools: HashSet<String>,
}

impl MainToolActivity {
    pub(crate) fn add_requested_tools(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.total = self.total.saturating_add(count as u64);
        self.visible = true;
    }

    pub(crate) fn record_backgrounded(&mut self, call_id: &str) {
        self.backgrounded_tools.insert(call_id.to_owned());
        if self.total != 0 {
            self.visible = true;
        }
    }

    pub(crate) fn record_completed(&mut self, call_id: &str) {
        self.backgrounded_tools.remove(call_id);
        if self.completed < self.total {
            self.completed += 1;
        }
        if self.total != 0 {
            self.visible = true;
        }
    }

    pub(crate) fn is_backgrounded(&self, call_id: &str) -> bool {
        self.backgrounded_tools.contains(call_id)
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_requested_backgrounded_and_completed_tools() {
        let mut activity = MainToolActivity::default();

        activity.add_requested_tools(2);
        activity.record_backgrounded("call-1");
        assert!(activity.is_backgrounded("call-1"));

        activity.record_completed("call-1");
        assert!(!activity.is_backgrounded("call-1"));
    }

    #[test]
    fn reset_clears_visible_status() {
        let mut activity = MainToolActivity::default();
        activity.add_requested_tools(1);
        activity.record_backgrounded("call-1");

        activity.reset();

        assert!(!activity.is_backgrounded("call-1"));
    }
}
