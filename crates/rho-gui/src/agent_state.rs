use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy)]
pub(crate) struct AgentContextUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) percent_used: Option<u8>,
    pub(crate) context_window: Option<u64>,
}

#[derive(Default)]
pub(crate) struct AgentState {
    current_agent_id: Option<String>,
    known_agents: HashSet<String>,
    live_agents: HashSet<String>,
    context_usage: HashMap<String, AgentContextUsage>,
    query_agents: HashMap<String, String>,
    prompt_agents: HashMap<String, String>,
    tool_agents: HashMap<String, String>,
    shell_agents: HashMap<String, String>,
    running_agents: HashSet<String>,
}

impl AgentState {
    pub(crate) fn current_agent_id(&self) -> Option<&str> {
        self.current_agent_id.as_deref()
    }

    pub(crate) fn current_agent_id_owned(&self) -> Option<String> {
        self.current_agent_id.clone()
    }

    pub(crate) fn clear_current_agent(&mut self) {
        self.current_agent_id = None;
    }

    pub(crate) fn remember(&mut self, agent_id: impl Into<String>) {
        self.known_agents.insert(agent_id.into());
    }

    pub(crate) fn mark_live(&mut self, agent_id: impl Into<String>) {
        let agent_id = agent_id.into();
        self.known_agents.insert(agent_id.clone());
        self.live_agents.insert(agent_id);
    }

    pub(crate) fn select(&mut self, agent_id: impl Into<String>) {
        let agent_id = agent_id.into();
        self.known_agents.insert(agent_id.clone());
        self.live_agents.insert(agent_id.clone());
        if self.current_agent_id.as_deref() != Some(agent_id.as_str()) {
            self.current_agent_id = Some(agent_id);
        }
    }

    pub(crate) fn unload(&mut self, agent_id: &str) {
        self.live_agents.remove(agent_id);
        if self.current_agent_id.as_deref() == Some(agent_id) {
            self.current_agent_id = None;
        }
    }

    pub(crate) fn known(&self, agent_id: &str) -> bool {
        self.known_agents.contains(agent_id)
    }

    pub(crate) fn selected_is_active(&self) -> bool {
        let Some(agent_id) = self.current_agent_id.as_deref() else {
            return true;
        };
        self.live_agents.contains(agent_id)
    }

    #[cfg(test)]
    pub(crate) fn running(&self, agent_id: &str) -> bool {
        self.running_agents.contains(agent_id)
    }

    pub(crate) fn known_agents_sorted(&self) -> Vec<String> {
        let mut known_agents = self.known_agents.iter().cloned().collect::<Vec<_>>();
        known_agents.sort();
        known_agents
    }

    pub(crate) fn completion_snapshot(&self) -> (Vec<String>, HashSet<String>) {
        (self.known_agents_sorted(), self.live_agents.clone())
    }

    pub(crate) fn next_active_agent(&self, delta: isize) -> Option<String> {
        let active_agents = self
            .known_agents_sorted()
            .into_iter()
            .filter(|agent| self.live_agents.contains(agent))
            .collect::<Vec<_>>();
        if active_agents.is_empty() {
            return None;
        }
        let len = active_agents.len() as isize;
        let index = self
            .current_agent_id
            .as_deref()
            .and_then(|current| active_agents.iter().position(|agent| agent == current))
            .map(|index| (index as isize + delta).rem_euclid(len) as usize)
            .unwrap_or_else(|| {
                if delta < 0 {
                    active_agents.len() - 1
                } else {
                    0
                }
            });
        active_agents.get(index).cloned()
    }

    pub(crate) fn record_context_usage(&mut self, agent_id: String, usage: AgentContextUsage) {
        self.context_usage.insert(agent_id, usage);
    }

    pub(crate) fn selected_context_usage(&self) -> Option<AgentContextUsage> {
        self.current_agent_id
            .as_deref()
            .and_then(|agent_id| self.context_usage.get(agent_id).copied())
    }

    pub(crate) fn clear_context_usage(&mut self) {
        self.context_usage.clear();
    }

    pub(crate) fn clear_routing(&mut self) {
        self.query_agents.clear();
        self.prompt_agents.clear();
        self.tool_agents.clear();
        self.shell_agents.clear();
        self.running_agents.clear();
    }

    pub(crate) fn observe_event(&mut self, event: &tau_proto::Event) {
        match event {
            tau_proto::Event::StartAgentAccepted(accepted) => {
                let agent_id = accepted.agent_id.to_string();
                self.query_agents
                    .insert(accepted.query_id.clone(), agent_id.clone());
                self.remember(agent_id);
            }
            tau_proto::Event::AgentStarted(started) => self.remember(started.agent_id.to_string()),
            tau_proto::Event::AgentLoaded(loaded) => self.remember(loaded.agent_id.to_string()),
            tau_proto::Event::AgentUnloaded(unloaded) => {
                self.unload(unloaded.agent_id.as_str());
                self.remove_agent_routes(unloaded.agent_id.as_str());
            }
            tau_proto::Event::UiPromptSubmitted(prompt) => {
                let agent_id = prompt.agent_id.to_string();
                if prompt.originator.is_user() {
                    self.select(agent_id.clone());
                } else {
                    self.remember(agent_id.clone());
                }
                self.record_originator_agent(&prompt.originator, agent_id);
            }
            tau_proto::Event::AgentPromptSubmitted(prompt) => {
                let agent_id = prompt.agent_id.to_string();
                if prompt.originator.is_user() && !prompt.message_class.is_internal() {
                    self.select(agent_id.clone());
                } else {
                    self.remember(agent_id.clone());
                }
                self.record_originator_agent(&prompt.originator, agent_id);
            }
            tau_proto::Event::AgentPromptQueued(queued) if !queued.message_class.is_internal() => {
                self.select(queued.agent_id.to_string());
            }
            tau_proto::Event::AgentUserMessageInjected(injected)
                if !injected.message_class.is_internal() =>
            {
                self.remember(injected.agent_id.to_string());
            }
            tau_proto::Event::UiShellCommand(command) => {
                if let Some(agent_id) = command.target_agent_id.as_deref() {
                    self.remember(agent_id.to_owned());
                    self.shell_agents
                        .insert(command.command_id.to_string(), agent_id.to_owned());
                }
            }
            tau_proto::Event::ShellCommandProgress(progress) => {
                if let Some(agent_id) = progress.target_agent_id.as_deref() {
                    self.remember(agent_id.to_owned());
                    self.shell_agents
                        .insert(progress.command_id.to_string(), agent_id.to_owned());
                }
            }
            tau_proto::Event::ShellCommandFinished(finished) => {
                if let Some(agent_id) = finished.target_agent_id.as_deref() {
                    self.remember(agent_id.to_owned());
                    self.shell_agents
                        .insert(finished.command_id.to_string(), agent_id.to_owned());
                }
            }
            tau_proto::Event::AgentMessageSent(message) => {
                self.remember(message.sender_id.to_string());
                if let Some(agent_id) = agent_message_sent_recipient_agent_id(message) {
                    self.remember(agent_id.to_owned());
                }
            }
            tau_proto::Event::AgentMessageReceived(message) => {
                self.remember(message.sender_id.to_string());
                self.remember(message.recipient_id.to_string());
            }
            tau_proto::Event::ToolDelegateProgress(progress) => {
                if let Some(agent_id) = &progress.agent_id {
                    self.mark_live(agent_id.clone());
                }
            }
            tau_proto::Event::AgentPromptCreated(created) => {
                let agent_id = created.agent_id.to_string();
                self.prompt_agents
                    .insert(created.agent_prompt_id.to_string(), agent_id.clone());
                if created.originator.is_user() {
                    self.select(agent_id.clone());
                } else {
                    self.remember(agent_id.clone());
                }
                self.record_originator_agent(&created.originator, agent_id);
            }
            tau_proto::Event::AgentPromptTerminated(terminated) => {
                self.prompt_agents.insert(
                    terminated.agent_prompt_id.to_string(),
                    terminated.agent_id.to_string(),
                );
            }
            tau_proto::Event::AgentState(state) => {
                let agent_id = state.agent_id.to_string();
                self.remember(agent_id.clone());
                match state.state {
                    tau_proto::AgentRuntimeState::Idle => {
                        self.running_agents.remove(agent_id.as_str());
                    }
                    tau_proto::AgentRuntimeState::Running => {
                        self.running_agents.insert(agent_id);
                    }
                }
            }
            tau_proto::Event::ProviderResponseFinished(finished) => {
                let agent_id = finished.agent_id.to_string();
                self.prompt_agents
                    .insert(finished.agent_prompt_id.to_string(), agent_id.clone());
                let tool_calls = tool_calls_from_output_items(&finished.output_items);
                for call in tool_calls {
                    self.tool_agents
                        .insert(call.call_id.to_string(), agent_id.clone());
                }
                if finished.originator.is_user() {
                    self.select(agent_id.clone());
                } else {
                    self.remember(agent_id.clone());
                }
                self.record_originator_agent(&finished.originator, agent_id);
            }
            tau_proto::Event::HarnessAgentContextUsageChanged(changed) => {
                self.remember(changed.agent_id.to_string());
            }
            _ => {}
        }
    }

    pub(crate) fn agent_id_for_event(&self, event: &tau_proto::Event) -> Option<String> {
        match event {
            tau_proto::Event::ToolRequest(request) => {
                self.tool_agents.get(request.call_id.as_str()).cloned()
            }
            tau_proto::Event::ToolStarted(started) => {
                self.tool_agents.get(started.call_id.as_str()).cloned()
            }
            tau_proto::Event::ToolProgress(progress) => {
                self.tool_agents.get(progress.call_id.as_str()).cloned()
            }
            tau_proto::Event::ToolDelegateProgress(progress) => {
                self.tool_agents.get(progress.call_id.as_str()).cloned()
            }
            tau_proto::Event::ToolResult(result) | tau_proto::Event::ProviderToolResult(result) => {
                self.tool_agents
                    .get(result.call_id.as_str())
                    .cloned()
                    .or_else(|| self.agent_id_for_originator(&result.originator))
            }
            tau_proto::Event::ToolError(error) | tau_proto::Event::ProviderToolError(error) => self
                .tool_agents
                .get(error.call_id.as_str())
                .cloned()
                .or_else(|| self.agent_id_for_originator(&error.originator)),
            tau_proto::Event::ToolBackgroundResult(result) => {
                self.tool_agents.get(result.call_id.as_str()).cloned()
            }
            tau_proto::Event::ToolBackgroundError(error) => {
                self.tool_agents.get(error.call_id.as_str()).cloned()
            }
            tau_proto::Event::ToolCancelled(cancelled) => {
                self.tool_agents.get(cancelled.call_id.as_str()).cloned()
            }
            tau_proto::Event::UiPromptSubmitted(prompt) => Some(prompt.agent_id.to_string()),
            tau_proto::Event::AgentPromptSubmitted(prompt) => Some(prompt.agent_id.to_string()),
            tau_proto::Event::AgentPromptQueued(queued) => Some(queued.agent_id.to_string()),
            tau_proto::Event::AgentPromptRecalled(recalled) => Some(recalled.agent_id.to_string()),
            tau_proto::Event::AgentPromptSteered(steered) => Some(steered.agent_id.to_string()),
            tau_proto::Event::AgentCompactionTriggered(triggered) => {
                Some(triggered.agent_id.to_string())
            }
            tau_proto::Event::AgentPromptCreated(created) => Some(created.agent_id.to_string()),
            tau_proto::Event::AgentPromptTerminated(terminated) => self
                .prompt_agents
                .get(terminated.agent_prompt_id.as_str())
                .cloned()
                .or_else(|| self.agent_id_for_originator(&terminated.originator)),
            tau_proto::Event::ProviderPromptSubmitted(submitted) => self
                .prompt_agents
                .get(submitted.agent_prompt_id.as_str())
                .cloned()
                .or_else(|| self.agent_id_for_originator(&submitted.originator)),
            tau_proto::Event::ProviderResponseUpdated(update) => self
                .prompt_agents
                .get(update.agent_prompt_id.as_str())
                .cloned()
                .or_else(|| self.agent_id_for_originator(&update.originator)),
            tau_proto::Event::ProviderResponseFinished(finished) => {
                Some(finished.agent_id.to_string())
            }
            tau_proto::Event::AgentState(state) => Some(state.agent_id.to_string()),
            tau_proto::Event::HarnessAgentContextUsageChanged(changed) => {
                Some(changed.agent_id.to_string())
            }
            tau_proto::Event::ExtensionContextReady(ready) => Some(ready.agent_id.to_string()),
            tau_proto::Event::UiCancelPrompt(cancel) => {
                cancel.target_agent_id.as_ref().map(ToString::to_string)
            }
            tau_proto::Event::UiRecallQueuedPrompt(recall) => {
                recall.target_agent_id.as_ref().map(ToString::to_string)
            }
            tau_proto::Event::UiShellCommand(command) => {
                command.target_agent_id.as_ref().map(ToString::to_string)
            }
            tau_proto::Event::ShellCommandProgress(progress) => progress
                .target_agent_id
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| self.shell_agents.get(progress.command_id.as_str()).cloned()),
            tau_proto::Event::ShellCommandFinished(finished) => finished
                .target_agent_id
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| self.shell_agents.get(finished.command_id.as_str()).cloned()),
            tau_proto::Event::AgentMessageSent(message) => Some(message.sender_id.to_string()),
            tau_proto::Event::AgentMessageReceived(message) => {
                Some(message.recipient_id.to_string())
            }
            _ => self.current_agent_id.clone(),
        }
    }

    fn record_originator_agent(
        &mut self,
        originator: &tau_proto::PromptOriginator,
        agent_id: String,
    ) {
        if let tau_proto::PromptOriginator::Extension { query_id, .. } = originator {
            self.query_agents.insert(query_id.clone(), agent_id);
        }
    }

    fn agent_id_for_originator(&self, originator: &tau_proto::PromptOriginator) -> Option<String> {
        match originator {
            tau_proto::PromptOriginator::User => self.current_agent_id.clone(),
            tau_proto::PromptOriginator::Extension { query_id, .. } => {
                self.query_agents.get(query_id).cloned()
            }
        }
    }

    fn remove_agent_routes(&mut self, agent_id: &str) {
        self.query_agents.retain(|_, value| value != agent_id);
        self.prompt_agents.retain(|_, value| value != agent_id);
        self.tool_agents.retain(|_, value| value != agent_id);
        self.shell_agents.retain(|_, value| value != agent_id);
        self.running_agents.remove(agent_id);
    }
}
fn tool_calls_from_output_items(
    output_items: &[tau_proto::ContextItem],
) -> Vec<&tau_proto::ToolCallItem> {
    output_items
        .iter()
        .filter_map(|item| match item {
            tau_proto::ContextItem::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn agent_message_sent_recipient_agent_id(message: &tau_proto::AgentMessageSent) -> Option<&str> {
    match &message.recipient {
        tau_proto::AgentMessageRecipient::Agent { agent_id } => Some(agent_id.as_str()),
        tau_proto::AgentMessageRecipient::User => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_id(value: &str) -> tau_proto::AgentId {
        tau_proto::AgentId::parse(value).expect("valid agent id")
    }

    #[test]
    fn routes_shell_progress_and_finished_by_command_id() {
        let mut state = AgentState::default();
        state.observe_event(&tau_proto::Event::UiShellCommand(
            tau_proto::UiShellCommand {
                command_id: tau_proto::ShellCommandId::from("command"),
                command: "echo hi".to_owned(),
                include_in_context: true,
                target_agent_id: Some(agent_id("agent")),
            },
        ));

        let progress = tau_proto::Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
            command_id: tau_proto::ShellCommandId::from("command"),
            stream: tau_proto::ShellStream::Stdout,
            chunk: "hi".to_owned(),
            target_agent_id: None,
        });
        assert_eq!(
            state.agent_id_for_event(&progress).as_deref(),
            Some("agent")
        );

        let finished = tau_proto::Event::ShellCommandFinished(tau_proto::ShellCommandFinished {
            command_id: tau_proto::ShellCommandId::from("command"),
            command: "echo hi".to_owned(),
            include_in_context: true,
            target_agent_id: None,
            output: "hi".to_owned(),
            exit_code: Some(0),
            cancelled: false,
        });
        assert_eq!(
            state.agent_id_for_event(&finished).as_deref(),
            Some("agent")
        );
    }

    #[test]
    fn routes_tool_events_from_provider_output_items() {
        let mut state = AgentState::default();
        state.observe_event(&tau_proto::Event::ProviderResponseFinished(
            tau_proto::ProviderResponseFinished {
                agent_prompt_id: tau_proto::AgentPromptId::from("prompt"),
                agent_id: agent_id("agent"),
                output_items: vec![tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
                    call_id: tau_proto::ToolCallId::from("call"),
                    name: tau_proto::ToolName::new("tool"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: tau_proto::CborValue::Null,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            },
        ));

        let progress = tau_proto::Event::ToolProgress(tau_proto::ToolProgress {
            call_id: tau_proto::ToolCallId::from("call"),
            tool_name: tau_proto::ToolName::new("tool"),
            message: Some("running".to_owned()),
            progress: None,
            display: None,
        });
        assert_eq!(
            state.agent_id_for_event(&progress).as_deref(),
            Some("agent")
        );
    }
    #[test]
    fn tracks_running_agents_from_agent_state_events() {
        let mut state = AgentState::default();

        state.observe_event(&tau_proto::Event::AgentState(
            tau_proto::AgentStateChanged {
                agent_id: agent_id("agent"),
                state: tau_proto::AgentRuntimeState::Running,
            },
        ));
        assert!(state.running("agent"));
        assert!(state.known("agent"));

        state.observe_event(&tau_proto::Event::AgentState(
            tau_proto::AgentStateChanged {
                agent_id: agent_id("agent"),
                state: tau_proto::AgentRuntimeState::Idle,
            },
        ));
        assert!(!state.running("agent"));
    }

    #[test]
    fn next_active_agent_wraps_live_agents() {
        let mut state = AgentState::default();
        state.mark_live("helper");
        state.mark_live("worker");
        state.unload("helper");

        assert_eq!(state.next_active_agent(1).as_deref(), Some("worker"));
        state.select("worker");
        assert_eq!(state.next_active_agent(1).as_deref(), Some("worker"));
        assert_eq!(state.next_active_agent(-1).as_deref(), Some("worker"));
    }
}
