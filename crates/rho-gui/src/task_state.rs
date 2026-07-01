//! Temporary task-board stub.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TaskId(pub(crate) u64);

#[derive(Default)]
pub(crate) struct TaskState;

impl TaskState {
    pub(crate) fn task_agent(&self, _id: TaskId) -> Option<String> { None }

    pub(crate) fn topic_groups(&self) -> Vec<TopicGroup> { Vec::new() }

    pub(crate) fn render_full_board(&self) -> BoardRender {
        BoardRender {
            text: "Tasks\n\n  Task board is unavailable until rho has native task state.\n".to_owned(),
            rows: Vec::new(),
        }
    }
}

pub(crate) struct BoardRender {
    pub(crate) text: String,
    pub(crate) rows: Vec<BoardRowRange>,
}

pub(crate) struct BoardRowRange {
    pub(crate) task_id: TaskId,
    pub(crate) range: Range<usize>,
}

#[derive(Clone)]
pub(crate) struct TopicGroup {
    pub(crate) name: String,
    pub(crate) agents: Vec<String>,
}
