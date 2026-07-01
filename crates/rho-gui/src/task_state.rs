//! The board's projection of the factory's tasks.
//!
//! The factory owns the durable task store; the UI keeps a derived, in-memory
//! mirror folded from the factory's `factory.tasks_update` custom events. Each
//! event carries a `Vec<Task>` that is a batch of upserts keyed by `TaskId`:
//! just the changed task on a mutation, the whole list in response to the
//! `factory.sync` the UI emits on connect. Tasks are never removed (closing is
//! a status), so the fold is a pure upsert with no tombstones.

use std::collections::BTreeMap;
use std::ops::Range;

use tau_proto::{CborValue, CustomEvent, Event, EventCategory, EventName, HarnessInputMessage};
use tau_task::{Attention, Status, Task, TaskId, Topic, wire};

#[derive(Default)]
pub(crate) struct TaskState {
    tasks: BTreeMap<TaskId, Task>,
    topics: BTreeMap<String, Topic>,
}

impl TaskState {
    /// Folds a `factory.tasks_update` into the mirror. Any other event is
    /// ignored, so this can be called for every delivered event.
    pub(crate) fn observe_event(&mut self, event: &Event) -> bool {
        let Event::ExtensionEvent(custom) = event else {
            return false;
        };
        if is_tasks_update(custom.name()) {
            match custom.payload().deserialized::<Vec<Task>>() {
                Ok(tasks) => {
                    let mut changed = false;
                    for task in tasks {
                        changed |= self.tasks.get(&task.id) != Some(&task);
                        self.tasks.insert(task.id, task);
                    }
                    changed
                }
                Err(error) => {
                    eprintln!("rho-gui: ignoring malformed factory.tasks_update: {error}");
                    false
                }
            }
        } else if is_topics_update(custom.name()) {
            match custom.payload().deserialized::<Vec<Topic>>() {
                Ok(topics) => {
                    let mut changed = false;
                    for topic in topics {
                        changed |= self.topics.get(&topic.id.0) != Some(&topic);
                        self.topics.insert(topic.id.0.clone(), topic);
                    }
                    changed
                }
                Err(error) => {
                    eprintln!("rho-gui: ignoring malformed factory.topics_update: {error}");
                    false
                }
            }
        } else {
            false
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.tasks.len()
    }

    /// How many tasks are waiting on the human right now — useful internally
    /// and in tests, even though the board intentionally does not print the
    /// count.
    #[cfg(test)]
    pub(crate) fn attention_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|task| task.attention.is_some())
            .count()
    }

    pub(crate) fn task_agent(&self, id: TaskId) -> Option<String> {
        self.tasks.get(&id).and_then(|task| match &task.status {
            Status::Active { agent, .. } | Status::Done { agent } => Some(agent.to_string()),
            Status::Closed { agent } => agent.as_ref().map(ToString::to_string),
            Status::Open => None,
        })
    }

    pub(crate) fn topic_groups(&self) -> Vec<TopicGroup> {
        let mut topics = self
            .topics
            .values()
            .filter(|topic| !topic.archived)
            .cloned()
            .collect::<Vec<_>>();
        topics.sort_by_key(|topic| (topic.name.to_lowercase(), topic.id.0.clone()));
        topics
            .into_iter()
            .map(|topic| {
                let mut agents = topic
                    .agents
                    .iter()
                    .map(|agent| agent.agent_id.to_string())
                    .collect::<Vec<_>>();
                agents.sort();
                TopicGroup {
                    name: topic.name,
                    agents,
                }
            })
            .collect()
    }

    /// The full task board as generated text, plus task anchor ranges expressed
    /// as byte offsets in that text. The UI turns these offsets into buffer
    /// anchors after inserting the text.
    pub(crate) fn render_full_board(&self) -> BoardRender {
        if self.tasks.is_empty() {
            return BoardRender {
                text: "Tasks\n\n  No tasks yet. Create one with /factory new <title>.\n".to_owned(),
                rows: Vec::new(),
            };
        }

        let mut render = BoardRender {
            text: "Tasks\n".to_owned(),
            rows: Vec::new(),
        };

        self.push_section(
            &mut render,
            "Needs you",
            self.tasks
                .values()
                .filter(|task| task.attention.is_some())
                .collect(),
            None,
        );
        self.push_section(
            &mut render,
            "Active",
            self.tasks
                .values()
                .filter(|task| {
                    task.attention.is_none() && matches!(task.status, Status::Active { .. })
                })
                .collect(),
            None,
        );
        self.push_section(
            &mut render,
            "Open",
            self.tasks
                .values()
                .filter(|task| task.attention.is_none() && matches!(task.status, Status::Open))
                .collect(),
            Some(3),
        );

        if self
            .tasks
            .values()
            .any(|task| task.attention.is_none() && matches!(task.status, Status::Done { .. }))
        {
            render.text.push_str("\n[+] Done\n");
        }
        if self
            .tasks
            .values()
            .any(|task| task.attention.is_none() && matches!(task.status, Status::Closed { .. }))
        {
            render.text.push_str("\n[+] Closed\n");
        }

        render
    }

    fn push_section(
        &self,
        render: &mut BoardRender,
        heading: &str,
        mut tasks: Vec<&Task>,
        limit: Option<usize>,
    ) {
        if tasks.is_empty() {
            return;
        }
        tasks.sort_by_key(|task| {
            (
                attention_priority(task.attention),
                std::cmp::Reverse(task.updated_at),
                task.id,
            )
        });

        render.text.push_str(&format!("\n{heading}\n"));
        let shown = limit.unwrap_or(tasks.len()).min(tasks.len());
        for task in tasks.iter().take(shown) {
            let start = render.text.len();
            render.text.push_str("  ");
            render.text.push_str(&task.title);
            render.text.push('\n');
            let end = render.text.len();
            render.rows.push(BoardRowRange {
                task_id: task.id,
                range: start..end,
            });
        }
        if shown < tasks.len() {
            render.text.push_str("  … more\n");
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

/// The `HarnessInputMessage` the UI emits to ask the factory for the current
/// board. Custom events are not replayed, so this request is how a freshly
/// connected UI learns the tasks that already exist.
#[allow(dead_code)]
pub(crate) fn sync_request() -> HarnessInputMessage {
    let name = EventName::new(EventCategory::Other(wire::CATEGORY.to_owned()), wire::SYNC);
    let event = CustomEvent::try_new(name, CborValue::Null)
        .expect("factory.sync is a valid extension-owned event name");
    HarnessInputMessage::emit(Event::ExtensionEvent(event))
}

fn is_tasks_update(name: &EventName) -> bool {
    name.category().as_str() == wire::CATEGORY && name.call().as_str() == wire::TASKS_UPDATE
}

fn is_topics_update(name: &EventName) -> bool {
    name.category().as_str() == wire::CATEGORY && name.call().as_str() == wire::TOPICS_UPDATE
}

fn attention_priority(attention: Option<Attention>) -> u8 {
    match attention {
        Some(Attention::Decision) => 0,
        Some(Attention::Question) => 1,
        Some(Attention::Review) => 2,
        None => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn agent(name: &str) -> tau_proto::AgentId {
        tau_proto::AgentId::parse(name).expect("valid agent id")
    }

    fn task(id: u64, title: &str, status: Status, attention: Option<Attention>) -> Task {
        let now = chrono::Utc::now();
        Task {
            id: TaskId(id),
            title: title.to_owned(),
            issue: String::new(),
            status,
            attention,
            created_at: now,
            updated_at: now,
        }
    }

    fn tasks_update(tasks: &[Task]) -> Event {
        let name = EventName::new(
            EventCategory::Other(wire::CATEGORY.to_owned()),
            wire::TASKS_UPDATE,
        );
        let payload = CborValue::serialized(&tasks.to_vec()).expect("serialize tasks");
        Event::ExtensionEvent(CustomEvent::try_new(name, payload).expect("valid event"))
    }

    #[test]
    fn folds_updates_as_upserts_keyed_by_id() {
        let mut state = TaskState::default();
        state.observe_event(&tasks_update(&[
            task(1, "First", Status::Open, None),
            task(2, "Second", Status::Open, None),
        ]));
        assert_eq!(state.len(), 2);

        // A later delta for an existing id replaces, not appends.
        state.observe_event(&tasks_update(&[task(
            1,
            "First renamed",
            Status::Open,
            None,
        )]));
        assert_eq!(state.len(), 2);
        assert!(state.render_full_board().text.contains("First renamed"));
    }

    #[test]
    fn attention_tasks_lead_regardless_of_status() {
        let mut state = TaskState::default();
        state.observe_event(&tasks_update(&[
            task(1, "Plain open", Status::Open, None),
            task(
                2,
                "Patch ready",
                Status::Active {
                    worktree: PathBuf::from("/tmp/ws"),
                    agent: agent("impl-2"),
                },
                Some(Attention::Review),
            ),
        ]));

        assert_eq!(state.attention_count(), 1);
        let board = state.render_full_board().text;
        let needs_you = board.find("Needs you").expect("needs-you section");
        let open = board.find("\nOpen").expect("open section");
        assert!(needs_you < open, "needs-you section must come first");
        assert!(
            !board.contains("\nActive"),
            "the active task has attention, so it belongs under Needs you, not Active"
        );
        assert!(board.contains("Patch ready"));
        assert!(!board.contains("impl-2"));
    }

    #[test]
    fn open_tasks_are_limited_by_default() {
        let mut state = TaskState::default();
        state.observe_event(&tasks_update(&[
            task(1, "One", Status::Open, None),
            task(2, "Two", Status::Open, None),
            task(3, "Three", Status::Open, None),
            task(4, "Four", Status::Open, None),
        ]));

        let board = state.render_full_board().text;
        assert!(board.contains("… more"));
        assert_eq!(
            board.matches("\n  ").count(),
            4,
            "three tasks plus more row"
        );
    }

    #[test]
    fn non_factory_events_are_ignored() {
        let mut state = TaskState::default();
        state.observe_event(&Event::ExtensionEvent(
            CustomEvent::try_new(
                EventName::new(EventCategory::Other("other".to_owned()), "thing"),
                CborValue::Null,
            )
            .expect("valid event"),
        ));
        assert_eq!(state.len(), 0);
    }
}
