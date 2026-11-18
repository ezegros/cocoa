//! Public browser, identity, persistence and refresh contracts with an isolated issuer.

use std::{
    collections::BTreeMap,
    error::Error,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use cocoa_oauth::{BrowserLogin, OAuthClient, OAuthError, Service};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

type TestResult = Result<(), Box<dyn Error>>;

#[tokio::test]
async fn persisted_store_accepts_unknown_fields() -> TestResult {
    let fixture = Fixture::new().await?;
    fixture.login("account_one", 3600, "use.direct").await?;
    let expected = fixture.client.active_registration()?;
    let mut disk: Value = serde_json::from_slice(&std::fs::read(&fixture.path)?)?;
    disk["unknown"] = json!({"future": [true, null]});
    std::fs::write(&fixture.path, serde_json::to_vec(&disk)?)?;
    assert_eq!(fixture.client.active_registration()?, expected);
    assert_eq!(fixture.client.accounts()?.len(), 1);
    Ok(())
}

#[test]
fn literal_loopback_http_accepts_both_ip_families_without_trusting_other_hosts() -> TestResult {
    let directory = tempfile::tempdir()?;
    for issuer in ["http://127.0.0.1:8080", "http://[::1]:8080"] {
        assert!(OAuthClient::new(service(issuer), directory.path().join("oauth.json")).is_ok());
        assert!(
            cocoa_oauth::mcp::McpOAuthClient::new(
                &format!("{issuer}/mcp"),
                directory.path().join("mcp.json"),
                None,
            )
            .is_ok()
        );
    }
    for issuer in [
        "http://localhost:8080",
        "http://192.0.2.1:8080",
        "http://[2001:db8::1]:8080",
        "http://[::]:8080",
        "http://user@[::1]:8080",
    ] {
        assert!(matches!(
            OAuthClient::new(service(issuer), directory.path().join("oauth.json")),
            Err(OAuthError::Configuration)
        ));
        assert!(matches!(
            cocoa_oauth::mcp::McpOAuthClient::new(
                &format!("{issuer}/mcp"),
                directory.path().join("mcp.json"),
                None,
            ),
            Err(OAuthError::Configuration)
        ));
    }
    Ok(())
}

#[tokio::test]
async fn unrelated_callback_disconnect_does_not_cancel_login() -> TestResult {
    let fixture = Fixture::new().await?;
    let login = fixture.client.begin(None).await?;
    let query = parameters(&login)?;
    reset_callback(&Url::parse(&query["redirect_uri"])?).await?;
    fixture
        .exchange(&query, "connected", json!({}), grant(3600, "use.direct"))
        .await?;
    let completion = tokio::spawn(login.finish(CancellationToken::new()));
    let callback_result = callback(&query, "connected").await;
    let result = tokio::time::timeout(Duration::from_secs(5), completion).await??;
    assert!(result.is_ok(), "callback failed: {result:?}");
    let account = result?;
    callback_result?;
    assert!(account.active && account.connected);
    Ok(())
}

#[tokio::test]
async fn accepted_callback_outcome_survives_browser_disconnect() -> TestResult {
    let fixture = Fixture::new().await?;
    let login = fixture.client.begin(None).await?;
    let query = parameters(&login)?;
    fixture
        .exchange(&query, "connected", json!({}), grant(3600, "use.direct"))
        .await?;
    let mut redirect = Url::parse(&query["redirect_uri"])?;
    redirect.query_pairs_mut().extend_pairs([
        ("state", query["state"].as_str()),
        ("code", "one-use-code"),
        ("client_id", "connected"),
    ]);
    reset_callback(&redirect).await?;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        login.finish(CancellationToken::new()),
    )
    .await?;
    assert!(result.is_ok(), "callback failed: {result:?}");
    let account = result?;
    assert!(account.active && account.connected);

    let login = fixture.client.begin(None).await?;
    let query = parameters(&login)?;
    let mut redirect = Url::parse(&query["redirect_uri"])?;
    redirect.query_pairs_mut().extend_pairs([
        ("state", query["state"].as_str()),
        ("error", "access_denied"),
    ]);
    reset_callback(&redirect).await?;
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            login.finish(CancellationToken::new()),
        )
        .await?,
        Err(OAuthError::Denied)
    ));
    assert_eq!(fixture.client.active_registration()?, "connected");
    Ok(())
}

#[tokio::test]
async fn browser_registration_pkce_state_accounts_and_logout() -> TestResult {
    let fixture = Fixture::new().await?;
    let attempt = fixture.client.begin(None).await?;
    let initial = parameters(&attempt)?;
    assert_eq!(initial["client_id"], "registration_entrypoint");
    assert_eq!(initial["code_challenge_method"], "S256");
    assert_eq!(initial["agent_name_hint"], "Fixture");
    assert!(initial["host"].starts_with("urn:uuid:"));
    let redirect = Url::parse(&initial["redirect_uri"])?;
    let mut invalid = redirect.clone();
    invalid.query_pairs_mut().extend_pairs([
        ("state", "wrong"),
        ("code", "wrong"),
        ("client_id", "wrong"),
    ]);
    let pending = tokio::spawn(attempt.finish(CancellationToken::new()));
    assert_eq!(reqwest::get(invalid).await?.status().as_u16(), 400);
    fixture
        .exchange(
            &initial,
            "account_one",
            json!({}),
            grant(3600, "use.direct"),
        )
        .await?;
    callback(&initial, "account_one").await?;
    let account = pending.await??;
    assert!(account.active && account.connected && account.inference_enabled);
    assert_eq!(account.label, "same@example.test");
    let requests = fixture
        .server
        .received_requests()
        .await
        .ok_or("missing requests")?;
    let request = requests
        .iter()
        .find(|request| request.url.path() == "/token")
        .ok_or("missing exchange")?;
    let form: BTreeMap<String, String> = url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect();
    assert_eq!(form["client_id"], "account_one");
    assert_eq!(form["redirect_uri"], initial["redirect_uri"]);
    assert_eq!(
        BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())),
        initial["code_challenge"]
    );
    assert_eq!(
        fixture
            .client
            .access_token("account_one")
            .await?
            .expose_secret(),
        "access-initial"
    );

    fixture.login("account_two", 3600, "use.direct").await?;
    assert_eq!(fixture.client.accounts()?.len(), 2);
    assert!(matches!(
        fixture.client.access_token("account_one").await,
        Err(OAuthError::Changed)
    ));
    fixture.client.select("account_one").await?;
    let reconnect = fixture.client.begin(Some("account_one".into())).await?;
    let returning = parameters(&reconnect)?;
    assert_eq!(returning["client_id"], "account_one");
    assert_eq!(returning["host"], initial["host"]);
    assert!(!returning.contains_key("agent_name_hint"));
    assert!(!returning.contains_key("id_token_hint"));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        reconnect.finish(cancellation).await,
        Err(OAuthError::Cancelled)
    ));
    assert_eq!(fixture.client.active_registration()?, "account_one");

    Mock::given(method("POST"))
        .and(path("/revoke"))
        .and(body_string_contains("client_id=account_one"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&fixture.server)
        .await;
    assert!(matches!(
        fixture.client.logout("account_one").await,
        Err(OAuthError::Revocation)
    ));
    assert!(matches!(
        fixture.client.access_token("account_one").await,
        Err(OAuthError::Changed)
    ));
    let accounts = fixture.client.accounts()?;
    assert!(!accounts[0].connected);
    assert!(accounts[1].connected);
    let disk: Value = serde_json::from_slice(&std::fs::read(&fixture.path)?)?;
    assert!(disk["registrations"]["account_one"]["state"]["tokens"].is_null());
    assert_eq!(disk["host_id"], initial["host"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&fixture.path)?.permissions().mode() & 0o777,
            0o600
        );
    }
    Ok(())
}

#[tokio::test]
async fn unverified_identity_never_replaces_or_activates_registration() -> TestResult {
    let fixture = Fixture::new().await?;
    fixture.login("original", 3600, "use.direct").await?;
    for (index, claims) in [
        json!({"nonce":"wrong"}),
        json!({"iss":"https://wrong.test"}),
        json!({"aud":"another_client"}),
        json!({"exp":1}),
        json!({"iat":u64::MAX}),
        json!({"aud":["original","other"]}),
        json!({"sub":"bad\u{1b}[2J"}),
    ]
    .into_iter()
    .enumerate()
    {
        let login = fixture.client.begin(Some("original".into())).await?;
        let query = parameters(&login)?;
        fixture
            .exchange(&query, "original", claims, grant(3600, "use.direct"))
            .await?;
        let pending = tokio::spawn(login.finish(CancellationToken::new()));
        callback(&query, "original").await?;
        assert!(
            matches!(pending.await?, Err(OAuthError::Identity)),
            "identity case {index}"
        );
        assert_eq!(fixture.client.active_registration()?, "original");
    }
    let login = fixture.client.begin(Some("original".into())).await?;
    let query = parameters(&login)?;
    fixture
        .exchange(
            &query,
            "original",
            json!({"sub":"other_user"}),
            grant(3600, "use.direct"),
        )
        .await?;
    let pending = tokio::spawn(login.finish(CancellationToken::new()));
    callback(&query, "original").await?;
    assert!(matches!(pending.await?, Err(OAuthError::AccountMismatch)));
    let login = fixture.client.begin(Some("original".into())).await?;
    let query = parameters(&login)?;
    let pending = tokio::spawn(login.finish(CancellationToken::new()));
    let mut redirect = Url::parse(&query["redirect_uri"])?;
    redirect.query_pairs_mut().extend_pairs([
        ("state", query["state"].as_str()),
        ("code", "one-use-code"),
        ("client_id", "different_client"),
    ]);
    let response = reqwest::get(redirect).await?;
    assert_eq!(response.status().as_u16(), 400);
    assert!(response.text().await?.contains("Sign-in failed:"));
    assert!(matches!(pending.await?, Err(OAuthError::Changed)));
    assert_eq!(
        fixture
            .client
            .access_token("original")
            .await?
            .expose_secret(),
        "access-initial"
    );
    Ok(())
}

#[tokio::test]
async fn identity_only_and_missing_inference_scope_cannot_infer() -> TestResult {
    let fixture = Fixture::new().await?;
    fixture
        .login("limited", 3600, "openid offline_access")
        .await?;
    assert!(!fixture.client.accounts()?[0].inference_enabled);
    assert!(matches!(
        fixture.client.access_token("limited").await,
        Err(OAuthError::Scope)
    ));
    let login = fixture.client.begin(None).await?;
    let query = parameters(&login)?;
    fixture
        .exchange(
            &query,
            "identity_only",
            json!({}),
            json!({"scope":"openid"}),
        )
        .await?;
    let pending = tokio::spawn(login.finish(CancellationToken::new()));
    callback(&query, "identity_only").await?;
    let account = pending.await??;
    assert!(account.connected && !account.inference_enabled);
    assert!(matches!(
        fixture.client.access_token("identity_only").await,
        Err(OAuthError::Scope)
    ));
    Ok(())
}

#[tokio::test]
async fn cancelled_waiter_and_concurrent_clients_rotate_once_then_logout_wins() -> TestResult {
    tokio::time::timeout(Duration::from_secs(15), async {
        let fixture = Fixture::new().await?;
        fixture.login("rotating", 30, "use.direct").await?;
        let gate = fixture.gated_refresh(grant_rotated()).await;
        let client = fixture.client.clone();
        let waiter = tokio::spawn(async move { client.access_token("rotating").await });
        gate.requested.notified().await;
        assert!(
            !waiter.is_finished(),
            "refresh must wait for response release"
        );
        waiter.abort();
        assert!(waiter.await.is_err_and(|error| error.is_cancelled()));
        gate.release.send(())?;
        let other = OAuthClient::new(service(&fixture.server.uri()), fixture.path.clone())?;
        assert_eq!(
            other.access_token("rotating").await?.expose_secret(),
            "access-rotated"
        );
        let disk: Value = serde_json::from_slice(&std::fs::read(&fixture.path)?)?;
        assert_eq!(
            disk["registrations"]["rotating"]["state"]["tokens"]["refresh_token"],
            "refresh-rotated"
        );
        Mock::given(method("POST"))
            .and(path("/revoke"))
            .and(body_string_contains("token=refresh-rotated"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&fixture.server)
            .await;
        fixture.client.logout("rotating").await?;
        assert!(matches!(
            other.access_token("rotating").await,
            Err(OAuthError::Changed)
        ));
        assert!(!fixture.client.accounts()?[0].connected);
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn rotated_checkpoint_survives_jwks_failure_and_terminal_refresh_disconnects() -> TestResult {
    let fixture = Fixture::new().await?;
    fixture.login("checkpoint", 30, "use.direct").await?;
    let mut rotated = grant_rotated();
    rotated["id_token"] = json!(fixture.sign("checkpoint", json!({}))?);
    fixture.refresh(rotated, Duration::ZERO).await;
    let failed_jwks = Mock::given(path("/jwks"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount_as_scoped(&fixture.server)
        .await;
    assert!(matches!(
        fixture.client.access_token("checkpoint").await,
        Err(OAuthError::Network)
    ));
    drop(failed_jwks);
    assert_eq!(
        fixture
            .client
            .access_token("checkpoint")
            .await?
            .expose_secret(),
        "access-rotated"
    );
    fixture.login("revoked", 30, "use.direct").await?;
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"error":"invalid_grant","error_description":"secret-never-display"}),
        ))
        .up_to_n_times(1)
        .expect(1)
        .mount(&fixture.server)
        .await;
    let error = fixture
        .client
        .access_token("revoked")
        .await
        .err()
        .ok_or("expected failure")?;
    assert!(matches!(error, OAuthError::Reauthorize));
    assert!(!error.to_string().contains("secret-never-display"));
    assert!(
        !fixture
            .client
            .accounts()?
            .into_iter()
            .find(|account| account.registration == "revoked")
            .ok_or("missing account")?
            .connected
    );
    Ok(())
}

#[tokio::test]
async fn expired_checkpoint_recovers_with_its_rotated_refresh_token() -> TestResult {
    let fixture = Fixture::new().await?;
    fixture
        .login("expired_checkpoint", 30, "use.direct")
        .await?;
    let mut rotated = grant_rotated();
    rotated["expires_in"] = json!(1);
    rotated["id_token"] = json!(fixture.sign("expired_checkpoint", json!({}))?);
    fixture.refresh(rotated, Duration::ZERO).await;
    let failed_jwks = Mock::given(path("/jwks"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount_as_scoped(&fixture.server)
        .await;
    assert!(matches!(
        fixture.client.access_token("expired_checkpoint").await,
        Err(OAuthError::Network)
    ));
    drop(failed_jwks);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut recovered = grant_rotated();
    recovered["access_token"] = json!("access-recovered");
    Mock::given(path("/token"))
        .and(body_string_contains("refresh_token=refresh-rotated"))
        .respond_with(ResponseTemplate::new(200).set_body_json(recovered))
        .expect(1)
        .mount(&fixture.server)
        .await;
    assert_eq!(
        fixture
            .client
            .access_token("expired_checkpoint")
            .await?
            .expose_secret(),
        "access-recovered"
    );
    Ok(())
}

#[tokio::test]
async fn logout_during_rotation_removes_the_replacement_and_cannot_resurrect() -> TestResult {
    let fixture = Fixture::new().await?;
    fixture.login("logout_race", 30, "use.direct").await?;
    fixture
        .refresh(grant_rotated(), Duration::from_millis(150))
        .await;
    Mock::given(path("/revoke"))
        .and(body_string_contains("token=refresh-rotated"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&fixture.server)
        .await;
    let client = fixture.client.clone();
    let resolver = tokio::spawn(async move { client.access_token("logout_race").await });
    fixture.wait_for_refresh().await?;
    fixture.client.logout("logout_race").await?;
    resolver.await??;
    assert!(matches!(
        fixture.client.access_token("logout_race").await,
        Err(OAuthError::Changed)
    ));
    assert!(!fixture.client.accounts()?[0].connected);
    assert!(!std::fs::read_to_string(&fixture.path)?.contains("refresh-rotated"));
    Ok(())
}

#[tokio::test]
async fn exchange_failure_retains_issued_client_and_signature_failure_never_activates() -> TestResult
{
    let fixture = Fixture::new().await?;
    let login = fixture.client.begin(None).await?;
    let query = parameters(&login)?;
    Mock::given(path("/token"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(json!({"error":"temporarily_unavailable"})),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&fixture.server)
        .await;
    let pending = tokio::spawn(login.finish(CancellationToken::new()));
    callback(&query, "saved_issued_client").await?;
    assert!(matches!(pending.await?, Err(OAuthError::Network)));
    assert_eq!(
        fixture.client.accounts()?[0].registration,
        "saved_issued_client"
    );
    let login = fixture
        .client
        .begin(Some("saved_issued_client".into()))
        .await?;
    let query = parameters(&login)?;
    assert_eq!(query["client_id"], "saved_issued_client");
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let invalid = encode(
        &Header::new(Algorithm::HS256),
        &json!({"iss":fixture.server.uri(),"aud":"saved_issued_client","sub":"forged","nonce":query["nonce"],"iat":now,"exp":now+3600}),
        &EncodingKey::from_secret(b"not-a-provider-signing-key"),
    )?;
    let mut response = grant(3600, "use.direct");
    response["id_token"] = json!(invalid);
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .up_to_n_times(1)
        .expect(1)
        .mount(&fixture.server)
        .await;
    let pending = tokio::spawn(login.finish(CancellationToken::new()));
    callback(&query, "saved_issued_client").await?;
    assert!(matches!(pending.await?, Err(OAuthError::Identity)));
    assert!(matches!(
        fixture.client.active_registration(),
        Err(OAuthError::Reauthorize)
    ));
    Ok(())
}

#[tokio::test]
async fn discovery_and_storage_cannot_redirect_credentials() -> TestResult {
    let fixture = Fixture::new().await?;
    Mock::given(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer":fixture.server.uri(),"authorization_endpoint":"https://foreign.test/authorize",
            "token_endpoint":format!("{}/token", fixture.server.uri()),"jwks_uri":format!("{}/jwks", fixture.server.uri())
        }))).with_priority(1).up_to_n_times(1).mount(&fixture.server).await;
    assert!(matches!(
        fixture.client.begin(None).await,
        Err(OAuthError::Response)
    ));
    fixture.login("private", 3600, "use.direct").await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o644))?;
        assert!(matches!(
            fixture.client.accounts(),
            Err(OAuthError::Storage)
        ));
        std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o600))?;
        let alias = fixture.path.with_file_name("alias.json");
        std::os::unix::fs::symlink(&fixture.path, &alias)?;
        let client = OAuthClient::new(service(&fixture.server.uri()), alias)?;
        assert!(matches!(client.accounts(), Err(OAuthError::Storage)));
    }
    Ok(())
}

#[tokio::test]
async fn separate_processes_share_the_rotation_lock() -> TestResult {
    tokio::time::timeout(Duration::from_secs(15), async {
        let fixture = Fixture::new().await?;
        fixture.login("process", 30, "use.direct").await?;
        fixture
            .refresh(grant_rotated(), Duration::from_millis(150))
            .await;
        let mut children = Vec::new();
        for temporary_directory in [
            fixture.path.join("unavailable"),
            fixture._directory.path().to_path_buf(),
        ] {
            children.push(
                tokio::process::Command::new(std::env::current_exe()?)
                    .args([
                        "--exact",
                        "process_worker",
                        "--nocapture",
                        "--format=pretty",
                        "--color=never",
                    ])
                    .env("COCOA_TEST_OAUTH_STORE", &fixture.path)
                    .env("COCOA_TEST_OAUTH_ISSUER", fixture.server.uri())
                    .env("TMPDIR", temporary_directory)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()?,
            );
        }
        for child in children {
            let output = child.wait_with_output().await?;
            assert_process_worker_completed(&output);
        }
        assert_eq!(
            fixture
                .client
                .access_token("process")
                .await?
                .expose_secret(),
            "access-rotated"
        );
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn graceful_process_exit_finishes_rotation_after_waiter_cancellation() -> TestResult {
    tokio::time::timeout(Duration::from_secs(15), async {
        let fixture = Fixture::new().await?;
        fixture.login("process", 30, "use.direct").await?;
        let gate = fixture.gated_refresh(grant_rotated()).await;
        let cancel = fixture.path.with_extension("cancel");
        let child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "process_worker",
                "--nocapture",
                "--format=pretty",
                "--color=never",
            ])
            .env("COCOA_TEST_OAUTH_STORE", &fixture.path)
            .env("COCOA_TEST_OAUTH_ISSUER", fixture.server.uri())
            .env("COCOA_TEST_OAUTH_CANCEL", &cancel)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        gate.requested.notified().await;
        std::fs::write(&cancel, b"cancel")?;
        wait_for_file(&cancel.with_extension("cancelled")).await?;
        gate.release.send(())?;
        let output = child.wait_with_output().await?;
        assert_process_worker_completed(&output);
        let disk: Value = serde_json::from_slice(&std::fs::read(&fixture.path)?)?;
        assert_eq!(
            disk["registrations"]["process"]["state"]["tokens"]["refresh_token"],
            "refresh-rotated"
        );
        assert_eq!(
            fixture
                .client
                .access_token("process")
                .await?
                .expose_secret(),
            "access-rotated"
        );
        Ok(())
    })
    .await?
}

#[test]
fn process_worker() -> TestResult {
    let Some(path) = std::env::var_os("COCOA_TEST_OAUTH_STORE") else {
        return Ok(());
    };
    let issuer = std::env::var("COCOA_TEST_OAUTH_ISSUER")?;
    let client = OAuthClient::new(service(&issuer), path.into())?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(15), async {
                if let Some(cancel) = std::env::var_os("COCOA_TEST_OAUTH_CANCEL") {
                    let waiter = tokio::spawn(async move { client.access_token("process").await });
                    let cancel = std::path::Path::new(&cancel);
                    wait_for_file(cancel).await?;
                    assert!(!waiter.is_finished(), "rotation must still be in flight");
                    waiter.abort();
                    assert!(waiter.await.is_err_and(|error| error.is_cancelled()));
                    std::fs::write(cancel.with_extension("cancelled"), b"cancelled")?;
                    cocoa_oauth::finish_refreshes().await;
                    return Ok(());
                }
                assert_eq!(
                    client.access_token("process").await?.expose_secret(),
                    "access-rotated"
                );
                Ok(())
            })
            .await?
        })
}

struct Fixture {
    server: MockServer,
    client: OAuthClient,
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
        let server = MockServer::start().await;
        let issuer = server.uri();
        Mock::given(path("/.well-known/openid-configuration")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer":issuer,"authorization_endpoint":format!("{issuer}/authorize"),
            "token_endpoint":format!("{issuer}/token"),"jwks_uri":format!("{issuer}/jwks"),"revocation_endpoint":format!("{issuer}/revoke")
        }))).mount(&server).await;
        Mock::given(path("/jwks")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"keys":[{
            "kty":"RSA","kid":"test","alg":"RS256","use":"sig","e":"AQAB",
            "n":"scFO3Wj280LOjp3HwjAODMmqh_OhXmR7nw06vykyRH0AGy1skweyLW7R0n-gCWzRp_n4N3IFfEcD7ck0zcfXQpuUXo0ngJ0rX6JW9wPJFbJufWIKUc3qyajW6JJYwYNbrTNnYdpaoou1KjCesO1WcwMj9AVvueJ1dHE10pXPskQKv-mOgnuUHBmDVyfjCTOahRlq2KcDcoOucpXik42dk37qsiCYcgLH6eXL-AYZhAlqRtpXKSKV355-DQRN4A1WYwTIjDlW9NT5pyioax2CAbNguvqKF0abP_iimb3Qmi2aGuI9wNd33xJ-vc1n3hoNtpr-5rRHAqJK_KK82TwZzw"
        }]}))).mount(&server).await;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("private/oauth.json");
        let client = OAuthClient::new(service(&issuer), path.clone())?;
        Ok(Self {
            server,
            client,
            path,
            _directory: directory,
        })
    }

    async fn login(&self, registration: &str, expires: u64, scope: &str) -> TestResult {
        let login = self.client.begin(None).await?;
        let query = parameters(&login)?;
        self.exchange(&query, registration, json!({}), grant(expires, scope))
            .await?;
        let pending = tokio::spawn(login.finish(CancellationToken::new()));
        callback(&query, registration).await?;
        pending.await??;
        Ok(())
    }

    async fn exchange(
        &self,
        query: &BTreeMap<String, String>,
        registration: &str,
        mut claims: Value,
        mut response: Value,
    ) -> TestResult {
        if claims.get("nonce").is_none() {
            claims["nonce"] = json!(query["nonce"]);
        }
        response["id_token"] = json!(self.sign(registration, claims)?);
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .up_to_n_times(1)
            .expect(1)
            .mount(&self.server)
            .await;
        Ok(())
    }

    fn sign(&self, registration: &str, overrides: Value) -> Result<String, Box<dyn Error>> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut claims = json!({"iss":self.server.uri(),"sub":"user-one","aud":registration,"exp":now+3600,"iat":now,"email":"same@example.test"});
        for (key, value) in overrides.as_object().ok_or("claims object")? {
            claims[key] = value.clone();
        }
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test".into());
        Ok(encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_der(include_bytes!("fixtures/test-key.der")),
        )?)
    }

    async fn refresh(&self, response: Value, delay: Duration) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=refresh-initial"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(response)
                    .set_delay(delay),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(&self.server)
            .await;
    }

    async fn gated_refresh(&self, response: Value) -> RefreshGate {
        let requested = std::sync::Arc::new(tokio::sync::Notify::new());
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let released = std::sync::Mutex::new(released);
        let observed = requested.clone();
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=refresh-initial"))
            .respond_with(move |_: &wiremock::Request| {
                observed.notify_one();
                // Wiremock owns a separate server thread; only the test releases this response.
                assert!(
                    released.lock().is_ok_and(|released| released
                        .recv_timeout(Duration::from_secs(10))
                        .is_ok()),
                    "refresh response was not released"
                );
                ResponseTemplate::new(200).set_body_json(&response)
            })
            .up_to_n_times(1)
            .expect(1)
            .mount(&self.server)
            .await;
        RefreshGate { requested, release }
    }

    async fn wait_for_refresh(&self) -> TestResult {
        for _ in 0..200 {
            if self
                .server
                .received_requests()
                .await
                .ok_or("missing requests")?
                .iter()
                .any(|request| {
                    String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token")
                })
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Err("refresh did not start".into())
    }
}

struct RefreshGate {
    requested: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::mpsc::SyncSender<()>,
}

fn assert_process_worker_completed(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line == "test process_worker ... ok"),
        "child did not complete process_worker: {stdout}"
    );
}

async fn wait_for_file(path: &std::path::Path) -> TestResult {
    for _ in 0..500 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("process signal did not arrive".into())
}

fn service(issuer: &str) -> Service {
    Service {
        issuer: issuer.into(),
        resource: "https://inference.example.test/v1".into(),
        scopes: "openid offline_access use.direct".into(),
        inference_scope: "use.direct".into(),
        registration_client: "registration_entrypoint".into(),
        registration_parameters: [("agent_name_hint".into(), "Fixture".into())].into(),
        host_id_parameter: Some("host".into()),
    }
}

fn parameters(login: &BrowserLogin) -> Result<BTreeMap<String, String>, Box<dyn Error>> {
    Ok(Url::parse(login.authorization_url().expose_secret())?
        .query_pairs()
        .into_owned()
        .collect())
}

async fn callback(query: &BTreeMap<String, String>, registration: &str) -> TestResult {
    let mut redirect = Url::parse(&query["redirect_uri"])?;
    redirect.query_pairs_mut().extend_pairs([
        ("state", query["state"].as_str()),
        ("code", "one-use-code"),
        ("client_id", registration),
    ]);
    assert_eq!(reqwest::get(redirect).await?.status().as_u16(), 200);
    Ok(())
}

async fn reset_callback(redirect: &Url) -> TestResult {
    use tokio::{io::AsyncWriteExt, net::TcpSocket};

    let port = redirect.port().ok_or("missing callback port")?;
    let socket = TcpSocket::new_v4()?;
    // Zero linger sends a reset without the blocking drop of a positive timeout.
    #[allow(deprecated)]
    socket.set_linger(Some(Duration::ZERO))?;
    let mut stream = socket.connect(([127, 0, 0, 1], port).into()).await?;
    let target = &redirect[url::Position::BeforePath..url::Position::AfterQuery];
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
        .await?;
    drop(stream);
    Ok(())
}

fn grant(expires: u64, scope: &str) -> Value {
    json!({"access_token":"access-initial","refresh_token":"refresh-initial","token_type":"Bearer","expires_in":expires,"scope":scope})
}

fn grant_rotated() -> Value {
    json!({"access_token":"access-rotated","refresh_token":"refresh-rotated","token_type":"Bearer","expires_in":3600})
}
