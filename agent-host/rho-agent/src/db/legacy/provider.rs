//! Decoder for persisted responses written by the old inference step API.
//! Variant order, the CallId newtype and JSON strings are historical encoding.
use std::sync::Arc;

use senax_encoder::{Decode, Encode};
use serde_json::{Value, json};

use crate::inference::{self, Image};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
struct CallId(String);

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Call {
    id: CallId,
    pub code: String,
}
impl Call {
    pub fn from_legacy(id: impl Into<String>, code: String) -> Self {
        Self {
            id: CallId(id.into()),
            code,
        }
    }
    pub fn display_id(&self) -> &str {
        &self.id.0
    }
    fn display(&self) -> inference::Call {
        inference::Call::new(self.display_id(), self.code.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct CallResult {
    id: CallId,
    pub text: String,
    pub images: Vec<Image>,
}
impl CallResult {
    pub fn from_legacy(id: impl Into<String>, text: String, images: Vec<Image>) -> Self {
        Self {
            id: CallId(id.into()),
            text,
            images,
        }
    }
    pub fn display_id(&self) -> &str {
        &self.id.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Carry(Arc<Inner>);

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
enum Inner {
    OpenAi {
        items: Vec<String>,
        #[senax(default)]
        display: Option<Vec<Call>>,
        #[senax(default)]
        evidence: Vec<String>,
    },
    Scripted {
        call: Option<Call>,
    },
    ScriptedCompaction,
    Imported {
        text: String,
        images: Vec<Image>,
        results: Vec<CallResult>,
    },
}
impl Carry {
    pub fn from_openai_items(items: Vec<String>) -> Self {
        Self(Arc::new(Inner::OpenAi {
            items,
            display: None,
            evidence: Vec::new(),
        }))
    }
    pub fn from_openai_items_with_evidence(items: Vec<String>, evidence: Vec<String>) -> Self {
        Self(Arc::new(Inner::OpenAi {
            items,
            display: None,
            evidence,
        }))
    }
    pub fn has_compaction(&self) -> bool {
        match &*self.0 {
            Inner::OpenAi { items, .. } => items.iter().any(|item| {
                serde_json::from_str::<Value>(item).expect("persisted provider replay item")["type"]
                    == "compaction"
            }),
            Inner::ScriptedCompaction => true,
            _ => false,
        }
    }
    pub fn call_ids(&self) -> Vec<String> {
        match &*self.0 {
            Inner::OpenAi { items, .. } => {
                let start = items
                    .iter()
                    .rposition(|item| {
                        serde_json::from_str::<Value>(item).expect("persisted provider replay item")
                            ["type"]
                            == "compaction"
                    })
                    .unwrap_or(0);
                items
                    .iter()
                    .skip(start)
                    .filter_map(|item| {
                        let value: Value =
                            serde_json::from_str(item).expect("persisted provider replay item");
                        (value["type"] == "custom_tool_call")
                            .then(|| value["call_id"].as_str().map(str::to_owned))
                            .flatten()
                    })
                    .collect()
            }
            Inner::Scripted { call } => call.iter().map(|call| call.id.0.clone()).collect(),
            _ => Vec::new(),
        }
    }
    pub fn into_live(&self, display: &[Call]) -> inference::Carry {
        let (items, compacted, evidence) = match &*self.0 {
            Inner::OpenAi {
                items, evidence, ..
            } => (
                items
                    .iter()
                    .map(|item| {
                        serde_json::value::RawValue::from_string(item.clone())
                            .expect("persisted provider replay item")
                    })
                    .collect(),
                self.has_compaction(),
                evidence
                    .iter()
                    .map(|item| {
                        serde_json::from_str::<Value>(item).expect("migration evidence JSON")
                    })
                    .collect(),
            ),
            Inner::Scripted { call } => (
                call.iter()
                    .map(|call| {
                        serde_json::value::to_raw_value(&json!({
                            "type": "custom_tool_call", "id": format!("ctc_{}", call.display_id()),
                            "call_id": call.display_id(), "name": "exec", "input": call.code,
                        }))
                        .unwrap()
                    })
                    .collect(),
                false,
                vec![],
            ),
            Inner::ScriptedCompaction => (vec![], true, vec![]),
            Inner::Imported { .. } => unreachable!("input is not a response"),
        };
        #[derive(serde::Serialize)]
        struct Payload {
            items: Vec<Box<serde_json::value::RawValue>>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            evidence: Vec<Value>,
        }
        inference::Carry::new(
            Payload { items, evidence },
            display.iter().map(Call::display).collect(),
            compacted,
        )
    }
}

pub fn imported(text: &str, images: &[Image], results: &[CallResult]) -> inference::Carry {
    inference::Carry::new(
        json!({"imported": {
            "text": text,
            "images": images.iter().map(|image| json!({"media_type": image.media_type, "data": image.data})).collect::<Vec<_>>(),
            "results": results.iter().map(|result| json!({
                "id": result.display_id(), "text": result.text,
                "images": result.images.iter().map(|image| json!({"media_type": image.media_type, "data": image.data})).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        }}),
        vec![],
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Predates the optional `display` field and the skipped preparation cache.
    // Encode independently so changes to this decoder cannot make the fixture
    // pass by changing both sides of a same-type roundtrip.
    #[derive(senax_encoder::Encode)]
    struct OldCarry(Arc<OldInner>);

    #[derive(senax_encoder::Encode)]
    enum OldInner {
        OpenAi { items: Vec<String> },
    }

    #[test]
    fn old_response_variants_translate_without_losing_display_or_replay() {
        let evicted = Call::from_legacy("gone", "old()".into());
        let kept = Call::from_legacy("kept", "new()".into());
        const KEPT: &str =
            r#"{ "type":"custom_tool_call","call_id":"kept","input":"n\u0065w()","future":1e+02 }"#;
        let old = OldCarry(Arc::new(OldInner::OpenAi {
            items: vec![
                r#"{"type":"custom_tool_call","call_id":"gone","input":"old()"}"#.into(),
                r#"{"type":"compaction","encrypted_content":"summary"}"#.into(),
                KEPT.into(),
            ],
        }));
        let mut encoded = senax_encoder::encode(&old).unwrap();
        let decoded: Carry = senax_encoder::decode(&mut encoded).unwrap();
        assert_eq!(decoded.call_ids(), ["kept"]);
        let live = decoded.into_live(&[evicted, kept]);
        assert!(live.has_compaction());
        assert!(
            live.data().get().contains(KEPT),
            "migration must retain old JSON item bytes"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(live.data().get()).unwrap()["items"][0]["call_id"],
            "gone"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(live.data().get()).unwrap()["items"][2]["call_id"],
            "kept"
        );
        assert_eq!(
            live.display_calls()
                .iter()
                .map(|call| call.display_id())
                .collect::<Vec<_>>(),
            ["gone", "kept"]
        );

        let scripted = Carry(Arc::new(Inner::Scripted {
            call: Some(Call::from_legacy("x", "print(1)".into())),
        }));
        let mut encoded = senax_encoder::encode(&scripted).unwrap();
        let scripted: Carry = senax_encoder::decode(&mut encoded).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(scripted.into_live(&[]).data().get())
                .unwrap(),
            json!({"items": [{
                "type": "custom_tool_call", "id": "ctc_x", "call_id": "x", "name": "exec", "input": "print(1)"
            }]})
        );
        let compacted = Carry(Arc::new(Inner::ScriptedCompaction)).into_live(&[]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(compacted.data().get()).unwrap(),
            json!({"items": []})
        );
        assert!(compacted.has_compaction());
    }

    #[test]
    fn old_report_preserves_distinct_delayed_results_and_images() {
        let images = [Image {
            media_type: "image/png".into(),
            data: vec![5, 9],
        }];
        let carry = imported(
            "fallback",
            &images,
            &[
                CallResult::from_legacy("first", "alpha".into(), vec![]),
                CallResult::from_legacy("second", "beta".into(), images.to_vec()),
            ],
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(carry.data().get()).unwrap(),
            json!({"imported": {
                "text": "fallback", "images": [{"media_type": "image/png", "data": [5, 9]}],
                "results": [
                    {"id": "first", "text": "alpha", "images": []},
                    {"id": "second", "text": "beta", "images": [{"media_type": "image/png", "data": [5, 9]}]}
                ]
            }})
        );
        assert!(!carry.has_compaction());
        assert!(carry.display_calls().is_empty());
    }
}
