use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::Form;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::net::{TcpListener, UnixListener};

use super::*;

fn grant(refresh: &str) -> String {
    json!({"type":"authorized_user","client_id":"desktop","client_secret":"secret","refresh_token":refresh}).to_string()
}

#[test]
fn import_keeps_only_the_offline_grant_and_never_quotes_bad_input() {
    let expected: Value = serde_json::from_str(&grant("refresh")).unwrap();
    let mut input = expected.clone();
    input["token_uri"] = json!("http://untrusted.invalid");
    input["access_token"] = json!("discard");
    let clean = authorized_user_json(&input.to_string()).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&clean).unwrap(), expected);
    for field in ["type", "client_id", "client_secret", "refresh_token"] {
        let mut input = expected.clone();
        input[field] = json!(" ");
        assert!(authorized_user_json(&input.to_string()).is_err());
    }
    input["type"] = json!("do-not-log");
    assert!(
        !authorized_user_json(&input.to_string())
            .unwrap_err()
            .to_string()
            .contains("do-not-log")
    );
}

async fn http(router: Router) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

async fn socket(router: Router) -> (tempfile::TempDir, Client) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("google.sock");
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (dir, Client::builder().unix_socket(path).build().unwrap())
}

#[tokio::test]
async fn native_reads_use_the_current_host_grant_and_preserve_google_responses() {
    let calls = Arc::new(AtomicUsize::new(0));
    let token = http(Router::new().route("/token", post({
        let calls = calls.clone();
        move |Form(form): Form<std::collections::HashMap<String,String>>| async move {
            assert_eq!(form["client_id"], "desktop");
            assert_eq!(form["client_secret"], "secret");
            assert_eq!(form["grant_type"], "refresh_token");
            let seq = calls.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({"access_token": format!("{}-{seq}", form["refresh_token"]), "expires_in": 3600}))
        }
    }))).await.join("/token").unwrap();
    let drive = http(Router::new().route(
        "/drive/v3/files/doc/export",
        get(|uri: Uri, h: HeaderMap| async move {
            assert_eq!(uri.query(), Some("mimeType=text%2Fplain&fields=id%2Cname"));
            assert_eq!(h[header::AUTHORIZATION], "Bearer initial-0");
            assert_eq!(h[header::RANGE], "bytes=0-2");
            assert_eq!(h["x-goog-drive-resource-keys"], "doc/shared-key");
            assert!(!h.contains_key("x-http-method-override"));
            assert!(!h.contains_key(header::COOKIE));
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, "text/plain"),
                    (header::CONTENT_RANGE, "bytes 0-2/10"),
                    (header::SET_COOKIE, "do-not-forward"),
                ],
                vec![0xff, 0, 1],
            )
        }),
    ))
    .await;
    let meet = http(Router::new().route(
        "/v2/conferenceRecords/c/transcripts/t/entries",
        get(|headers: HeaderMap| async move {
            match headers[header::AUTHORIZATION].to_str().unwrap() {
                "Bearer initial-0" => (StatusCode::FORBIDDEN, "not a participant"),
                "Bearer replaced-1" => (StatusCode::UNAUTHORIZED, "expired"),
                "Bearer replaced-2" => (StatusCode::OK, "a transcript entry"),
                _ => panic!("unexpected credential"),
            }
        }),
    ))
    .await;
    let installed = Arc::new(StdMutex::new(Some(grant("initial"))));
    let router = router_at(Endpoints { token, drive, meet }, {
        let installed = installed.clone();
        move || installed.lock().unwrap().clone().ok_or(anyhow!("missing"))
    });
    let (_dir, client) = socket(router).await;
    let res = client
        .get("http://localhost/drive/v3/files/doc/export?mimeType=text%2Fplain&fields=id%2Cname")
        .header(header::AUTHORIZATION, "Bearer agent")
        .header(header::COOKIE, "bad")
        .header("x-goog-drive-resource-keys", "doc/shared-key")
        .header("x-http-method-override", "DELETE")
        .header(header::RANGE, "bytes=0-2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.headers()[header::CONTENT_RANGE], "bytes 0-2/10");
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/plain");
    assert!(!res.headers().contains_key(header::SET_COOKIE));
    assert_eq!(&res.bytes().await.unwrap()[..], &[0xff, 0, 1]);

    let entries = "http://localhost/v2/conferenceRecords/c/transcripts/t/entries";
    let res = client.get(entries).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(res.text().await.unwrap(), "not a participant");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    *installed.lock().unwrap() = Some(grant("replaced"));
    assert_eq!(
        client.get(entries).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(entries)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "a transcript entry"
    );
    *installed.lock().unwrap() = None;
    assert_eq!(
        client.get(entries).send().await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn reject_unsafe_targets_before_reading_any_credentials() {
    let (_dir, client) = socket(router(|| panic!("must reject before reading the grant"))).await;
    for (method, path, status) in [
        (
            Method::POST,
            "/drive/v3/files",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Method::DELETE,
            "/v2/conferenceRecords/id",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (Method::GET, "/v2/spaces/abc", StatusCode::BAD_REQUEST),
        (
            Method::GET,
            "//evil.example/drive/v3/files",
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/drive/v3/files/a%2F..%2F..",
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/drive/v3/files?%24httpHeaders=x",
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/drive/v3/files?access_token=x",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            client
                .request(method, format!("http://localhost{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            status,
            "{path}"
        );
    }
}

#[tokio::test]
async fn oauth_errors_are_sanitized_and_redirects_never_leave_the_proxy() {
    let hits = Arc::new(AtomicUsize::new(0));
    let dest = http(Router::new().fallback({
        let hits = hits.clone();
        move || async move {
            hits.fetch_add(1, Ordering::SeqCst);
            "wrong origin"
        }
    }))
    .await;
    let token = http(Router::new().route(
        "/token",
        post(
            |Form(form): Form<std::collections::HashMap<String, String>>| async move {
                if form["refresh_token"] == "bad" {
                    (StatusCode::BAD_REQUEST, "sensitive OAuth diagnostic").into_response()
                } else {
                    axum::Json(json!({"access_token":"bearer","expires_in":3600})).into_response()
                }
            },
        ),
    ))
    .await
    .join("/token")
    .unwrap();
    let drive = http(Router::new().fallback(move || async move {
        (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, dest.as_str().to_owned())],
        )
    }))
    .await;
    let installed = Arc::new(StdMutex::new(grant("bad")));
    let router = router_at(
        Endpoints {
            token,
            meet: drive.clone(),
            drive,
        },
        {
            let installed = installed.clone();
            move || Ok(installed.lock().unwrap().clone())
        },
    );
    let (_dir, client) = socket(router).await;
    let url = "http://localhost/drive/v3/files/id";
    let res = client.get(url).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert!(!res.text().await.unwrap().contains("sensitive"));
    *installed.lock().unwrap() = grant("ok");
    let res = client.get(url).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert!(!res.headers().contains_key(header::LOCATION));
    assert!(res.text().await.unwrap().contains("redirect"));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}
