use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use axum::extract::Request;
use reqwest::Url;
use serde_json::{Value, json};

use super::*;

/// A fake Google: its token endpoint and one API server for every host.
#[derive(Default)]
struct Fake {
    /// The refresh grants it saw, as their form fields.
    refreshes: Vec<HashMap<String, String>>,
    /// The API calls it saw: method, path with query, bearer token, the
    /// agent's own header, and body.
    calls: Vec<(String, String, String, String, String)>,
    /// Tokens it answers 401 to.
    revoked: HashSet<String>,
    /// The lifetime of the access tokens it grants.
    expires_in: u64,
}

async fn fake() -> (Url, Arc<Mutex<Fake>>) {
    let fake = Arc::new(Mutex::new(Fake {
        expires_in: 3600,
        ..Fake::default()
    }));
    let shared = fake.clone();
    let app = Router::new().fallback(any(move |request: Request| {
        let fake = shared.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let mut fake = fake.lock().unwrap();
            if parts.uri.path() == "/token" {
                let form: HashMap<String, String> =
                    url::form_urlencoded::parse(&body).into_owned().collect();
                fake.refreshes.push(form);
                let n = fake.refreshes.len();
                return axum::Json(json!({
                    "access_token": format!("a{n}"),
                    "expires_in": fake.expires_in,
                    "token_type": "Bearer",
                }))
                .into_response();
            }
            let header = |name: &str| {
                parts
                    .headers
                    .get(name)
                    .map(|value| value.to_str().unwrap().to_owned())
                    .unwrap_or_default()
            };
            let bearer = header("authorization");
            fake.calls.push((
                parts.method.to_string(),
                parts.uri.to_string(),
                bearer.clone(),
                header("x-agent"),
                String::from_utf8(body.to_vec()).unwrap(),
            ));
            if fake
                .revoked
                .contains(bearer.strip_prefix("Bearer ").unwrap_or_default())
            {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(json!({"error": {"code": 401, "message": "expired"}})),
                )
                    .into_response();
            }
            (
                StatusCode::CREATED,
                [
                    (header::CONTENT_TYPE, "text/plain"),
                    (header::SET_COOKIE, "session=1"),
                ],
                "transcript text",
            )
                .into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake)
}

async fn server(grant: bool, google: &Url) -> String {
    let app = router(
        Arc::new(move |name| match name {
            CLIENT_ID if grant => Ok("client".to_owned()),
            CLIENT_SECRET if grant => Ok("secret\n".to_owned()),
            REFRESH_TOKEN if grant => Ok("r1".to_owned()),
            _ => anyhow::bail!("no {name}"),
        }),
        Endpoints::at(google),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn calls(fake: &Arc<Mutex<Fake>>) -> Vec<(String, String, String, String, String)> {
    std::mem::take(&mut fake.lock().unwrap().calls)
}

#[tokio::test]
async fn forwards_the_call_raw_with_the_grants_token_in_place_of_the_agents() {
    let (google, fake) = fake().await;
    let server = server(true, &google).await;

    let response = reqwest::Client::new()
        .post(format!(
            "{server}/meet.googleapis.com/v2/conferenceRecords%2Fabc/transcripts:list?pageSize=2&q=name%20contains%20%27x%27"
        ))
        .header("authorization", "Bearer agent-made-up")
        .header("cookie", "agent=1")
        .header("x-agent", "kept")
        .body("{\"a\":1}")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(response.text().await.unwrap(), "transcript text");
    assert_eq!(
        calls(&fake),
        [(
            "POST".to_owned(),
            "/v2/conferenceRecords%2Fabc/transcripts:list?pageSize=2&q=name%20contains%20%27x%27"
                .to_owned(),
            "Bearer a1".to_owned(),
            "kept".to_owned(),
            "{\"a\":1}".to_owned(),
        )]
    );
    let refreshes = std::mem::take(&mut fake.lock().unwrap().refreshes);
    assert_eq!(refreshes.len(), 1);
    assert_eq!(refreshes[0]["grant_type"], "refresh_token");
    assert_eq!(refreshes[0]["refresh_token"], "r1");
    assert_eq!(refreshes[0]["client_id"], "client");
    assert_eq!(refreshes[0]["client_secret"], "secret");
}

#[tokio::test]
async fn keeps_the_token_until_it_nears_expiry() {
    let (google, fake) = fake().await;
    let server = server(true, &google).await;
    let get = |path: &str| reqwest::get(format!("{server}/www.googleapis.com{path}"));

    get("/drive/v3/files").await.unwrap();
    get("/drive/v3/about").await.unwrap();
    assert_eq!(fake.lock().unwrap().refreshes.len(), 1);

    // A token that expires within the margin is refreshed before each call.
    fake.lock().unwrap().revoked.insert("a1".to_owned());
    fake.lock().unwrap().expires_in = 30;
    get("/drive/v3/files").await.unwrap();
    get("/drive/v3/files").await.unwrap();
    let bearers: Vec<String> = calls(&fake).into_iter().map(|call| call.2).collect();
    assert_eq!(
        bearers,
        [
            "Bearer a1",
            "Bearer a1",
            "Bearer a1",
            "Bearer a2",
            "Bearer a3"
        ]
    );
}

#[tokio::test]
async fn a_401_refreshes_once_and_retries_once() {
    let (google, fake) = fake().await;
    let server = server(true, &google).await;
    let get = || reqwest::get(format!("{server}/www.googleapis.com/drive/v3/files"));

    get().await.unwrap();
    // Revoked: the kept token fails, a fresh one works.
    fake.lock().unwrap().revoked.insert("a1".to_owned());
    let response = get().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bearers: Vec<String> = calls(&fake).into_iter().map(|call| call.2).collect();
    assert_eq!(bearers, ["Bearer a1", "Bearer a1", "Bearer a2"]);

    // Google refuses every token: the agent gets Google's 401, after one retry.
    fake.lock()
        .unwrap()
        .revoked
        .extend(["a2".to_owned(), "a3".to_owned()]);
    let response = get().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "expired");
    let bearers: Vec<String> = calls(&fake).into_iter().map(|call| call.2).collect();
    assert_eq!(bearers, ["Bearer a2", "Bearer a3"]);
}

#[tokio::test]
async fn the_token_goes_only_to_hosts_under_googleapis_com() {
    let (google, fake) = fake().await;
    let server = server(true, &google).await;

    for host in [
        "example.com",
        "googleapis.com",
        "googleapis.com.example.com",
        "www.googleapis.com.example.com",
        "evilgoogleapis.com",
        "WWW.googleapis.com",
        "www.googleapis.com:8443",
        "x@www.googleapis.com",
        ".googleapis.com",
        "a..googleapis.com",
    ] {
        let response = reqwest::get(format!("{server}/{host}/drive/v3/files"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host}");
        let body: Value = response.json().await.unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("rho_host_unavailable"),
            "{host}: {body}"
        );
    }
    let response = reqwest::get(format!("{server}/")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(calls(&fake).is_empty());
    assert!(fake.lock().unwrap().refreshes.is_empty());
}

#[tokio::test]
async fn without_a_grant_the_agent_is_told_to_run_init() {
    let (google, fake) = fake().await;
    let server = server(false, &google).await;

    let response = reqwest::get(format!("{server}/www.googleapis.com/drive/v3/files"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("rho_no_google_grant"),
        "{body}"
    );
    assert!(calls(&fake).is_empty());
}

#[test]
fn split_keeps_the_path_encoded() {
    assert_eq!(
        split("/www.googleapis.com/drive/v3/files/a%2Fb"),
        Some(("www.googleapis.com", "/drive/v3/files/a%2Fb"))
    );
    assert_eq!(
        split("/meet.googleapis.com"),
        Some(("meet.googleapis.com", "/"))
    );
    assert_eq!(split("/"), None);
}
