use std::collections::HashMap;

use crate::transcript::InsertedTranscript;

#[derive(Default)]
pub(crate) struct ToolState {
    pending_calls: HashMap<String, InsertedTranscript>,
}

impl ToolState {
    pub(crate) fn contains_pending(&self, call_id: &str) -> bool {
        self.pending_calls.contains_key(call_id)
    }

    pub(crate) fn take_pending(&mut self, call_id: &str) -> Option<InsertedTranscript> {
        self.pending_calls.remove(call_id)
    }

    pub(crate) fn insert_pending(&mut self, call_id: String, inserted: InsertedTranscript) {
        self.pending_calls.insert(call_id, inserted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_has_no_pending_calls() {
        let state = ToolState::default();

        assert!(!state.contains_pending("call"));
    }
}
