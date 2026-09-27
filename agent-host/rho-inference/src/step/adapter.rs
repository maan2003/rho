//! Implement the agent's turn contract without exposing provider pairing or
//! policy.
use std::sync::Arc;

use futures::future::BoxFuture;
use rho_agent::inference as agent;

use super::{Carry, Event, Item, Request, Step};

impl agent::Backend for crate::Inference {
    fn session(
        &self,
        profile: agent::InferenceProfile,
        model: agent::InferenceModel,
    ) -> agent::InferenceSession {
        Arc::new(Session {
            inference: self.clone(),
            transport: self.session(profile, model),
        })
    }
    fn text(&self, instructions: Arc<str>, input: String) -> BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(self.text(instructions, input))
    }
    fn web_credentials(&self) -> BoxFuture<'_, anyhow::Result<rho_web_search::Credentials>> {
        Box::pin(async {
            use anyhow::Context as _;
            let selected = self
                .auth()
                .await
                .context("selecting ChatGPT OAuth credentials")?;
            let auth = self
                .resolve_auth(selected)
                .await
                .context("resolving ChatGPT OAuth credentials")?;
            Ok(rho_web_search::Credentials {
                bearer_token: auth.bearer_token,
                account_id: auth.account_id,
            })
        })
    }
}

struct Session {
    inference: crate::Inference,
    transport: super::InferenceSession,
}
impl agent::Session for Session {
    fn start(&self, request: agent::Request) -> agent::Response {
        let (events, response) = tokio::sync::mpsc::unbounded_channel();
        let inference = self.inference.clone();
        let transport = self.transport.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = events.closed() => {},
                _ = async {
                    let (selected, auth) = match inference.select_resolved().await {
                        Ok(auth) => auth,
                        Err(error) => {
                            let error = classify(error, None, &inference).await;
                            let _ = events.send(agent::Event::Failed(error));
                            return;
                        }
                    };
                    let mut incoming = transport.start(request_input(request), selected.clone(), auth);
                    while let Some(event) = incoming.recv().await {
                        let event = match event {
                            Event::Call {carry} => agent::Event::Call {carry:carry.0},
                            Event::Code(code) => agent::Event::Code(code),
                            Event::Completed(step) => agent::Event::Completed(step_output(step)),
                            Event::NeedsContext => agent::Event::NeedsContext,
                            Event::Failed(error) => agent::Event::Failed(classify(error,Some(&selected),&inference).await),
                        };
                        if events.send(event).is_err() { break; }
                    }
                } => {}
            }
        });
        response
    }
}

async fn classify(
    error: anyhow::Error,
    selected: Option<&crate::SelectedAuth>,
    inference: &crate::Inference,
) -> anyhow::Error {
    let retryable = match selected {
        Some(selected) => inference.retryable(&error, selected).await,
        None => super::is_retryable(&error),
    };
    if retryable {
        agent::Retryable(format!("{error:#}")).into()
    } else {
        error
    }
}

fn step_output(step: Step) -> agent::Step {
    agent::Step {
        continuation: step.response_id.map(agent::Continuation::new),
        call: step.call.map(|call| agent::Call::new(call.id.0, call.code)),
        prose: step.prose,
        carry: step.carry.0,
        usage: step.usage,
    }
}

fn request_input(request: agent::Request) -> Request {
    let items = request
        .items
        .into_iter()
        .map(|item| match item {
            agent::Item::Step { carry, exec } => {
                // Interrupted execution records only the code actually admitted.
                // The provider-start payload still owns the original call identity.
                let mut carry = Carry(carry);
                if let Some(code) = exec
                    && carry.replay().pending_exec
                {
                    // A stream-start carry contains exactly its one unfinished call.
                    let mut data: serde_json::Value =
                        serde_json::from_str(carry.0.data().get()).expect("stream-start carry");
                    data["items"][0]["input"] = code.into();
                    carry.0 = agent::Carry::new(data, carry.0.display_calls(), false);
                }
                Item::Step(carry)
            }
            agent::Item::Report {
                text,
                images,
                reply_to,
            } => {
                let results =
                    reply_to.map_or_else(Vec::new, |carry| Carry(carry).reply(&text, &images));
                Item::Report {
                    text,
                    images,
                    results,
                }
            }
            agent::Item::User { text, images } => Item::User { text, images },
            agent::Item::CompactionTrigger => Item::CompactionTrigger,
        })
        .collect();
    let key = super::CacheKey::from_u128(request.cache_key.0);
    match request.continuation {
        Some(previous) => {
            Request::continuation(request.instructions, items, key, previous.into_token())
        }
        None => Request::new(request.instructions, items, key),
    }
}

#[cfg(test)]
mod tests {
    use agent::{Call as DisplayCall, Image};
    use serde_json::json;

    use super::*;

    #[test]
    fn interrupted_code_is_patched_but_historical_calls_are_never_rewritten() {
        let start =
            super::super::Carry::bare(super::super::Call::from_legacy("stream", String::new())).0;
        let historical = agent::Carry::new(
            json!({"items":[
                {"type":"custom_tool_call","call_id":"old-a","input":"alpha()"},
                {"type":"custom_tool_call","call_id":"old-b","input":"beta()"}
            ]}),
            vec![
                DisplayCall::new("old-a", "alpha()".into()),
                DisplayCall::new("old-b", "beta()".into()),
            ],
            false,
        );
        let compacted = agent::Carry::new(
            json!({"items":[
                {"type":"compaction","encrypted_content":"kept"},
                {"type":"custom_tool_call","call_id":"kept","input":"retained()"}
            ]}),
            vec![
                DisplayCall::new("evicted", "evicted()".into()),
                DisplayCall::new("kept", "retained()".into()),
            ],
            true,
        );
        // Historical Entry.exec is the first display call, not the last retained call.
        let request = request_input(agent::Request::new(
            "system".into(),
            vec![
                agent::Item::Step {
                    carry: historical,
                    exec: Some("alpha()".into()),
                },
                agent::Item::Step {
                    carry: start.clone(),
                    exec: Some("admitted()".into()),
                },
                agent::Item::Report {
                    text: "observed".into(),
                    images: vec![Image {
                        media_type: "image/png".into(),
                        data: vec![3, 8],
                    }],
                    reply_to: Some(start),
                },
            ],
            agent::CacheKey::from_u128(2),
        ));
        let Item::Step(history) = &request.items[0] else {
            panic!()
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(history.0.data().get()).unwrap()["items"][1]
                ["input"],
            "beta()"
        );
        let Item::Step(partial) = &request.items[1] else {
            panic!()
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(partial.0.data().get()).unwrap()["items"][0]
                ["input"],
            "admitted()"
        );
        let Item::Result(result) = &request.items[2] else {
            panic!()
        };
        assert_eq!(result.display_id(), "stream");
        assert_eq!(result.text, "observed");
        assert_eq!(result.images[0].data, [3, 8]);
        let replay = request_input(agent::Request::new(
            "system".into(),
            vec![agent::Item::Step {
                carry: compacted,
                exec: Some("evicted()".into()),
            }],
            agent::CacheKey::from_u128(2),
        ));
        let Item::Step(compacted) = &replay.items[0] else {
            panic!()
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(compacted.0.data().get()).unwrap()["items"]
                [1]["input"],
            "retained()"
        );
    }

    #[test]
    fn display_metadata_is_not_authority_for_provider_pairing() {
        let carry = agent::Carry::new(
            json!({"items":[]}),
            vec![DisplayCall::new("evicted", "old()".into())],
            false,
        );
        let request = request_input(agent::Request::continuation(
            "system".into(),
            vec![agent::Item::Report {
                text: "ordinary update".into(),
                images: vec![],
                reply_to: Some(carry),
            }],
            agent::CacheKey::from_u128(3),
            agent::Continuation::new("previous".into()),
        ));
        assert_eq!(request.previous_response_id.as_deref(), Some("previous"));
        assert!(matches!(&request.items[0],Item::User{text,..} if text=="ordinary update"));
    }
}
