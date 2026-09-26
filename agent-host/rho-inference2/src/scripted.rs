//! A model that answers from a script, for tests and dry runs.

use std::collections::VecDeque;
use std::sync::Mutex;

use futures::future::BoxFuture;

use crate::{Call, Carry, Inner, Model, Request, Step, Usage};

/// Answers each request with the next scripted cell, and keeps every request
/// it was sent. Out of script, it fails.
#[derive(Default)]
pub struct Scripted {
    steps: Mutex<VecDeque<Option<String>>>,
    requests: Mutex<Vec<Request>>,
    calls: Mutex<u64>,
}

impl Scripted {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next step calls `exec` with `code`.
    pub fn then(&self, code: &str) -> &Self {
        self.steps.lock().unwrap().push_back(Some(code.to_owned()));
        self
    }

    /// The next step writes prose and makes no call.
    pub fn then_prose(&self) -> &Self {
        self.steps.lock().unwrap().push_back(None);
        self
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn remaining(&self) -> usize {
        self.steps.lock().unwrap().len()
    }
}

impl Model for Scripted {
    fn step<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, anyhow::Result<Step>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request.clone());
            let next = self.steps.lock().unwrap().pop_front();
            let Some(next) = next else {
                anyhow::bail!("the script has ended");
            };
            let call = next.map(|code| {
                let mut n = self.calls.lock().unwrap();
                *n += 1;
                Call {
                    id: format!("call_{n}"),
                    code,
                }
            });
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
        })
    }
}
