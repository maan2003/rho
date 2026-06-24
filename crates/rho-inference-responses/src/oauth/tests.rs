use serde_json::json;

use super::*;

fn jwt_with_claims(claims: Value) -> String {
    let header = URL_SAFE_NO_PAD.encode("{}");
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    format!("{header}.{payload}.signature")
}

#[test]
fn oauth_file_saves_loads_and_resolves_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let file = OAuthFile::open_in(temp.path(), "chatgpt").unwrap();
    file.save(&ResponsesOAuthCredentials {
        access_token: "access".to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: Some("acct".to_owned()),
    })
    .unwrap();

    let auth = ResponsesAuth::oauth_file(file.path());
    let resolved = auth
        .resolve_with_refresh(|_| panic!("should not refresh"))
        .unwrap()
        .unwrap();

    assert_eq!(resolved.bearer_token, "access");
    assert_eq!(resolved.account_id.as_deref(), Some("acct"));
}

#[test]
fn oauth_credentials_remain_the_file_json_format() {
    let credentials = ResponsesOAuthCredentials {
        access_token: "access".to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: 123,
        account_id: Some("acct".to_owned()),
    };

    let json = serde_json::to_string(&credentials).unwrap();
    let round_tripped: ResponsesOAuthCredentials = serde_json::from_str(&json).unwrap();

    assert_eq!(round_tripped, credentials);
}

#[test]
fn oauth_file_open_at_uses_exact_auth_dir() {
    let file = OAuthFile::open_at("/tmp/rho-auth.d", "chatgpt").unwrap();

    assert_eq!(file.path(), PathBuf::from("/tmp/rho-auth.d/chatgpt.json"));
}

#[test]
fn oauth_file_delete_reports_whether_file_existed() {
    let temp = tempfile::tempdir().unwrap();
    let file = OAuthFile::open_in(temp.path(), "chatgpt").unwrap();

    assert!(!file.delete().unwrap());
    file.save(&ResponsesOAuthCredentials::from_access_token("access"))
        .unwrap();
    assert!(file.delete().unwrap());
    assert!(!file.delete().unwrap());
}

#[test]
fn oauth_file_refreshes_expired_credentials_and_persists_them() {
    let temp = tempfile::tempdir().unwrap();
    let file = OAuthFile::open_in(temp.path(), "chatgpt").unwrap();
    file.save(&ResponsesOAuthCredentials {
        access_token: "old".to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: 1,
        account_id: Some("old-account".to_owned()),
    })
    .unwrap();

    let auth = ResponsesAuth::oauth_file(file.path());
    let resolved = auth
        .resolve_with_refresh(|refresh_token| {
            assert_eq!(refresh_token, "refresh");
            Ok(ResponsesOAuthCredentials {
                access_token: "new".to_owned(),
                refresh_token: "new-refresh".to_owned(),
                expires_at_ms: u64::MAX,
                account_id: Some("new-account".to_owned()),
            })
        })
        .unwrap()
        .unwrap();

    assert_eq!(resolved.bearer_token, "new");
    assert_eq!(resolved.account_id.as_deref(), Some("new-account"));
    assert_eq!(file.load().unwrap().unwrap().access_token, "new");
}

#[test]
fn refresh_policy_uses_jwt_half_life() {
    let old_iat = now_ms().saturating_sub(Duration::from_secs(120).as_millis() as u64) / 1000;
    let token = jwt_with_claims(json!({"iat": old_iat}));
    let expires_at_ms = now_ms().saturating_add(Duration::from_secs(120).as_millis() as u64);

    assert!(oauth_token_should_refresh(&token, expires_at_ms));
}

#[test]
fn oauth_extracts_account_id_from_openai_jwt() {
    let token = jwt_with_claims(json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct_from_jwt",
        },
    }));

    let credentials = ResponsesOAuthCredentials::from_access_token(token);

    assert_eq!(credentials.account_id.as_deref(), Some("acct_from_jwt"));
}

#[test]
fn oauth_file_rejects_unsafe_names() {
    for name in ["", ".hidden", "-leading", "has/slash", "has space"] {
        assert!(OAuthFile::open_in("/tmp", name).is_err());
    }
}

#[test]
fn openai_codex_auth_url_contains_pkce_fields() {
    let (auth_url, state, verifier) = openai_codex_auth_url();

    assert!(auth_url.starts_with(OPENAI_AUTH_URL));
    assert!(auth_url.contains("code_challenge_method=S256"));
    assert!(auth_url.contains("offline_access"));
    assert_eq!(state.len(), 32);
    assert_eq!(verifier.len(), 64);
}

#[test]
fn parse_redirect_url_accepts_full_url_path_and_query() {
    assert_eq!(
        parse_redirect_url("http://localhost:1455/auth/callback?code=abc&state=xyz").unwrap(),
        ("abc".to_owned(), "xyz".to_owned())
    );
    assert_eq!(
        parse_redirect_url("/auth/callback?code=abc&state=xyz").unwrap(),
        ("abc".to_owned(), "xyz".to_owned())
    );
    assert_eq!(
        parse_redirect_url("?code=abc&state=xyz").unwrap(),
        ("abc".to_owned(), "xyz".to_owned())
    );
}

#[test]
fn parse_redirect_url_rejects_bare_query_text() {
    let error = parse_redirect_url("code=abc&state=xyz").unwrap_err();

    assert!(error.contains("expected full URL"));
}
