use rho_agent::inference::{CacheKey, Call, Carry, Image, Item, is_retryable};
use serde_json::{Value, json};

use super::wire;
use crate::config::{InferenceModel, ReasoningEffort};

fn body(items: &[Item]) -> Value {
    wire::body(
        InferenceModel::Hydrogen,
        ReasoningEffort::Xhigh,
        "aicode-consumers",
        "be brief",
        items,
        CacheKey::from_u128(7),
    )
}

fn contents(items: &[Item]) -> Vec<Value> {
    body(items)["request"]["contents"]
        .as_array()
        .unwrap()
        .clone()
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.into(),
        images: vec![],
    }
}

fn report(text: &str, reply_to: &Carry) -> Item {
    Item::Report {
        text: text.into(),
        images: vec![],
        reply_to: reply_to.clone(),
    }
}

fn step(carry: &Carry) -> Item {
    Item::Step {
        carry: carry.clone(),
        exec: None,
    }
}

/// The carry Gemini's answer leaves, through the same parse a live answer
/// takes.
fn answer(parts: Value) -> rho_agent::inference::Step {
    let body = json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 40,
                          "candidatesTokenCount": 7, "thoughtsTokenCount": 30},
        "responseId": "resp-1",
    }});
    wire::step(body.to_string().as_bytes(), false).unwrap()
}

fn signed_call(id: &str, code: &str) -> Value {
    json!({"thoughtSignature": format!("sig-{id}"),
           "functionCall": {"name": "exec", "args": {"code": code}, "id": id}})
}

/// A step's parts are replayed exactly as they came, signature included, and
/// the report answers its call by id, with its images beside it.
#[test]
fn a_call_replays_signed_and_its_report_answers_it_by_id() {
    let first = answer(json!([signed_call("call-1", "print(2**100)")]));
    assert_eq!(first.call.as_ref().unwrap().code, "print(2**100)");
    assert_eq!(first.call.as_ref().unwrap().display_id(), "call-1");
    let items = [
        user("compute"),
        step(&first.carry),
        Item::Report {
            text: "1267650600228229401496703205376".into(),
            images: vec![Image {
                media_type: "image/png".into(),
                data: vec![1, 2, 3],
            }],
            reply_to: first.carry.clone(),
        },
    ];
    let contents = contents(&items);
    assert_eq!(
        contents,
        vec![
            json!({"role": "user", "parts": [{"text": "compute"}]}),
            json!({"role": "model", "parts": [signed_call("call-1", "print(2**100)")]}),
            json!({"role": "user", "parts": [
                {"functionResponse": {"name": "exec", "id": "call-1",
                                      "response": {"output": "1267650600228229401496703205376"}}},
                {"inlineData": {"mimeType": "image/png", "data": "AQID"}},
            ]}),
        ]
    );
    let request = &body(&items)["request"];
    assert_eq!(
        request["toolConfig"]["functionCallingConfig"]["mode"],
        "AUTO"
    );
    // Gemini has no `xhigh`: it refuses the request.
    assert_eq!(
        request["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    assert_eq!(request["sessionId"], CacheKey::from_u128(7).to_string());
    assert_eq!(
        body(&items)["userPromptId"],
        CacheKey::from_u128(7).to_string()
    );
    assert_eq!(body(&items)["model"], "gemini-4-hydrogen-sonar");
}

/// Gemini may call several times in one answer. Only the first is kept, in
/// the carry as in the step, so that the one report pairs with it.
#[test]
fn only_the_first_of_parallel_calls_is_kept() {
    let step = answer(json!([
        {"text": "thinking out loud", "thought": true},
        {"text": "Let me look. "},
        signed_call("a", "first()"),
        {"functionCall": {"name": "exec", "args": {"code": "second()"}, "id": "b"}},
    ]));
    assert_eq!(step.call.as_ref().unwrap().code, "first()");
    assert_eq!(step.prose, "Let me look. ");
    assert_eq!(step.carry.display_calls().len(), 1);
    let replayed = contents(&[
        user("go"),
        Item::Step {
            carry: step.carry,
            exec: None,
        },
    ]);
    let parts = replayed[1]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 3);
    assert!(!replayed[1].to_string().contains("second()"));
    // Thoughts count as output: they were generated and billed.
    assert_eq!(step.usage.input_tokens, 100);
    assert_eq!(step.usage.cached_tokens, 40);
    assert_eq!(step.usage.output_tokens, 37);
}

/// A call left unanswered still gets a response before the conversation
/// goes on, or Gemini refuses the request.
#[test]
fn an_unanswered_call_is_closed_before_the_next_user_turn() {
    let first = answer(json!([signed_call("c", "sleep(9)")]));
    let contents = contents(&[user("go"), step(&first.carry), user("stop that")]);
    assert_eq!(contents.len(), 3);
    assert_eq!(
        contents[2]["parts"],
        json!([
            {"functionResponse": {"name": "exec", "id": "c", "response": {"output": "(no output)"}}},
            {"text": "stop that"},
        ])
    );
}

/// A compaction asks for a summary with tools off; its carry then replaces
/// everything before it, and roles still alternate after it.
#[test]
fn a_compaction_summary_replaces_the_history_before_it() {
    let first = answer(json!([signed_call("c", "work()")]));
    let before = [
        user("the task"),
        step(&first.carry),
        report("done", &first.carry),
        Item::CompactionTrigger,
    ];
    let request = body(&before);
    assert_eq!(
        request["request"]["toolConfig"]["functionCallingConfig"]["mode"],
        "NONE"
    );
    let asked = request["request"]["contents"].as_array().unwrap();
    let last = asked.last().unwrap();
    assert_eq!(last["role"], "user");
    assert!(last["parts"][0].get("functionResponse").is_some());
    assert!(
        last["parts"][1]["text"]
            .as_str()
            .unwrap()
            .contains("summary")
    );

    let summary = json!({"response": {"candidates": [{"content": {"parts": [
        {"text": "Task: X. Done: work(). Next: verify."}]}, "finishReason": "STOP"}]}});
    let compacted = wire::step(summary.to_string().as_bytes(), true).unwrap();
    assert!(compacted.carry.has_compaction());
    assert!(compacted.call.is_none());
    assert!(compacted.prose.is_empty());

    let after = contents(&[
        user("the task"),
        step(&first.carry),
        report("done", &first.carry),
        step(&compacted.carry),
        user("continue"),
    ]);
    assert_eq!(after.len(), 1, "{after:#?}");
    assert_eq!(after[0]["role"], "user");
    let text = after[0]["parts"][0]["text"].as_str().unwrap();
    assert!(text.ends_with("Task: X. Done: work(). Next: verify."));
    assert_eq!(after[0]["parts"][1]["text"], "continue");
    assert!(!json!(after).to_string().contains("the task"));
}

/// An empty summary would erase the context: it is retried instead.
#[test]
fn an_empty_summary_is_retried() {
    let empty = json!({"response": {"candidates": [{"content": {"parts": [{"text": " "}]},
                                                   "finishReason": "STOP"}]}});
    let error = wire::step(empty.to_string().as_bytes(), true).unwrap_err();
    assert!(is_retryable(&error));
}

/// After the agent switches from an OpenAI model, its earlier calls replay
/// from their code, unsigned, and their reports answer them without an id.
#[test]
fn another_models_steps_replay_from_their_code() {
    let openai = Carry::new(
        json!({"items": [{"type": "custom_tool_call", "call_id": "call_x", "input": "print(1)"}]}),
        vec![Call::new("call_x", "print(1)".into())],
        false,
    );
    let contents = contents(&[user("hi"), step(&openai), report("1", &openai)]);
    assert_eq!(
        contents[1],
        json!({"role": "model", "parts": [{
            "functionCall": {"name": "exec", "args": {"code": "print(1)"}},
            "thoughtSignature": "skip_thought_signature_validator",
        }]})
    );
    assert_eq!(
        contents[2]["parts"][0],
        json!({"functionResponse": {"name": "exec", "response": {"output": "1"}}})
    );
}

/// Output whose call no longer exists is still delivered, marked as exec
/// output rather than the user's words.
#[test]
fn a_report_without_its_call_is_marked_as_exec_output() {
    let summary = json!({"response": {"candidates": [{"content": {"parts": [{"text": "s"}]},
                                                     "finishReason": "STOP"}]}});
    let compacted = wire::step(summary.to_string().as_bytes(), true).unwrap();
    let first = answer(json!([signed_call("c", "work()")]));
    let contents = contents(&[step(&compacted.carry), report("late output", &first.carry)]);
    assert_eq!(contents.len(), 1);
    assert_eq!(
        contents[0]["parts"][1]["text"],
        "Exec output, not from the user:\nlate output"
    );
}

/// A broken or foreign call is the model's slip: asking again usually
/// fixes it. A refusal for content is final.
#[test]
fn broken_calls_retry_and_refusals_do_not() {
    let malformed =
        json!({"response": {"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL"}]}});
    assert!(is_retryable(
        &wire::step(malformed.to_string().as_bytes(), false).unwrap_err()
    ));
    let other = json!({"response": {"candidates": [{"content": {"parts": [
        {"functionCall": {"name": "run_shell", "args": {"cmd": "ls"}}}]}, "finishReason": "STOP"}]}});
    assert!(is_retryable(
        &wire::step(other.to_string().as_bytes(), false).unwrap_err()
    ));
    let safety = json!({"response": {"candidates": [{"finishReason": "SAFETY"}]}});
    assert!(!is_retryable(
        &wire::step(safety.to_string().as_bytes(), false).unwrap_err()
    ));
}

#[test]
fn rate_limits_and_outages_retry_and_bad_requests_do_not() {
    let body = br#"{"error":{"code":429,"message":"Resource has been exhausted","status":"RESOURCE_EXHAUSTED"}}"#;
    let limited = super::refusal(reqwest::StatusCode::TOO_MANY_REQUESTS, body);
    assert!(is_retryable(&limited));
    assert!(limited.to_string().contains("Resource has been exhausted"));
    assert!(is_retryable(&super::refusal(
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        b"upstream"
    )));
    let bad = super::refusal(
        reqwest::StatusCode::BAD_REQUEST,
        br#"{"error":{"message":"Thinking level is unsupported"}}"#,
    );
    assert!(!is_retryable(&bad));
    assert!(bad.to_string().contains("Thinking level is unsupported"));
}

/// The whole path against Cloud Code: the host's login, a call, its report,
/// an answer, a compaction, and an answer from the summary alone.
#[tokio::test]
#[ignore = "manual: needs an Antigravity login at ~/.local/state/rho/auth.d/antigravity/default.json"]
async fn live_hydrogen_calls_answers_and_compacts() {
    use rho_agent::inference::{Backend, Event, Request, Step};

    use crate::config::InferenceProfile;

    async fn complete(session: &rho_agent::inference::InferenceSession, request: Request) -> Step {
        let mut events = session.start(request);
        match events.recv().await.expect("an event") {
            Event::Completed(step) => step,
            Event::Failed(error) => panic!("{error:#}"),
            other => panic!("{other:?}"),
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let host = crate::Accounts::new_with_config(
        rho_db::RhoDb::open(temp.path().join("rho.redb")),
        crate::InferenceConfig::with_responses_base_url("http://127.0.0.1:9").unwrap(),
    )
    .await
    .unwrap();
    // The trait's, not the OpenAI transport's inherent `session`.
    let session = Backend::session(
        &host.client(),
        InferenceProfile {
            effort: ReasoningEffort::Low,
            fast_mode: false,
        },
        InferenceModel::Hydrogen,
    );
    let instructions: std::sync::Arc<str> = "You are a test agent. You act by calling exec with \
        Python. Once a cell's output gives the answer, reply in plain text and do not call exec."
        .into();
    let key = CacheKey::new();
    let request = |items: &[Item]| Request::new(instructions.clone(), items.to_vec(), key);

    let mut items = vec![user(
        "Use exec to compute 2**100, then tell me its digit sum.",
    )];
    let first = complete(&session, request(&items)).await;
    let call = first.call.clone().expect("a call");
    eprintln!("call: {:?}\nusage: {:?}", call.code, first.usage);
    assert!(call.code.contains("100"), "{}", call.code);
    items.push(step(&first.carry));
    items.push(report(
        "1267650600228229401496703205376\n115\n",
        &first.carry,
    ));

    let second = complete(&session, request(&items)).await;
    eprintln!("answer: {:?}", second.prose);
    assert!(second.call.is_none(), "{:?}", second.call);
    assert!(second.prose.contains("115"), "{}", second.prose);
    items.push(step(&second.carry));

    items.push(Item::CompactionTrigger);
    let compacted = complete(&session, request(&items)).await;
    assert!(compacted.carry.has_compaction());
    eprintln!("summary: {}", compacted.carry.data().get());
    items.pop();
    items.push(step(&compacted.carry));
    items.push(user(
        "What was the digit sum? Answer in plain text, without exec.",
    ));
    let after = complete(&session, request(&items)).await;
    eprintln!("after: {:?}", after.prose);
    assert!(after.prose.contains("115"), "{}", after.prose);
}
