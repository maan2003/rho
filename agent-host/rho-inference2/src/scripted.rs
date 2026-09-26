//! A model that answers from a script, for tests and dry runs.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::{Call, Carry, Inner, Request, Step, Stream, Usage};

enum Scripting {
    Call(String),
    Prose,
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
        let (code, cut) = match next {
            Scripting::Call(code) => (Some(code), false),
            Scripting::Prose => (None, false),
            Scripting::Cut(code) => (Some(code), true),
        };
        let call = code.map(|code| {
            let mut n = self.calls.lock().unwrap();
            *n += 1;
            Call {
                id: format!("call_{n}"),
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
            prose: if call.is_none() {
                "prose".into()
            } else {
                String::new()
            },
            carry: Carry(Inner::Scripted { call: call.clone() }),
            call,
            usage: Usage::default(),
        })
    }
}
