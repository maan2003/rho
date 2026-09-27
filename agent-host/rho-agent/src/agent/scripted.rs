//! A model that answers from a script, for tests and dry runs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::inference::{Call, Carry, Event, Request, Response, Step, Usage};

enum Scripting {
    Call(String),
    Prose,
    Transient,
    Compaction,
    /// The call's code streams, then the response fails.
    CutAfter(String, tokio::sync::oneshot::Receiver<()>),
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

    /// Stream code, then wait for an observed admission before dropping
    /// the scripted provider connection. Used to test a cut *after* Python
    /// has actually run the prefix, independently of machine scheduling.
    pub fn then_cut_after(
        &self,
        code: &str,
        admitted: tokio::sync::oneshot::Receiver<()>,
    ) -> &Self {
        self.steps
            .lock()
            .unwrap()
            .push_back(Scripting::CutAfter(code.to_owned(), admitted));
        self
    }

    /// The next exchange fails before admitting any code.
    pub fn then_transient(&self) -> &Self {
        self.steps.lock().unwrap().push_back(Scripting::Transient);
        self
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Scripted {
    pub fn start(self: &Arc<Self>, request: Request) -> Response {
        let (events, response) = tokio::sync::mpsc::unbounded_channel();
        let script = self.clone();
        tokio::spawn(async move {
            let mut forward = |event| {
                let _ = events.send(event);
            };
            let result = tokio::select! {
                biased;
                _ = events.closed() => return,
                result = script.step(&request, &mut forward) => result,
            };
            let _ = events.send(match result {
                Ok(step) => Event::Completed(step),
                Err(error) => Event::Failed(error),
            });
        });
        response
    }

    /// The next scripted step, its code streamed a line at a time.
    pub(crate) async fn step(
        &self,
        request: &Request,
        stream: &mut (dyn FnMut(Event) + Send),
    ) -> anyhow::Result<Step> {
        self.requests.lock().unwrap().push(request.clone());
        let next = self.steps.lock().unwrap().pop_front();
        let Some(next) = next else {
            anyhow::bail!("the script has ended");
        };
        let (code, cut, compacted, admitted) = match next {
            Scripting::Transient => {
                return Err(crate::inference::Retryable("temporary outage".into()).into());
            }
            Scripting::Call(code) => (Some(code), false, false, None),
            Scripting::Prose => (None, false, false, None),
            Scripting::Compaction => (None, false, true, None),
            Scripting::CutAfter(code, admitted) => (Some(code), true, false, Some(admitted)),
        };
        let call = code.map(|code| {
            let mut n = self.calls.lock().unwrap();
            *n += 1;
            Call::new(format!("call_{n}"), code)
        });
        if let Some(call) = &call {
            stream(Event::Call {
                carry: carry(call.clone()),
            });
            for line in call.code.split_inclusive('\n') {
                stream(Event::Code(line.to_owned()));
                // As a provider would: the rest is still being written.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        if let Some(admitted) = admitted {
            admitted
                .await
                .map_err(|_| anyhow::anyhow!("admission gate dropped"))?;
        }
        if cut {
            return Err(crate::inference::Retryable("the connection dropped".into()).into());
        }
        Ok(Step {
            continuation: None,
            prose: if call.is_none() && !compacted {
                "prose".into()
            } else {
                String::new()
            },
            carry: if compacted {
                Carry::new(serde_json::json!("script-compaction"), vec![], true)
            } else if let Some(call) = &call {
                carry(call.clone())
            } else {
                Carry::new(serde_json::Value::Null, vec![], false)
            },
            call,
            usage: Usage::default(),
        })
    }
}

impl crate::inference::Session for Arc<Scripted> {
    fn start(&self, request: Request) -> Response {
        Scripted::start(self, request)
    }
}
pub(super) fn carry(call: Call) -> Carry {
    Carry::new(
        serde_json::json!({"script":call.display_id()}),
        vec![call],
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::Item;

    #[tokio::test]
    async fn scripted_compaction_marks_a_replay_boundary() {
        let model = Scripted::new();
        model.then("print(1)").then_compaction().then("print(2)");
        let request = Request::new("hello".into(), vec![], crate::inference::CacheKey::new());
        let first = model.step(&request, &mut |_| {}).await.unwrap();
        assert!(!first.carry.has_compaction());
        let compacted = model.step(&request, &mut |_| {}).await.unwrap();
        assert!(compacted.carry.has_compaction());
        assert!(compacted.call.is_none());
        assert!(compacted.prose.is_empty());
        let later = model.step(&request, &mut |_| {}).await.unwrap();
        let replay = Request::new(
            request.instructions.clone(),
            vec![
                Item::Step {
                    carry: first.carry,
                    exec: None,
                },
                Item::Step {
                    carry: compacted.carry,
                    exec: None,
                },
                Item::Step {
                    carry: later.carry,
                    exec: None,
                },
            ],
            request.cache_key,
        );
        model.then_prose();
        model.step(&replay, &mut |_| {}).await.unwrap();
        let seen = model.requests();
        assert_eq!(seen.len(), 4);
        assert!(matches!(&seen[3].items()[1], Item::Step {carry,..} if carry.has_compaction()));
    }
}
