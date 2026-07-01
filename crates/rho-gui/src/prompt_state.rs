use std::collections::{HashMap, VecDeque};

use crate::transcript::InsertedTranscript;

#[derive(Default)]
pub(crate) struct PromptState {
    queued_prompts: VecDeque<QueuedPrompt>,
    streamed_responses: HashMap<String, String>,
    live_response_ranges: HashMap<String, InsertedTranscript>,
    live_compaction_ranges: HashMap<String, InsertedTranscript>,
}

pub(crate) struct QueuedPrompt {
    pub(crate) text: String,
    pub(crate) inserted: InsertedTranscript,
}

impl PromptState {
    pub(crate) fn push_queued_prompt(&mut self, text: String, inserted: InsertedTranscript) {
        self.queued_prompts
            .push_back(QueuedPrompt { text, inserted });
    }

    pub(crate) fn pop_matching_queued_prompt(&mut self, text: &str) -> Option<QueuedPrompt> {
        if self
            .queued_prompts
            .front()
            .is_some_and(|queued| queued.text == text)
        {
            return self.queued_prompts.pop_front();
        }
        None
    }

    pub(crate) fn pop_front_queued_prompt(&mut self) -> Option<QueuedPrompt> {
        self.queued_prompts.pop_front()
    }

    pub(crate) fn pop_back_queued_prompt(&mut self) -> Option<QueuedPrompt> {
        self.queued_prompts.pop_back()
    }
    pub(crate) fn append_streamed_response(&mut self, key: String, text: String) -> String {
        let response = self.streamed_responses.entry(key).or_default();
        response.push_str(&text);
        response.clone()
    }

    pub(crate) fn remove_streamed_response(&mut self, key: &str) -> Option<String> {
        self.streamed_responses.remove(key)
    }

    pub(crate) fn take_live_response(&mut self, key: &str) -> Option<InsertedTranscript> {
        self.live_response_ranges.remove(key)
    }

    pub(crate) fn insert_live_response(&mut self, key: String, inserted: InsertedTranscript) {
        self.live_response_ranges.insert(key, inserted);
    }

    pub(crate) fn take_live_compaction(&mut self, key: &str) -> Option<InsertedTranscript> {
        self.live_compaction_ranges.remove(key)
    }

    pub(crate) fn insert_live_compaction(&mut self, key: String, inserted: InsertedTranscript) {
        self.live_compaction_ranges.insert(key, inserted);
    }

    pub(crate) fn remove_prompt(&mut self, key: &str) -> PromptCleanup {
        self.streamed_responses.remove(key);
        PromptCleanup {
            live_response: self.live_response_ranges.remove(key),
            live_compaction: self.live_compaction_ranges.remove(key),
        }
    }
}

pub(crate) struct PromptCleanup {
    pub(crate) live_response: Option<InsertedTranscript>,
    pub(crate) live_compaction: Option<InsertedTranscript>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streamed_responses_are_recorded_and_removed() {
        let mut state = PromptState::default();

        let _ = state.append_streamed_response("prompt".to_owned(), "text".to_owned());

        assert_eq!(
            state.remove_streamed_response("prompt").as_deref(),
            Some("text")
        );
        assert_eq!(state.remove_streamed_response("prompt"), None);
    }

    #[test]
    fn remove_prompt_clears_streamed_response_even_when_no_live_ranges_exist() {
        let mut state = PromptState::default();
        let _ = state.append_streamed_response("prompt".to_owned(), "text".to_owned());

        let cleanup = state.remove_prompt("prompt");

        assert!(cleanup.live_response.is_none());
        assert!(cleanup.live_compaction.is_none());
        assert_eq!(state.remove_streamed_response("prompt"), None);
    }
}
