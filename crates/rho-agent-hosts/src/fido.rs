//! A stable client identity sealed to the user's security key by WebAuthn's
//! hmac-secret extension (`hmacCreateSecret`/`hmacGetSecret`, touch only; PRF
//! would make the platform ask the key's PIN each time).
//! Only the credential id, never the derived secret, is stored on disk.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context as _, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng as _;
use redb::{TableDefinition, TableHandle as _};
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};
use serde_json::{Value, json};
use zbus::zvariant::{OwnedValue, Value as BusValue};

const RP_ID: &str = "gui.rho.dev";
const SALT: &[u8; 32] = b"rho iroh identity v1 (32 bytes)!";
const IDENTITY: TableDefinition<(), Sen<Credential>> = TableDefinition::new("rho_fido_identity_v1");
static DB: OnceLock<RhoDb> = OnceLock::new();
static ENDPOINT: tokio::sync::OnceCell<iroh::Endpoint> = tokio::sync::OnceCell::const_new();

#[derive(Clone, Debug, Encode, Decode)]
struct Credential {
    id: Vec<u8>,
    rp_id: String,
}

/// Set the client database before any host supervisor starts.
pub fn init(db: RhoDb) {
    let _ = DB.set(db);
}

pub async fn endpoint() -> anyhow::Result<iroh::Endpoint> {
    ENDPOINT
        .get_or_try_init(|| async {
            let db = DB.get().context("FIDO client database not initialized")?;
            let saved = {
                let read = db.read();
                if read.has_table(IDENTITY.name()) {
                    read.open_table(IDENTITY)
                        .get(())
                        .map(|row| row.value().into_owned())
                } else {
                    None
                }
            };
            let credential = match saved {
                Some(credential) => credential,
                None => {
                    let result = portal("CreateCredential", &creation_request()).await?;
                    let id = parse_creation(&result)?;
                    let credential = Credential {
                        id,
                        rp_id: RP_ID.to_owned(),
                    };
                    let mut write = db.write().await;
                    write
                        .open_table(IDENTITY)
                        .insert((), SenValue::borrowed(&credential));
                    write.commit();
                    credential
                }
            };
            ensure!(
                credential.rp_id == RP_ID,
                "unsupported FIDO credential RP ID"
            );
            let result = portal("GetCredential", &assertion_request(&credential)).await?;
            let secret = parse_assertion(&result)?;
            rho_rpc::bind_iroh_client(iroh::SecretKey::from_bytes(&secret)).await
        })
        .await
        .cloned()
}

fn random_challenge() -> String {
    let mut bytes = [0; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .expect("system entropy");
    URL_SAFE_NO_PAD.encode(bytes)
}

fn creation_request() -> Value {
    let mut user_id = [0; 16];
    rand::rngs::SysRng
        .try_fill_bytes(&mut user_id)
        .expect("system entropy");
    json!({
        "rp": {"id": RP_ID, "name": "rho"},
        "user": {"id": URL_SAFE_NO_PAD.encode(user_id), "name": "rho", "displayName": "rho"},
        "challenge": random_challenge(),
        "pubKeyCredParams": [{"type": "public-key", "alg": -8}, {"type": "public-key", "alg": -7}],
        "authenticatorSelection": {"residentKey": "discouraged", "userVerification": "discouraged"},
        "extensions": {"hmacCreateSecret": true}
    })
}

fn assertion_request(credential: &Credential) -> Value {
    json!({
        "challenge": random_challenge(),
        "rpId": credential.rp_id,
        "allowCredentials": [{"type": "public-key", "id": URL_SAFE_NO_PAD.encode(&credential.id)}],
        "userVerification": "discouraged",
        "extensions": {"hmacGetSecret": {"salt1": URL_SAFE_NO_PAD.encode(SALT)}}
    })
}

fn decode_field(response: &Value, path: &[&str]) -> anyhow::Result<Vec<u8>> {
    let value = path
        .iter()
        .try_fold(response, |value, key| value.get(*key))
        .and_then(Value::as_str)
        .with_context(|| format!("missing credential field {}", path.join(".")))?;
    URL_SAFE_NO_PAD
        .decode(value)
        .context("invalid credential base64url")
}

fn parse_creation(response: &Value) -> anyhow::Result<Vec<u8>> {
    ensure!(
        response.pointer("/clientExtensionResults/hmacCreateSecret") == Some(&json!(true)),
        "security key does not support hmac-secret"
    );
    let id = decode_field(
        response,
        &[if response.get("rawId").is_some() {
            "rawId"
        } else {
            "id"
        }],
    )?;
    ensure!(!id.is_empty(), "empty FIDO credential id");
    Ok(id)
}

fn parse_assertion(response: &Value) -> anyhow::Result<[u8; 32]> {
    let bytes = decode_field(
        response,
        &["clientExtensionResults", "hmacGetSecret", "output1"],
    )?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("hmac-secret output must be 32 bytes"))
}

async fn portal(method: &str, request: &Value) -> anyhow::Result<Value> {
    let bus = zbus::Connection::session()
        .await
        .context("connect to credentials portal session bus")?;
    let proxy = zbus::Proxy::new(
        &bus,
        "xyz.iinuwa.credentialsd.Credentials",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.handler.portal.experimental.Credential",
    )
    .await
    .context("find credentials portal")?;
    let mut options = HashMap::new();
    options.insert("public_key", BusValue::new(request.to_string()));
    let (code, results): (u32, HashMap<String, OwnedValue>) = if method == "CreateCredential" {
        proxy
            .call(
                method,
                &("", "app:dev.rho.Gui", "publicKey", options, "dev.rho.Gui"),
            )
            .await
    } else {
        proxy
            .call(method, &("", "app:dev.rho.Gui", options, "dev.rho.Gui"))
            .await
    }
    .with_context(|| format!("credentials portal {method}"))?;
    ensure!(
        code == 0,
        "credentials portal {method} declined (response {code})"
    );
    let kind: String = results
        .get("type")
        .context("missing portal credential type")?
        .try_clone()?
        .try_into()?;
    ensure!(
        kind == "public-key",
        "unexpected portal credential type: {kind}"
    );
    let nested: HashMap<String, OwnedValue> = results
        .get("public_key")
        .context("missing portal public_key")?
        .try_clone()?
        .try_into()?;
    let key = if method == "CreateCredential" {
        "registration_response_json"
    } else {
        "authentication_response_json"
    };
    let json_text: String = nested
        .get(key)
        .with_context(|| format!("missing portal {key}"))?
        .try_clone()?
        .try_into()?;
    serde_json::from_str(&json_text).context("parse portal credential JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_fields_and_hmac_secret_capability() {
        let request = creation_request();
        assert_eq!(request["rp"], json!({"id":"gui.rho.dev", "name":"rho"}));
        assert_eq!(request["user"]["name"], "rho");
        assert_eq!(request["user"]["displayName"], "rho");
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(request["user"]["id"].as_str().unwrap())
                .unwrap()
                .len(),
            16
        );
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(request["challenge"].as_str().unwrap())
                .unwrap()
                .len(),
            32
        );
        assert_eq!(
            request["pubKeyCredParams"],
            json!([{"type":"public-key","alg":-8},{"type":"public-key","alg":-7}])
        );
        assert_eq!(
            request["authenticatorSelection"],
            json!({"residentKey":"discouraged","userVerification":"discouraged"})
        );
        assert_eq!(request["extensions"], json!({"hmacCreateSecret":true}));
        let valid = json!({"rawId":"AQID", "clientExtensionResults":{"hmacCreateSecret":true}});
        assert_eq!(parse_creation(&valid).unwrap(), vec![1, 2, 3]);
        assert_eq!(
            parse_creation(
                &json!({"id":"BAUG", "clientExtensionResults":{"hmacCreateSecret":true}})
            )
            .unwrap(),
            vec![4, 5, 6]
        );
        assert!(
            parse_creation(
                &json!({"rawId":"AQID", "clientExtensionResults":{"hmacCreateSecret":false}})
            )
            .is_err()
        );
    }

    #[test]
    fn assertion_fields_and_derived_identity() {
        let request = assertion_request(&Credential {
            id: vec![1, 2, 3],
            rp_id: RP_ID.to_owned(),
        });
        assert_eq!(request["rpId"], "gui.rho.dev");
        assert_eq!(
            request["allowCredentials"],
            json!([{"type":"public-key","id":"AQID"}])
        );
        assert_eq!(request["userVerification"], "discouraged");
        assert_eq!(
            request["extensions"]["hmacGetSecret"]["salt1"],
            "cmhvIGlyb2ggaWRlbnRpdHkgdjEgKDMyIGJ5dGVzKSE"
        );
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(request["challenge"].as_str().unwrap())
                .unwrap()
                .len(),
            32
        );
        let bytes = [19_u8; 32];
        let response = json!({"clientExtensionResults":{"hmacGetSecret":{"output1": URL_SAFE_NO_PAD.encode(bytes)}}});
        assert_eq!(
            iroh::SecretKey::from_bytes(&parse_assertion(&response).unwrap()).public(),
            iroh::SecretKey::from_bytes(&bytes).public()
        );
        assert!(
            parse_assertion(
                &json!({"clientExtensionResults":{"hmacGetSecret":{"output1":"AQID"}}})
            )
            .is_err()
        );
    }
}
