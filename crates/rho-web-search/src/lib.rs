//! Client-executed ChatGPT web search, compatible with Codex's `web.run` tool.

mod schema;
mod search;

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use rho_core::{
    ContentPart, ContextBlock, InferenceResponseItem, ToolCall, ToolExecutionContext, ToolName,
    ToolOutput, ToolOutputStatus, ToolSpec, ToolType,
};
use rho_inference::InferenceAuth;

use crate::schema::commands_schema;
use crate::search::{
    AllowedCaller, ContentItem, ExternalWebAccess, MessagePhase, ResponseItem, SearchCommands,
    SearchInput, SearchRequest, SearchResponse, SearchSettings,
};

pub const WEB_SEARCH_TOOL_NAME: &str = "web__run";
const SEARCH_URL: &str = "https://chatgpt.com/backend-api/codex/alpha/search";
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const ASSISTANT_CONTEXT_TOKENS: usize = 1_000;
const APPROX_CHARS_PER_TOKEN: u64 = 4;

#[derive(Clone)]
pub struct WebSearchTools {
    auth: InferenceAuth,
    session_id: Arc<str>,
    client: reqwest::Client,
    search_url: Arc<str>,
}

impl WebSearchTools {
    pub fn new(auth: InferenceAuth, session_id: impl Into<Arc<str>>) -> Self {
        Self {
            auth,
            session_id: session_id.into(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("static web search HTTP client configuration is valid"),
            search_url: SEARCH_URL.into(),
        }
    }

    pub fn spec(&self) -> ToolSpec {
        web_search_spec()
    }

    pub fn call(
        &self,
        call: ToolCall,
        context: ToolExecutionContext,
    ) -> BoxFuture<'static, ToolOutput> {
        let tools = self.clone();
        Box::pin(async move {
            match tools.call_inner(call, context).await {
                Ok(output) => ToolOutput {
                    output: Arc::new(output),
                    status: ToolOutputStatus::Success,
                },
                Err(error) => ToolOutput {
                    output: Arc::new(error),
                    status: ToolOutputStatus::Error,
                },
            }
        })
    }

    async fn call_inner(
        &self,
        call: ToolCall,
        context: ToolExecutionContext,
    ) -> Result<String, String> {
        let commands = if call.arguments.trim().is_empty() {
            SearchCommands::default()
        } else {
            serde_json::from_str::<SearchCommands>(&call.arguments)
                .map_err(|error| format!("invalid web search arguments: {error}"))?
        };

        let auth = self.auth.clone();
        let auth = tokio::task::spawn_blocking(move || auth.resolve_oauth())
            .await
            .map_err(|_| "OAuth credential resolution task failed".to_owned())?
            .map_err(|error| format!("resolving ChatGPT OAuth credentials: {error}"))?;
        let request = SearchRequest {
            id: self.session_id.to_string(),
            model: context.model.to_string(),
            reasoning: None,
            input: recent_input(&context.input).map(SearchInput::Items),
            commands: Some(commands),
            settings: Some(SearchSettings {
                allowed_callers: Some(vec![AllowedCaller::Direct]),
                external_web_access: Some(ExternalWebAccess::Boolean(true)),
                ..Default::default()
            }),
            max_output_tokens: context.max_output_tokens,
        };
        let mut builder = self
            .client
            .post(self.search_url.as_ref())
            .bearer_auth(auth.bearer_token)
            .json(&request);
        if let Some(account_id) = auth.account_id {
            builder = builder.header("ChatGPT-Account-ID", account_id);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|error| format!("calling ChatGPT web search: {error}"))?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("reading ChatGPT web search response: {error}"))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err("ChatGPT web search response exceeded 4 MiB".to_owned());
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&body)
                .chars()
                .take(4_000)
                .collect::<String>();
            return Err(format!("ChatGPT web search returned {status}: {detail}"));
        }
        let response: SearchResponse = serde_json::from_slice(&body)
            .map_err(|error| format!("decoding ChatGPT web search response: {error}"))?;
        let max_chars = context
            .max_output_tokens
            .unwrap_or(10_000)
            .saturating_mul(APPROX_CHARS_PER_TOKEN)
            .min(usize::MAX as u64) as usize;
        Ok(truncate_output(response.output, max_chars))
    }
}

pub fn web_search_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(WEB_SEARCH_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: DESCRIPTION.to_owned(),
        input_schema: commands_schema(),
        format: None,
    }
}

fn recent_input(blocks: &[Arc<ContextBlock>]) -> Option<Vec<ResponseItem>> {
    let mut messages = Vec::new();
    for block in blocks {
        match block.as_ref() {
            ContextBlock::UserMessage { content, .. } => {
                push_message(&mut messages, "user", content, None)
            }
            ContextBlock::InferenceResponse { items, .. } => {
                for item in items {
                    if let InferenceResponseItem::AssistantMessage { content, phase, .. } = item {
                        push_message(
                            &mut messages,
                            "assistant",
                            content,
                            phase.map(|phase| match phase {
                                rho_core::MessagePhase::Commentary => MessagePhase::Commentary,
                                rho_core::MessagePhase::FinalAnswer => MessagePhase::FinalAnswer,
                            }),
                        );
                    }
                }
            }
            ContextBlock::ToolResults { .. }
            | ContextBlock::ToolUpdate(_)
            | ContextBlock::CompactionTrigger => {}
        }
    }

    let latest_user = messages.iter().rposition(
        |message| matches!(message, ResponseItem::Message { role, .. } if role == "user"),
    )?;
    messages.truncate(latest_user + 1);
    let start = messages
        .iter()
        .enumerate()
        .rev()
        .filter(
            |(_, message)| matches!(message, ResponseItem::Message { role, .. } if role == "user"),
        )
        .take(2)
        .last()
        .map(|(index, _)| index)
        .unwrap_or(latest_user);
    messages.drain(..start);

    let mut remaining_tokens = ASSISTANT_CONTEXT_TOKENS;
    messages.retain_mut(|message| {
        let ResponseItem::Message { role, content, .. } = message;
        if role != "assistant" {
            return true;
        }
        content.retain_mut(|item| {
            let ContentItem::OutputText { text } = item else {
                return true;
            };
            if remaining_tokens == 0 {
                return false;
            }
            let tokens = text.len().saturating_add(3) / 4;
            if tokens <= remaining_tokens {
                remaining_tokens -= tokens;
            } else {
                *text = truncate_middle_tokens(text, remaining_tokens);
                remaining_tokens = 0;
            }
            true
        });
        !content.is_empty()
    });
    (!messages.is_empty()).then_some(messages)
}

fn push_message(
    messages: &mut Vec<ResponseItem>,
    role: &str,
    content: &[ContentPart],
    phase: Option<MessagePhase>,
) {
    let content = content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } if role == "assistant" => {
                ContentItem::OutputText { text: text.clone() }
            }
            ContentPart::Text { text } => ContentItem::InputText { text: text.clone() },
        })
        .collect::<Vec<_>>();
    if !content.is_empty() {
        messages.push(ResponseItem::Message {
            id: None,
            role: role.to_owned(),
            content,
            phase,
            internal_chat_message_metadata_passthrough: None,
        });
    }
}

fn truncate_middle_tokens(text: &str, max_tokens: usize) -> String {
    let max_bytes = max_tokens.saturating_mul(4);
    if max_tokens > 0 && text.len() <= max_bytes {
        return text.to_owned();
    }
    let left_budget = max_bytes / 2;
    let right_budget = max_bytes - left_budget;
    let prefix_end = text
        .char_indices()
        .take_while(|(index, ch)| index + ch.len_utf8() <= left_budget)
        .map(|(index, ch)| index + ch.len_utf8())
        .last()
        .unwrap_or(0);
    let target = text.len().saturating_sub(right_budget);
    let suffix_start = text
        .char_indices()
        .find_map(|(index, _)| (index >= target).then_some(index))
        .unwrap_or(text.len())
        .max(prefix_end);
    let removed_tokens = text.len().saturating_sub(max_bytes).saturating_add(3) / 4;
    format!(
        "{}…{removed_tokens} tokens truncated…{}",
        &text[..prefix_end],
        &text[suffix_start..]
    )
}

fn truncate_output(output: String, max_chars: usize) -> String {
    if output.chars().count() <= max_chars {
        return output;
    }
    let mut output = output.chars().take(max_chars).collect::<String>();
    output.push_str("\n[web output truncated]");
    output
}

const DESCRIPTION: &str = r#"Tool for accessing the internet. Supports search_query, image_query, open, click, find, screenshot, finance, weather, sports, and time commands. Batch related operations in one call; search_query accepts at most four queries. Use returned reference IDs only in later calls to this tool. Cite final-answer sources with normal Markdown links from the returned results, not internal reference IDs."#;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn recent_input_keeps_two_user_turns_and_caps_assistant_text() {
        fn user(text: &str) -> Arc<ContextBlock> {
            Arc::new(ContextBlock::UserMessage {
                sender: rho_core::MessageSender::User,
                content: vec![ContentPart::Text { text: text.into() }],
            })
        }
        fn assistant(text: &str) -> Arc<ContextBlock> {
            Arc::new(ContextBlock::InferenceResponse {
                items: vec![InferenceResponseItem::AssistantMessage {
                    provider_specific: Box::new(rho_core::UnknownProviderSpecificData {
                        tag: "test".to_owned(),
                    }),
                    content: vec![ContentPart::Text { text: text.into() }],
                    phase: None,
                }],
                provider_response_id: None,
            })
        }
        let input = recent_input(&[
            user("old"),
            assistant("old answer"),
            user("previous"),
            assistant(&"a".repeat(5_000)),
            user("current"),
        ])
        .unwrap();
        assert_eq!(input.len(), 3);
        fn text(item: &ResponseItem) -> &str {
            match item {
                ResponseItem::Message { content, .. } => match &content[0] {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => text,
                },
            }
        }
        assert_eq!(text(&input[0]), "previous");
        assert!(text(&input[1]).contains("tokens truncated"));
        assert_eq!(text(&input[2]), "current");
    }

    #[test]
    fn schema_exposes_codex_commands() {
        let schema = commands_schema();
        for command in [
            "search_query",
            "image_query",
            "open",
            "click",
            "find",
            "screenshot",
            "finance",
            "weather",
            "sports",
            "time",
            "response_length",
        ] {
            assert!(
                schema["properties"].get(command).is_some(),
                "missing {command}"
            );
        }
        assert_eq!(
            schema["properties"]["search_query"]["items"]["properties"]["recency"],
            json!({
                "description": "Whether to filter by recency, as a number of recent days.",
                "type": "integer"
            })
        );
    }

    #[test]
    fn production_url_matches_codex_provider_path_join() {
        assert_eq!(
            SEARCH_URL,
            "https://chatgpt.com/backend-api/codex/alpha/search"
        );
    }

    #[test]
    fn request_matches_codex_search_shape() {
        let input = vec![ResponseItem::Message {
            id: None,
            role: "user".to_owned(),
            content: vec![ContentItem::InputText {
                text: "latest question".to_owned(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];
        let request = SearchRequest {
            id: "agent-session".to_owned(),
            model: "gpt-test".to_owned(),
            reasoning: None,
            input: Some(SearchInput::Items(input)),
            commands: Some(
                serde_json::from_value(json!({
                    "search_query": [{"q": "rho"}]
                }))
                .unwrap(),
            ),
            settings: Some(SearchSettings {
                allowed_callers: Some(vec![AllowedCaller::Direct]),
                external_web_access: Some(ExternalWebAccess::Boolean(true)),
                ..Default::default()
            }),
            max_output_tokens: Some(10_000),
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "id": "agent-session",
                "model": "gpt-test",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "latest question"}]
                }],
                "commands": {"search_query": [{"q": "rho"}]},
                "settings": {
                    "allowed_callers": ["direct"],
                    "external_web_access": true
                },
                "max_output_tokens": 10_000
            })
        );
    }

    #[test]
    fn output_truncation_respects_unicode_boundaries() {
        assert_eq!(
            truncate_output("aé日z".to_owned(), 3),
            "aé日\n[web output truncated]"
        );
    }
}
