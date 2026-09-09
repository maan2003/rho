//! What a tool is to the agent loop, and the real tools in that shape.
//!
//! A process or a script is a [`ToolSession`]: it holds its output until the
//! core asks, says how urgent that output is, and is answered whenever the
//! next request goes out. Nothing here waits for a deadline of its own — a
//! command that takes a minute costs one request, one that takes an hour
//! costs the same one, and the model names any pace it wants with `wait`.

mod code_mode;
mod python;
mod shell;
#[cfg(test)]
mod tests;
mod tool;

use std::future::Future;
use std::sync::{Arc, Mutex};

pub use code_mode::CodeModeTool;
use futures::future::BoxFuture;
pub use python::{PythonExec, PythonTool};
use rho_core::{ToolCall, ToolExecutionContext, ToolOutput, ToolOutputStatus, ToolSpec, UnixMs};
use rho_tool_shell::ShellTools;
use rho_web_search::WebSearchTools;
pub use shell::ShellTool;
pub use tool::{
    PythonCompletion, PythonExecFacts, PythonOperationFacts, PythonOutput, SourceFacts,
    SourceWaker, Tool, ToolHaste, ToolSession,
};

/// A tool whose whole answer is one future. Usable directly by the model or
/// from a code-mode script, which is why it is not a [`Tool`] itself.
pub trait FutureTool: Send + Sync + 'static {
    fn spec(&self) -> ToolSpec;
    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput>;
}

impl FutureTool for WebSearchTools {
    fn spec(&self) -> ToolSpec {
        WebSearchTools::spec(self)
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        WebSearchTools::call(self, call, ToolExecutionContext::default())
    }
}

/// A [`FutureTool`] offered to the model directly.
pub struct Direct(pub Arc<dyn FutureTool>);

impl Tool for Direct {
    fn spec(&self) -> ToolSpec {
        self.0.spec()
    }

    fn run(&self, call: ToolCall, waker: SourceWaker) -> Box<dyn ToolSession> {
        Box::new(OneShot::spawn(waker, self.0.call(call)))
    }
}

/// Runtime for an agent's code-mode surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeMode {
    JavaScript,
    Python,
}

/// With `code_mode`, that is a single `exec` whose scripts reach the shell
/// and `others` as nested tools. Without it, every tool is called directly.
/// Must be called on a tokio runtime. Python mode starts an interpreter thread
/// and blocks briefly while it comes up.
pub fn tools(
    shell: ShellTools,
    others: Vec<Arc<dyn FutureTool>>,
    code_mode: Option<CodeMode>,
) -> Result<Vec<Arc<dyn Tool>>, String> {
    match code_mode {
        Some(CodeMode::Python) => return Ok(vec![Arc::new(PythonTool::new(shell, others)?)]),
        Some(CodeMode::JavaScript) => return Ok(vec![Arc::new(CodeModeTool::new(shell, others)?)]),
        None => {}
    }
    let mut tools = ShellTool::all(shell);
    tools.extend(
        others
            .into_iter()
            .map(|tool| Arc::new(Direct(tool)) as Arc<dyn Tool>),
    );
    Ok(tools)
}

/// A call that is over before it is asked anything: the result is its one
/// answer, and it has nothing to add.
pub(crate) struct Finished {
    output: ToolOutput,
    at: UnixMs,
}

impl Finished {
    pub(crate) fn new(output: ToolOutput) -> Self {
        Self {
            output,
            at: UnixMs::now(),
        }
    }

    pub(crate) fn error(text: impl Into<String>) -> Self {
        Self::new(output(text.into(), ToolOutputStatus::Error))
    }
}

impl ToolSession for Finished {
    fn sources(&self) -> Vec<(u64, crate::SourceFacts)> {
        let haste = ToolHaste::Ended { at: self.at };
        vec![(0, crate::SourceFacts::Tool(haste))]
    }

    fn done(&self) -> bool {
        true
    }

    fn first_output(&mut self) -> ToolOutput {
        self.output.clone()
    }

    fn more_output(&mut self) -> Option<ToolOutput> {
        None
    }

    fn cancel(&mut self) {}
}

/// A call whose work is one future: silent until it resolves, then ended.
///
/// If the core answers the call before the future resolves — because
/// something else made a request — the answer says so, and the result arrives
/// later as an update.
pub(crate) struct OneShot {
    result: Arc<Mutex<Option<(ToolOutput, UnixMs)>>>,
    task: tokio::task::JoinHandle<()>,
    waker: SourceWaker,
    answered: bool,
    delivered: bool,
}

impl OneShot {
    pub(crate) fn spawn(
        waker: SourceWaker,
        future: impl Future<Output = ToolOutput> + Send + 'static,
    ) -> Self {
        let result = Arc::new(Mutex::new(None));
        let task = tokio::spawn({
            let result = Arc::clone(&result);
            let waker = waker.clone();
            async move {
                let output = future.await;
                *result.lock().unwrap() = Some((output, UnixMs::now()));
                waker.wake();
            }
        });
        Self {
            result,
            task,
            waker,
            answered: false,
            delivered: false,
        }
    }
}

impl ToolSession for OneShot {
    fn sources(&self) -> Vec<(u64, crate::SourceFacts)> {
        let haste = match &*self.result.lock().unwrap() {
            Some((_, at)) => ToolHaste::Ended { at: *at },
            None => ToolHaste::None,
        };
        vec![(0, crate::SourceFacts::Tool(haste))]
    }

    fn done(&self) -> bool {
        self.delivered
    }

    fn first_output(&mut self) -> ToolOutput {
        self.answered = true;
        match self.result.lock().unwrap().as_ref() {
            Some((output, _)) => {
                self.delivered = true;
                output.clone()
            }
            None => output(
                "Still running; the result will arrive on this call when it is ready.",
                ToolOutputStatus::Success,
            ),
        }
    }

    fn more_output(&mut self) -> Option<ToolOutput> {
        if self.delivered {
            return None;
        }
        let output = self.result.lock().unwrap().as_ref()?.0.clone();
        self.delivered = true;
        Some(output)
    }

    fn cancel(&mut self) {
        let mut result = self.result.lock().unwrap();
        if result.is_none() {
            self.task.abort();
            *result = Some((
                output("Cancelled before it finished.", ToolOutputStatus::Cancelled),
                UnixMs::now(),
            ));
            self.waker.wake();
        }
    }
}

impl Drop for OneShot {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) fn output(text: impl Into<String>, status: ToolOutputStatus) -> ToolOutput {
    ToolOutput {
        output: Arc::new(text.into()),
        full_output: None,
        images: Arc::new(Vec::new()),
        status,
    }
}

/// Lines that mean "look now" whatever else the command goes on to print: a
/// crash, a test verdict, a server that is up, a prompt waiting for a person.
/// Everything else a running command says can wait for its end or the model's
/// next look-in.
pub(crate) fn stands_on_its_own(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "panicked at",
        "error[E",
        "error:",
        "FAILED",
        "test result:",
        "Listening on",
        "listening on",
        "Local:",
        "ready in",
        "Password:",
        "password:",
        "[Y/n]",
        "[y/N]",
        "(y/n)",
        "(yes/no)",
    ];
    MARKERS.iter().any(|marker| text.contains(marker))
}
