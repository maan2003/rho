//! A switch's handoff, from rows shaped like the ones each runtime writes.

use rho_agent_types::transcript::ExecId;
use rho_agent_types::{AgentId, AgentIdDomain, ContentPart, Role, UnixMs, Wpos};
use serde_json::{Value, json};

use super::*;
use crate::TranscriptCall;
use crate::inference::config::{InferenceModel, InferenceProfile};
use crate::inference::{Call, Carry, PromptCacheKey};
use crate::record::{AgentSpec, CallResult, Event, Input, Record, Runtime};

pub(crate) fn agent() -> AgentId {
    AgentId::from_counter(3, &AgentIdDomain(7)).unwrap()
}

fn claude() -> AgentSpec {
    AgentSpec {
        runtime: Runtime::Claude {
            model: rho_claude::Model::Opus,
            effort: rho_claude::Effort::High,
            session_id: uuid::Uuid::nil(),
        },
        props: Default::default(),
    }
}

fn native() -> AgentSpec {
    AgentSpec {
        runtime: Runtime::Native {
            model: InferenceModel::Astra,
            profile: InferenceProfile::default(),
            cache_key: PromptCacheKey::generate(),
        },
        props: Default::default(),
    }
}

fn text(text: &str) -> Vec<ContentPart> {
    vec![ContentPart::Text { text: text.into() }]
}

fn input(role: Role, said: &str) -> Input {
    Input {
        role,
        message: None,
        content: text(said),
    }
}

fn request(output: &str, inputs: Vec<Input>) -> Event<'static> {
    Event::Request {
        agent: agent(),
        output: if output.is_empty() {
            Vec::new()
        } else {
            text(output)
        },
        inputs,
        notices: Vec::new(),
        compact: false,
        turn: Some(uuid::Uuid::nil()),
        timing: None,
    }
}

fn reply(said: &str, calls: &[(&str, &str, Value)]) -> Event<'static> {
    Event::ClaudeReply {
        agent: agent(),
        text: said.into(),
        calls: calls
            .iter()
            .map(|(id, name, arguments)| TranscriptCall {
                id: (*id).into(),
                name: (*name).into(),
                arguments: arguments.to_string(),
            })
            .collect(),
        usage: None,
        context_used: Some(90_000),
    }
}

pub(crate) fn rows(events: Vec<Event<'static>>) -> Vec<(Wpos, Record<'static>)> {
    events
        .into_iter()
        .enumerate()
        .map(|(at, event)| {
            (
                Wpos(at as u64 + 10),
                Record {
                    at: UnixMs(1),
                    event,
                },
            )
        })
        .collect()
}

/// A Claude agent's rows the way Claude Code leaves them: one row per block
/// (thinking makes none), a compaction, a call of Claude Code's own, and a
/// turn that ends in words.
pub(crate) fn claude_era() -> Vec<Event<'static>> {
    vec![
        Event::AgentCreated {
            agent: agent(),
            spec: claude(),
        },
        request("", vec![input(Role::User, "long ago")]),
        reply("Done long ago.", &[]),
        request("", vec![input(Role::User, "Count the rows.")]),
        reply(
            "",
            &[("toolu_old", "mcp__py__exec", json!({"source": "old()"}))],
        ),
        // Claude Code compacts in the middle of the turn, and keeps it.
        Event::ClaudeCompacted {
            agent: agent(),
            context_used: None,
        },
        request("old output", vec![]),
        reply("Looking.", &[]),
        reply(
            "",
            &[("toolu_a", "mcp__py__exec", json!({"source": "print(6*7)"}))],
        ),
        request(
            "42",
            vec![input(Role::Tool, "[message from eng-x]\nalso this")],
        ),
        reply("", &[("toolu_b", "ToolSearch", json!({"query": "slack"}))]),
        Event::ClaudeResults {
            agent: agent(),
            results: vec![CallResult {
                call: ExecId::try_from("toolu_b").unwrap(),
                output: "no tools found".into(),
            }],
        },
        reply("There are 42.", &[]),
        request("", vec![input(Role::User, "Thanks. Next?")]),
    ]
}

pub(crate) fn transcribe(prose: &str, call: Option<Call>) -> Carry {
    Carry::new(
        json!({"prose": prose, "call": call.as_ref().map(Call::display_id)}),
        call.into_iter().collect(),
        false,
    )
}

fn texts(content: &[ContentPart]) -> String {
    crate::entry::plain_text(content)
}

/// What each converted row says, in a form a test can compare whole.
fn shape(events: &[Event<'_>]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            Event::Request { output, inputs, .. } => format!(
                "request out={:?} in={:?}",
                texts(output),
                inputs
                    .iter()
                    .map(|input| format!("{:?}:{}", input.role, texts(&input.content)))
                    .collect::<Vec<_>>()
            ),
            Event::Step {
                prose, exec, carry, ..
            } => format!(
                "step prose={prose:?} exec={exec:?} call={:?}",
                carry
                    .display_calls()
                    .first()
                    .map(|call| call.display_id().to_owned())
            ),
            other => format!("{other:?}"),
        })
        .collect()
}

/// The turns since Claude's compaction become native rows, one step per
/// reply, each request kept with its roles; the summary opens them, and
/// nothing from before the compaction is replayed.
#[test]
fn claude_rows_since_the_compaction_become_native_steps_and_requests() {
    let rows = rows(claude_era());
    let out = claude_to_native(
        agent(),
        era(&rows),
        Some("SUMMARY: the user wants rows counted."),
        "Claude Code (Opus)",
        "gpt-6.2-astra in Rho's native runtime",
        HANDOFF_BYTES,
        &transcribe,
    );
    let said = shape(&out);
    assert!(
        said[0].starts_with("request out=\"\" in=[\"System:This agent ran on Claude Code (Opus)"),
        "{said:#?}"
    );
    // The framing is Rho's, the summary is Claude's and sits in the user's
    // seat, as Claude Code puts it.
    assert!(
        said[0].ends_with("\", \"User:SUMMARY: the user wants rows counted.\"]"),
        "{said:#?}"
    );
    assert_eq!(said[0].matches("SUMMARY").count(), 1, "{said:#?}");
    assert!(said[0].contains("summary of the conversation before its last compaction"));
    assert!(!said[0].contains("left out"));
    assert_eq!(
        said[1..],
        [
            "request out=\"\" in=[\"User:Count the rows.\"]",
            "step prose=\"\" exec=Some(\"old()\") call=Some(\"toolu_old\")",
            "request out=\"old output\" in=[]",
            "step prose=\"Looking.\" exec=Some(\"print(6*7)\") call=Some(\"toolu_a\")",
            "request out=\"42\" in=[\"Tool:[message from eng-x]\\nalso this\"]",
            "step prose=\"[I called Claude Code's `ToolSearch` tool with {\\\"query\\\":\\\"slack\\\"}]\\n\\n\
             [`ToolSearch` returned: no tools found]\\n\\nThere are 42.\" exec=None call=None",
            "request out=\"\" in=[\"User:Thanks. Next?\"]",
        ],
        "{said:#?}"
    );
    let all = format!("{out:?}");
    assert!(!all.contains("long ago"));
}

/// Without a summary the history says only that the turns follow.
#[test]
fn a_session_that_never_compacted_replays_from_the_start() {
    let mut events = claude_era();
    events.retain(|event| !matches!(event, Event::ClaudeCompacted { .. }));
    let rows = rows(events);
    let out = claude_to_native(
        agent(),
        era(&rows),
        None,
        "A",
        "B",
        HANDOFF_BYTES,
        &transcribe,
    );
    let said = shape(&out);
    assert!(said[0].contains("The turns follow"), "{said:#?}");
    assert!(
        !said[0].contains("User:"),
        "no summary, no user input: {said:#?}"
    );
    assert_eq!(said[1], "request out=\"\" in=[\"User:long ago\"]");
    assert_eq!(said[2], "step prose=\"Done long ago.\" exec=None call=None");
}

/// Over budget, whole turns go from the oldest end: a call is never parted
/// from its answer, and the opening says how many went.
#[test]
fn over_budget_the_oldest_whole_turns_are_left_out() {
    let big = "x".repeat(30_000);
    let rows = rows(vec![
        Event::AgentCreated {
            agent: agent(),
            spec: claude(),
        },
        request("", vec![input(Role::User, "first")]),
        reply("", &[("toolu_1", "exec", json!({"source": big}))]),
        request(&big, vec![]),
        reply("done one", &[]),
        request("", vec![input(Role::User, "second")]),
        reply("", &[("toolu_2", "exec", json!({"source": "small()"}))]),
        request(&"y".repeat(20_000), vec![]),
        reply("done two", &[]),
        request("", vec![input(Role::User, "third")]),
    ]);
    // Room for the second and third turns, not the 60 KB first one.
    let out = claude_to_native(agent(), era(&rows), None, "A", "B", 25_000, &transcribe);
    let said = shape(&out);
    assert!(
        said[0].contains("except the oldest turn, left out"),
        "{said:#?}"
    );
    assert_eq!(said[1], "request out=\"\" in=[\"User:second\"]");
    assert_eq!(said.len(), 6, "{said:#?}");
    // Not even the newest turn fits: it is kept anyway.
    let out = claude_to_native(agent(), era(&rows), None, "A", "B", 10, &transcribe);
    let said = shape(&out);
    assert!(said[0].contains("except the 2 oldest turns"), "{said:#?}");
    assert_eq!(said[1..], ["request out=\"\" in=[\"User:third\"]"]);
}

/// One huge output is cut from its middle, keeping both ends.
#[test]
fn a_huge_output_is_cut_in_the_middle() {
    let output = format!("HEAD{}TAIL", "z".repeat(100_000));
    let rows = rows(vec![
        request("", vec![input(Role::User, "go")]),
        reply("", &[("toolu_1", "exec", json!({"source": "go()"}))]),
        request(&output, vec![]),
    ]);
    let out = claude_to_native(agent(), &rows, None, "A", "B", HANDOFF_BYTES, &transcribe);
    let Event::Request { output, .. } = &out[3] else {
        panic!("{out:#?}")
    };
    let output = texts(output);
    assert!(output.starts_with("HEAD") && output.ends_with("TAIL"));
    assert!(output.contains("bytes cut by the runtime switch"));
    assert!(output.len() < 41_000);
}

/// Only the rows since the latest change of kind are the current runtime's.
#[test]
fn the_era_starts_after_the_latest_change_of_kind() {
    let rows = rows(vec![
        Event::AgentCreated {
            agent: agent(),
            spec: claude(),
        },
        request("", vec![input(Role::User, "claude one")]),
        Event::AgentChanged {
            agent: agent(),
            spec: claude(),
        },
        request("", vec![input(Role::User, "claude two")]),
        Event::AgentChanged {
            agent: agent(),
            spec: native(),
        },
        request("", vec![input(Role::User, "native")]),
    ]);
    let era = era(&rows);
    assert_eq!(era.len(), 1);
    assert_eq!(era[0].0, Wpos(15));
    // A change within a kind starts nothing.
    assert_eq!(super::era(&rows[..4]).len(), 4);
}

fn step(prose: &str, exec: Option<&str>, id: &str) -> Event<'static> {
    Event::Step {
        agent: agent(),
        exec: exec.map(str::to_owned),
        prose: prose.into(),
        carry: transcribe(prose, exec.map(|code| Call::new(id, code.into()))),
        usage: None,
    }
}

fn native_era() -> Vec<Event<'static>> {
    vec![
        Event::AgentCreated {
            agent: agent(),
            spec: native(),
        },
        request("", vec![input(Role::User, "hi")]),
        step("ok", Some("print(1)"), "call_1"),
        request("1", vec![input(Role::Tool, "[message from eng-x]\nping")]),
        // An encrypted compaction, which nothing else can read.
        Event::Step {
            agent: agent(),
            exec: None,
            prose: String::new(),
            carry: Carry::new(json!("encrypted"), vec![], true),
            usage: None,
        },
        request("", vec![input(Role::System, "kept skill")]),
        step("all done", None, ""),
        request("", vec![input(Role::User, "later")]),
    ]
}

/// A native agent's rows become Claude messages: the open call's answer a
/// `tool_result` holding output and tool messages, people's words text, each
/// step its prose and a `tool_use` of Claude's exec name. The encrypted
/// compaction is dropped, and the rows it summarized are kept.
#[test]
fn native_rows_become_a_claude_conversation() {
    let rows = rows(native_era());
    let messages = native_to_claude(
        era(&rows),
        "gpt-6.2-astra in Rho's native runtime",
        "Claude Code (Opus)",
        "mcp__py__exec",
        HANDOFF_BYTES,
    );
    let opening = messages[0]["content"][0]["text"].as_str().unwrap();
    assert!(opening.starts_with("This agent ran on gpt-6.2-astra"));
    assert!(!opening.contains("left out"));
    assert_eq!(
        messages,
        vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": opening},
                {"type": "text", "text": "hi"},
            ]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "ok"},
                {"type": "tool_use", "id": "toolu_rho_switch_1", "name": "mcp__py__exec",
                 "input": {"source": "print(1)"}},
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_rho_switch_1", "content": [
                    {"type": "text", "text": "1\n\n[message from eng-x]\nping"},
                ]},
                {"type": "text", "text": "kept skill"},
            ]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "all done"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "later"}]}),
        ]
    );
}

/// A call the switch cut off is answered, since Claude takes no call
/// without its result.
#[test]
fn a_call_open_at_the_switch_is_answered() {
    let rows = rows(vec![
        request("", vec![input(Role::User, "go")]),
        step("", Some("sleep()"), "call_9"),
    ]);
    let messages = native_to_claude(&rows, "A", "B", "exec", HANDOFF_BYTES);
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"][0]["type"], "tool_result");
    assert_eq!(last["content"][0]["tool_use_id"], "toolu_rho_switch_1");
    assert!(
        last["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cut this call off")
    );
}

/// Over budget, Claude's history starts at a person's turn, never at a
/// tool result.
#[test]
fn over_budget_claude_history_starts_at_a_turn() {
    let big = "b".repeat(5_000);
    let rows = rows(vec![
        request("", vec![input(Role::User, "first")]),
        step("", Some("one()"), "c1"),
        request(&big, vec![]),
        step("one done", None, ""),
        request("", vec![input(Role::User, "second")]),
        step("", Some("two()"), "c2"),
        request("2", vec![]),
        step("two done", None, ""),
    ]);
    let messages = native_to_claude(&rows, "A", "B", "exec", 1_000);
    let opening = messages[0]["content"][0]["text"].as_str().unwrap();
    assert!(
        opening.contains("except the oldest turn, left out"),
        "{opening}"
    );
    assert_eq!(messages[0]["content"][1]["text"], "second");
    assert_eq!(messages.len(), 4, "{messages:#?}");
    assert_eq!(messages[1]["content"][0]["id"], "toolu_rho_switch_2");
    assert_eq!(
        messages[2]["content"][0]["tool_use_id"],
        "toolu_rho_switch_2"
    );
}
