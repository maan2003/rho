//! One model step at a time, for an agent whose every response is code.
//!
//! The caller owns history and supplies full or incremental [`Request`] input.
//! This crate owns the warm connection, not the context window. Each request
//! receives a [`Step`]. The model answers only
//! by calling `exec` with a cell of Python. Anything else it writes is kept as
//! `prose`, which the agent does not deliver anywhere.
//!
//! What only the provider understands (item ids, encrypted reasoning) rides
//! in a [`Carry`]: stored by the agent, replayed verbatim, never read.

use std::sync::Arc;

use rho_agent::inference::{CacheKey, Call, Carry, Response, Step};
pub(crate) use rho_agent::inference::{Event, Retryable};
use serde_json::value::RawValue;
mod adapter;

mod openai;
pub use openai::InferenceSession;

pub fn is_retryable(error: &anyhow::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    if error.is::<Retryable>() || error.is::<tokio::time::error::Elapsed>() {
        return true;
    }
    if let Some(error) = error.downcast_ref::<Error>() {
        return match error {
            Error::Io(_)
            | Error::Tls(_)
            | Error::ConnectionClosed
            | Error::AlreadyClosed
            | Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
            Error::Http(response) => {
                response.status().is_server_error() || response.status().as_u16() == 408
            }
            _ => false,
        };
    }
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::BrokenPipe
        )
    })
}

/// The name of the one tool.
pub const EXEC: &str = "exec";

/// A full active context, or only the suffix after `previous_response_id`.
/// The caller owns history. A rejected continuation requires database replay.
#[derive(Clone, Debug)]
pub struct Request {
    pub instructions: Arc<str>,
    pub(crate) items: Vec<Item>,
    pub cache_key: CacheKey,
    pub previous_response_id: Option<String>,
}

impl Request {
    #[cfg(test)]
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn new(instructions: Arc<str>, items: Vec<Item>, cache_key: CacheKey) -> Self {
        let start = items
            .iter()
            .rposition(|item| matches!(item, Item::Step(carry) if carry.has_compaction()))
            .unwrap_or(0);
        Self {
            instructions,
            items: items.into_iter().skip(start).collect(),
            cache_key,
            previous_response_id: None,
        }
    }

    pub fn continuation(
        instructions: Arc<str>,
        items: Vec<Item>,
        cache_key: CacheKey,
        previous_response_id: String,
    ) -> Self {
        Self {
            instructions,
            items,
            cache_key,
            previous_response_id: Some(previous_response_id),
        }
    }
}

/// Scope an agent cache identity to its endpoint and credential.
fn wire_uuid(key: CacheKey, base_url: &str, client_secret: [u8; 32]) -> uuid::Uuid {
    use std::hash::Hasher;
    let mut bytes = [0; 16];
    for (part, tag) in bytes
        .chunks_mut(8)
        .zip([b"rho-step-cache:v1:0", b"rho-step-cache:v1:1"])
    {
        let mut hash = fnv::FnvHasher::default();
        for input in [
            &tag[..],
            &key.0.to_le_bytes(),
            base_url.as_bytes(),
            &client_secret,
        ] {
            hash.write(input);
        }
        part.copy_from_slice(&hash.finish().to_be_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes)
}

#[derive(Clone, Debug)]
pub enum Item {
    /// One of the model's earlier responses, replayed as it came.
    Step(Carry),
    /// Ask the provider to compact its context on the next response.
    CompactionTrigger,
    /// What an earlier step's `exec` call produced.
    Result(CallResult),
    /// Anything else the model is told: messages, and a report on a step
    /// that made no call.
    User { text: String, images: Vec<Image> },
}

pub use rho_agent::inference::{Image, Usage};

/// An opaque provider pairing with an execution's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallResult {
    id: String,
    function: bool,
    pub text: String,
    pub images: Vec<Image>,
}
impl CallResult {
    pub fn display_id(&self) -> &str {
        self.id.as_str()
    }
}

/// Replay wrapper metadata surrounds each verbatim provider JSON item.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Replay<'a> {
    #[serde(default, borrow)]
    items: Vec<&'a RawValue>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    single_exec: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pending_exec: bool,
}
#[derive(serde::Deserialize)]
struct ItemMeta<'a> {
    #[serde(rename = "type", borrow)]
    kind: std::borrow::Cow<'a, str>,
    #[serde(default, borrow)]
    call_id: Option<std::borrow::Cow<'a, str>>,
}
struct Prepared<'a> {
    items: Vec<&'a RawValue>,
    compaction: Option<usize>,
    calls: Vec<(String, bool)>,
}
fn replay(carry: &Carry) -> Replay<'_> {
    serde_json::from_str(carry.data().get()).expect("provider replay record")
}
fn prepared(carry: &Carry) -> Prepared<'_> {
    let replay = replay(carry);
    let mut prepared = Prepared {
        items: Vec::new(),
        compaction: None,
        calls: Vec::new(),
    };
    let mut called = false;
    for item in replay.items {
        let meta: ItemMeta<'_> = serde_json::from_str(item.get()).expect("provider item metadata");
        let call = matches!(meta.kind.as_ref(), "custom_tool_call" | "function_call");
        if call && replay.single_exec && called {
            continue;
        }
        called |= call;
        if meta.kind == "compaction" {
            prepared.compaction = Some(prepared.items.len());
            prepared.calls.clear();
        }
        if call && let Some(id) = meta.call_id {
            prepared
                .calls
                .push((id.into(), meta.kind == "function_call"));
        }
        prepared.items.push(item);
    }
    prepared
}
fn from_raw_items(items: Vec<Box<RawValue>>, single_exec: bool) -> Carry {
    let mut calls = Vec::new();
    let mut compacted = false;
    for item in &items {
        let meta: ItemMeta<'_> = serde_json::from_str(item.get()).expect("completed item metadata");
        compacted |= meta.kind == "compaction";
        if matches!(meta.kind.as_ref(), "custom_tool_call" | "function_call")
            && (!single_exec || calls.is_empty())
            && let Some(id) = meta.call_id
        {
            #[derive(serde::Deserialize)]
            struct Code {
                input: Option<String>,
                arguments: Option<String>,
            }
            let code: Code = serde_json::from_str(item.get()).expect("completed call");
            calls.push(rho_agent::inference::Call::new(
                id,
                code.input.or(code.arguments).unwrap_or_default(),
            ));
        }
    }
    Carry::new(
        Replay {
            items: items.iter().map(|item| &**item).collect(),
            single_exec,
            ..Default::default()
        },
        calls,
        compacted,
    )
}
#[cfg(test)]
fn from_openai_values(items: Vec<serde_json::Value>) -> Carry {
    from_raw_items(
        items
            .iter()
            .map(|item| serde_json::value::to_raw_value(item).unwrap())
            .collect(),
        false,
    )
}
pub(crate) fn display_results(
    carry: &rho_agent::inference::Carry,
    text: &str,
    images: &[Image],
) -> Vec<CallResult> {
    reply(carry, text, images)
}

/// Observations for the account owner. Acknowledgment preserves policy ordering
/// before completing a response or using a fallback route.
pub enum Observation {
    Quota {
        selected: crate::SelectedAuth,
        quota: crate::QuotaUpdate,
        done: tokio::sync::oneshot::Sender<()>,
    },
    RouteFailed {
        selected: crate::SelectedAuth,
        route: crate::DialRoute,
        done: tokio::sync::oneshot::Sender<()>,
    },
}

#[derive(Debug)]
pub struct RateLimited(pub String);
impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RateLimited {}

#[cfg(test)]
fn from_openai_items(items: Vec<String>) -> Carry {
    from_raw_items(
        items
            .into_iter()
            .map(|item| RawValue::from_string(item).unwrap())
            .collect(),
        false,
    )
}
#[cfg(test)]
fn call_ids(carry: &Carry) -> Vec<String> {
    prepared(carry)
        .calls
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}
#[cfg(test)]
fn has_call(carry: &Carry) -> bool {
    !call_ids(carry).is_empty()
}
#[cfg(test)]
fn import_response(response: Carry, calls: Vec<Call>) -> Carry {
    Carry::new(response.data(), calls, response.has_compaction())
}
fn reply(carry: &Carry, text: &str, images: &[Image]) -> Vec<CallResult> {
    prepared(carry)
        .calls
        .into_iter()
        .map(|(id, function)| CallResult {
            id,
            function,
            text: text.to_owned(),
            images: images.to_vec(),
        })
        .collect()
}
#[cfg(test)]
fn with_code(carry: &Carry, code: String) -> Call {
    Call::new(call_ids(carry).last().unwrap().as_str(), code)
}
fn bare(call: Call) -> Carry {
    Carry::new(
        serde_json::json!({
            "items":[{
                "type":"custom_tool_call", "id":format!("ctc_{}",call.display_id()),
                "call_id":call.display_id(), "name":EXEC,"input":call.code
            }],
            "pending_exec":true
        }),
        vec![call],
        false,
    )
}

#[cfg(test)]
mod retry_tests {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::http::Response;

    use super::*;

    #[test]
    fn display_code_does_not_create_a_provider_call() {
        let carry = import_response(
            from_openai_items(vec![]),
            vec![Call::new("evicted", "print('old code')".into())],
        );
        assert!(!has_call(&carry));
        assert!(reply(&carry, "new output", &[]).is_empty());
        assert!(prepared(&carry).items.is_empty());
        assert_eq!(carry.display_calls()[0].code, "print('old code')");
        let bytes = senax_encoder::encode(&carry).unwrap();
        let decoded: Carry = senax_encoder::decode(&mut bytes.as_ref()).unwrap();
        assert_eq!(decoded.display_calls()[0].display_id(), "evicted");
        assert!(!has_call(&decoded));
    }

    #[test]
    fn opaque_stream_carry_pairs_partial_code_and_output() {
        let carry = bare(Call::new("provider-stream-7", String::new()));
        let partial = with_code(&carry, "print('admitted')".into());
        assert_eq!(partial.code, "print('admitted')");
        let persisted = bare(partial);
        let results = reply(
            &persisted,
            "printed",
            &[Image {
                media_type: "image/png".into(),
                data: vec![9, 2, 6],
            }],
        );
        let request = Request::continuation(
            "system".into(),
            results.into_iter().map(Item::Result).collect(),
            CacheKey::from_u128(2),
            "response-previous".into(),
        );
        assert!(matches!(&request.items()[0], Item::Result(result)
            if result.display_id() == "provider-stream-7"
            && result.text == "printed"
            && result.images[0].data == [9, 2, 6]));
        assert_eq!(request.items().len(), 1);

        let compacted = from_openai_items(vec![
            r#"{"type":"compaction","encrypted_content":"summary"}"#.into(),
        ]);
        assert!(reply(&compacted, "no old call", &[]).is_empty());
    }

    #[test]
    fn reply_pairs_only_calls_retained_after_compaction() {
        let first = from_openai_items(vec![
            r#"{"type":"custom_tool_call","call_id":"removed","input":"old()"}"#.into(),
            r#"{"type":"compaction","encrypted_content":"summary"}"#.into(),
            r#"{"type":"custom_tool_call","call_id":"retained-a","input":"a()"}"#.into(),
            r#"{"type":"custom_tool_call","call_id":"retained-b","input":"b()"}"#.into(),
        ]);
        let replies = reply(&first, "multi-call output", &[]);
        assert_eq!(
            replies
                .iter()
                .map(CallResult::display_id)
                .collect::<Vec<_>>(),
            ["retained-a", "retained-b"]
        );
    }

    #[test]
    fn transient_transport_errors_are_distinct_from_auth_and_bad_requests() {
        for status in [400, 401, 403, 404, 422] {
            let error = anyhow::Error::from(Error::Http(Box::new(
                Response::builder().status(status).body(None).unwrap(),
            )))
            .context("handshake");
            assert!(!is_retryable(&error), "{status}");
        }
        for status in [408, 500, 502, 503, 504] {
            let error = anyhow::Error::from(Error::Http(Box::new(
                Response::builder().status(status).body(None).unwrap(),
            )))
            .context("handshake");
            assert!(is_retryable(&error), "{status}");
        }
        assert!(is_retryable(
            &anyhow::Error::from(Retryable("throttled".into())).context("provider")
        ));
        for error in [
            Error::Protocol(
                tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
            ),
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "peer closed connection without sending TLS close_notify",
            )),
            Error::Tls(tokio_tungstenite::tungstenite::error::TlsError::Rustls(
                Box::new(rustls::Error::General("TLS peer closed".into())),
            )),
            Error::ConnectionClosed,
            Error::AlreadyClosed,
        ] {
            assert!(is_retryable(
                &anyhow::Error::from(error).context("transport")
            ));
        }
        assert!(!is_retryable(&anyhow::anyhow!(
            "previous_response_id expired"
        )));
        assert!(!is_retryable(&anyhow::anyhow!("invalid request")));
        assert!(!is_retryable(&anyhow::Error::from(
            serde_json::from_str::<serde_json::Value>("invalid").unwrap_err()
        )));
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn replay_json_is_shared_across_clones() {
        let carry = from_openai_values(vec![serde_json::json!({
            "type":"custom_tool_call","call_id":"c","input":"pass"
        })]);
        let cloned = carry.clone();
        assert!(std::ptr::eq(carry.data(), cloned.data()));
        let bytes = senax_encoder::encode(&carry).unwrap();
        let restored: Carry = senax_encoder::decode(&mut bytes.as_ref()).unwrap();
        assert_eq!(call_ids(&restored), ["c".to_owned()]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(restored.data().get()).unwrap()["items"][0]["input"],
            "pass"
        );
    }

    #[test]
    fn full_request_trims_before_last_compaction() {
        let request = Request::new(
            "instructions".into(),
            vec![
                Item::User {
                    text: "discard".repeat(10000),
                    images: vec![],
                },
                Item::Step(from_openai_items(vec![
                    r#"{"type":"compaction","encrypted_content":"summary"}"#.into(),
                ])),
                Item::User {
                    text: "keep".into(),
                    images: vec![],
                },
            ],
            CacheKey::from_u128(7),
        );
        assert_eq!(request.items.len(), 2);
        assert!(matches!(&request.items[0], Item::Step(carry) if carry.has_compaction()));
    }

    #[test]
    fn wire_cache_identity_is_endpoint_and_credential_scoped() {
        let key = CacheKey::from_u128(19);
        let id = wire_uuid(key, "https://one", [1; 32]);
        assert_eq!(id, wire_uuid(key, "https://one", [1; 32]));
        assert_ne!(id, wire_uuid(key, "https://two", [1; 32]));
        assert_ne!(id, wire_uuid(key, "https://one", [2; 32]));
    }
}
