use std::collections::HashMap;

use crate::transcript::InsertedTranscript;

pub(crate) struct ShellCommandState {
    pub(crate) inserted: InsertedTranscript,
    pub(crate) command: String,
    pub(crate) include_in_context: bool,
    pub(crate) output: String,
}

#[derive(Default)]
pub(crate) struct ShellState {
    commands: HashMap<String, ShellCommandState>,
}

impl ShellState {
    pub(crate) fn insert(&mut self, command_id: String, state: ShellCommandState) {
        self.commands.insert(command_id, state);
    }

    pub(crate) fn take(&mut self, command_id: &str) -> Option<ShellCommandState> {
        self.commands.remove(command_id)
    }
}
