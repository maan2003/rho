//! A model that answers from a script, for tests and dry runs.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::{Call, CallId, Carry, Inner, Request, Step, Stream, Usage};

enum Scripting {
    Call(String),
    Prose,
    Compaction,
    /// The call's code streams, then the response fails.
    Cut(String),
}

/// Answers each request with the next scripted cell, and keeps every request
/// it was sent. Out of script, it fails.
#[derive(Default)]
pub struct Scripted {
    steps: Mutex<VecDeque<Scripting>>,
    requests: Mutex<Vec<Request>>,
    calls: Mutex<u64>,
}

impl Scripted {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next step calls `exec` with `code`.
    pub fn then(&self, code: &str) -> &Self {
        self.steps
            .lock()
            .unwrap()
            .push_back(Scripting::Call(code.to_owned()));
        self
    }

    /// The next step writes prose and makes no call.
    pub fn then_prose(&self) -> &Self {
        self.steps.lock().unwrap().push_back(Scripting::Prose);
        self
    }

    /// The next step returns a provider compaction boundary without a call.
    pub fn then_compaction(&self) -> &Self {
        self.steps.lock().unwrap().push_back(Scripting::Compaction);
        self
    }

    /// The next step streams `code` and then fails, as a dropped connection
    /// would.
    pub fn then_cut(&self, code: &str) -> &Self {
        self.steps
            .lock()
            .unwrap()
            .push_back(Scripting::Cut(code.to_owned()));
        self
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn remaining(&self) -> usize {
        self.steps.lock().unwrap().len()
    }
}

impl Scripted {
    /// The next scripted step, its code streamed a line at a time.
    pub(crate) async fn step(
        &self,
        request: &Request,
        stream: &mut (dyn FnMut(Stream<'_>) + Send),
    ) -> anyhow::Result<Step> {
        self.requests.lock().unwrap().push(request.clone());
        let next = self.steps.lock().unwrap().pop_front();
        let Some(next) = next else {
            anyhow::bail!("the script has ended");
        };
        let (code, cut, compacted) = match next {
            Scripting::Call(code) => (Some(code), false, false),
            Scripting::Prose => (None, false, false),
            Scripting::Compaction => (None, false, true),
            Scripting::Cut(code) => (Some(code), true, false),
        };
        let call = code.map(|code| {
            let mut n = self.calls.lock().unwrap();
            *n += 1;
            Call {
                id: CallId::new(format!("call_{n}")),
                code,
            }
        });
        if let Some(call) = &call {
            stream(Stream::Call { id: &call.id });
            for line in call.code.split_inclusive('\n') {
                stream(Stream::Code(line));
                // As a provider would: the rest is still being written.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        if cut {
            anyhow::bail!("the connection dropped");
        }
        Ok(Step {
            prose: if call.is_none() && !compacted {
                "prose".into()
            } else {
                String::new()
            },
            carry: Carry(if compacted {
                Inner::ScriptedCompaction
            } else {
                Inner::Scripted { call: call.clone() }
            }),
            call,
            usage: Usage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Item;

    #[tokio::test]
    async fn scripted_compaction_marks_a_replay_boundary() {
        let model = Scripted::new();
        model.then("print(1)").then_compaction().then("print(2)");
        let request = Request {
            instructions: "hello".into(),
            items: vec![],
            cache_key: crate::CacheKey::new(),
        };
        let first = model.step(&request, &mut |_| {}).await.unwrap();
        assert!(!first.carry.has_compaction());
        let compacted = model.step(&request, &mut |_| {}).await.unwrap();
        assert!(compacted.carry.has_compaction());
        assert!(compacted.call.is_none());
        assert!(compacted.prose.is_empty());
        let later = model.step(&request, &mut |_| {}).await.unwrap();
        let replay = Request {
            items: vec![
                Item::Step(first.carry),
                Item::Step(compacted.carry),
                Item::Step(later.carry),
            ],
            ..request
        };
        model.then_prose();
        model.step(&replay, &mut |_| {}).await.unwrap();
        let seen = model.requests();
        assert_eq!(seen.len(), 4);
        assert!(matches!(&seen[3].items[1], Item::Step(carry) if carry.has_compaction()));
    }
}
