//! Cloud Code's `generateContent`, as Antigravity calls it: a Gemini request
//! wrapped with the account's project, and a Gemini response wrapped in
//! `response`.
//!
//! A step's [`Carry`] holds the model's parts as they came, thought
//! signatures included: Gemini rejects a replayed call without its
//! signature. A compaction's carry holds the summary that replaces the
//! history before it.

use rho_agent::inference::{CacheKey, Call, Carry, Image, Item, Retryable, Step, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{InferenceModel, ReasoningEffort};
use crate::step::EXEC;

/// Antigravity's own signature for a call it did not sign, documented for
/// calls replayed from another model.
const UNSIGNED: &str = "skip_thought_signature_validator";

const SUMMARY_HEAD: &str = "Your context was compacted. This is the summary you wrote of the \
work before it:\n\n";
const FOREIGN_COMPACTION: &str =
    "Your context was compacted by another model. What came before it is not available.";
const UNPAIRED: &str = "Exec output, not from the user:\n";
const NO_OUTPUT: &str = "(no output)";
const COMPACT: &str = "Write a concise handoff for the next context window. It will have this \
summary instead of the conversation above. Do not call tools or continue the task. Return only \
the summary, with these headings:

- Task and constraints: The user's requested outcome, success criteria, scope, and commitments. \
Separate the user's requirements from your assumptions.
- Progress and evidence: What changed, what you verified, and what remains unverified. Keep the \
file paths, IDs, commands, and results needed to resume. Include failed approaches only if they \
affect the next step.
- Current state: Work in flight, relevant session or job IDs, decisions and their reasons, and \
blockers or unanswered questions.
- Next steps: The remaining actions in order, including checks and cleanup. If the task is \
complete, say so.";

#[derive(Serialize, Deserialize)]
struct Replay {
    gemini: Turn,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Turn {
    /// The model's content parts, verbatim.
    Parts(Vec<Value>),
    /// What replaces the history before a compaction.
    Summary(String),
}

/// What a carry holds, if this provider wrote it. Another provider's carry
/// (the agent changed models) is replayed from its display calls.
fn turn(carry: &Carry) -> Option<Turn> {
    serde_json::from_str::<Replay>(carry.data().get())
        .ok()
        .map(|replay| replay.gemini)
}

/// The Gemini call a carry made, by its id, which Gemini may leave out.
/// `None` when the carry made no call.
fn call_of(carry: &Carry) -> Option<Option<String>> {
    match turn(carry) {
        Some(Turn::Parts(parts)) => parts.iter().find_map(|part| {
            part.get("functionCall")
                .map(|call| call.get("id").and_then(Value::as_str).map(str::to_owned))
        }),
        Some(Turn::Summary(_)) => None,
        None => carry.has_call().then_some(None),
    }
}

fn thinking_level(effort: ReasoningEffort) -> &'static str {
    // Hydrogen uses the standard thinking config, which has no `xhigh`.
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High | ReasoningEffort::Xhigh => "high",
    }
}

/// Contents with each turn's parts gathered under its role: Gemini wants
/// the roles to alternate.
#[derive(Default)]
struct Contents {
    contents: Vec<Value>,
    /// The id of the model's call that has no response yet.
    open: Option<Option<String>>,
}

impl Contents {
    fn push(&mut self, role: &str, parts: Vec<Value>) {
        if parts.is_empty() {
            return;
        }
        if role == "model" {
            self.close();
        }
        if let Some(last) = self.contents.last_mut()
            && last["role"] == role
        {
            last["parts"].as_array_mut().expect("parts").extend(parts);
        } else {
            self.contents.push(json!({"role": role, "parts": parts}));
        }
    }

    /// Answer the open call, if any: Gemini requires a response to every
    /// call before the conversation goes on.
    fn respond(&mut self, id: Option<String>, output: &str, images: &[Image]) {
        self.open = None;
        let mut response = json!({"name": EXEC, "response": {"output": output}});
        if let Some(id) = id {
            response["id"] = id.into();
        }
        let mut parts = vec![json!({"functionResponse": response})];
        parts.extend(images.iter().map(inline));
        self.push("user", parts);
    }

    fn close(&mut self) {
        if let Some(id) = self.open.take() {
            self.respond(id, NO_OUTPUT, &[]);
        }
    }

    fn user(&mut self, text: String, images: &[Image]) {
        self.close();
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(json!({"text": text}));
        }
        parts.extend(images.iter().map(inline));
        self.push("user", parts);
    }

    fn step(&mut self, carry: &Carry) {
        match turn(carry) {
            Some(Turn::Parts(parts)) => {
                let open = call_of(carry);
                self.push("model", parts);
                self.open = open;
            }
            Some(Turn::Summary(summary)) => self.user(format!("{SUMMARY_HEAD}{summary}"), &[]),
            None if carry.has_compaction() => self.user(FOREIGN_COMPACTION.into(), &[]),
            None => {
                // Another model's step keeps only its call's code.
                if let Some(call) = carry.display_calls().first() {
                    self.push(
                        "model",
                        vec![json!({
                            "functionCall": {"name": EXEC, "args": {"code": call.code}},
                            "thoughtSignature": UNSIGNED,
                        })],
                    );
                    self.open = Some(None);
                }
            }
        }
    }

    fn report(&mut self, text: &str, images: &[Image], reply_to: &Carry) {
        match (call_of(reply_to), self.open.is_some()) {
            (Some(id), true) => self.respond(id, text, images),
            // The call this answers was compacted away.
            _ => self.user(format!("{UNPAIRED}{text}"), images),
        }
    }
}

fn inline(image: &Image) -> Value {
    use base64::Engine as _;
    json!({"inlineData": {
        "mimeType": image.media_type,
        "data": base64::engine::general_purpose::STANDARD.encode(&image.data),
    }})
}

/// Whether this request asks for a compaction rather than a step.
pub(super) fn compacting(items: &[Item]) -> bool {
    items
        .iter()
        .any(|item| matches!(item, Item::CompactionTrigger))
}

/// The Cloud Code body for one step, or for a compaction.
pub(super) fn body(
    model: InferenceModel,
    effort: ReasoningEffort,
    project: &str,
    instructions: &str,
    items: &[Item],
    cache_key: CacheKey,
) -> Value {
    let start = items
        .iter()
        .rposition(|item| matches!(item, Item::Step { carry, .. } if carry.has_compaction()))
        .unwrap_or(0);
    let mut contents = Contents::default();
    for item in &items[start..] {
        match item {
            Item::Step { carry, .. } => contents.step(carry),
            Item::Report {
                text,
                images,
                reply_to,
            } => contents.report(text, images, reply_to),
            Item::User { text, images } => contents.user(text.clone(), images),
            Item::CompactionTrigger => {}
        }
    }
    let compacting = compacting(items);
    if compacting {
        contents.user(COMPACT.into(), &[]);
    }
    contents.close();
    let (model_id, thinking_config) = if model == InferenceModel::Neon {
        // Antigravity sends both the catalog ID and the level. On its JSON
        // wire, Extra High is a numeric enum value, not a string.
        let level = match effort {
            ReasoningEffort::Low => json!("LOW"),
            ReasoningEffort::Medium => json!("MEDIUM"),
            ReasoningEffort::High => json!("HIGH"),
            ReasoningEffort::Xhigh => json!(100),
        };
        (
            format!("{}-{}", model.as_str(), effort.as_str()),
            json!({"includeThoughts": true, "thinkingLevel": level}),
        )
    } else {
        (
            model.as_str().to_owned(),
            json!({"thinkingLevel": thinking_level(effort)}),
        )
    };
    json!({
        "model": model_id,
        "project": project,
        "requestId": format!("agent-{}", uuid::Uuid::new_v4()),
        "userAgent": "antigravity",
        "requestType": "agent",
        // Antigravity's client sends it too. Whether it helps the cache is
        // not known.
        "userPromptId": cache_key.to_string(),
        "request": {
            "contents": contents.contents,
            "systemInstruction": {"parts": [{"text": instructions}]},
            "tools": [{"functionDeclarations": [{
                "name": EXEC,
                "description": "Run Python in your notebook. Every response is one call to this tool.",
                "parameters": {
                    "type": "object",
                    "properties": {"code": {"type": "string", "description": "The Python cell to run."}},
                    "required": ["code"],
                },
            }]}],
            "toolConfig": {"functionCallingConfig": {"mode": if compacting { "NONE" } else { "AUTO" }}},
            // Cloud Code caches a prompt prefix per session.
            "sessionId": cache_key.to_string(),
            "generationConfig": {"thinkingConfig": thinking_config},
        },
    })
}

#[derive(Deserialize)]
struct Envelope {
    response: Response,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default)]
    usage_metadata: UsageMetadata,
    #[serde(default)]
    prompt_feedback: Option<Value>,
    #[serde(default)]
    response_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    #[serde(default)]
    content: Option<Content>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    parts: Vec<Value>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    #[serde(default)]
    prompt_token_count: u64,
    #[serde(default)]
    cached_content_token_count: u64,
    #[serde(default)]
    candidates_token_count: u64,
    #[serde(default)]
    thoughts_token_count: u64,
}

/// The step a Cloud Code response body holds.
pub(super) fn step(body: &[u8], compacting: bool) -> anyhow::Result<Step> {
    let Envelope { response } = serde_json::from_slice(body)?;
    let usage = Usage {
        input_tokens: response.usage_metadata.prompt_token_count,
        cached_tokens: response.usage_metadata.cached_content_token_count,
        output_tokens: response.usage_metadata.candidates_token_count
            + response.usage_metadata.thoughts_token_count,
    };
    let Some(candidate) = response.candidates.into_iter().next() else {
        anyhow::bail!(
            "Gemini answered nothing: {}",
            response.prompt_feedback.unwrap_or_default()
        );
    };
    let finish = candidate.finish_reason.unwrap_or_default();
    if matches!(
        finish.as_str(),
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL"
    ) {
        return Err(Retryable(format!("Gemini wrote a broken call ({finish})")).into());
    }
    let mut parts = candidate
        .content
        .map(|content| content.parts)
        .unwrap_or_default();
    // One call per step: drop the rest, and their replies with them.
    let mut called = false;
    parts.retain(|part| {
        let call = part.get("functionCall").is_some();
        let keep = !(call && called);
        called |= call;
        keep
    });
    if parts.is_empty() && !matches!(finish.as_str(), "STOP" | "MAX_TOKENS") {
        anyhow::bail!("Gemini stopped without an answer ({finish})");
    }
    let prose: String = parts
        .iter()
        .filter(|part| part.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    if compacting {
        if prose.trim().is_empty() {
            return Err(Retryable("Gemini wrote an empty summary".into()).into());
        }
        return Ok(Step {
            continuation: None,
            call: None,
            prose: String::new(),
            carry: Carry::new(
                Replay {
                    gemini: Turn::Summary(prose),
                },
                vec![],
                true,
            ),
            usage,
        });
    }
    let call = match parts.iter().find_map(|part| part.get("functionCall")) {
        None => None,
        Some(call) => {
            let name = call.get("name").and_then(Value::as_str);
            let code = call.pointer("/args/code").and_then(Value::as_str);
            let (Some(EXEC), Some(code)) = (name, code) else {
                return Err(Retryable(format!("Gemini called {call} instead of exec")).into());
            };
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(response.response_id)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            Some(Call::new(id, code.to_owned()))
        }
    };
    Ok(Step {
        continuation: None,
        carry: Carry::new(
            Replay {
                gemini: Turn::Parts(parts),
            },
            call.iter().cloned().collect(),
            false,
        ),
        call,
        prose,
        usage,
    })
}
