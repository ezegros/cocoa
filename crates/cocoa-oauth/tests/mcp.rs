//! Generic MCP OAuth shares subscription security machinery without requiring OIDC.

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use cocoa_oauth::{OAuthError, mcp::McpOAuthClient};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, error::Error, time::Duration};
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

#[tokio::test]
async fn login_token_lifetime_includes_time_waiting_to_persist() -> Result<(), Box<dyn Error>> {
    let remote = MockServer::start().await;
    let resource = format!("{}/mcp", remote.uri());
    Mock::given(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": resource, "authorization_servers": [remote.uri()]
        })))
        .mount(&remote)
        .await;
    Mock::given(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": remote.uri(),
            "authorization_endpoint": format!("{}/authorize", remote.uri()),
            "token_endpoint": format!("{}/token", remote.uri()),
            "code_challenge_methods_supported": ["S256"]
        })))
        .mount(&remote)
        .await;
    let response_sent = std::sync::Arc::new(tokio::sync::Notify::new());
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with({
            let response_sent = response_sent.clone();
            move |_: &Request| {
                response_sent.notify_one();
                ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": "short-lived", "refresh_token": "refresh-one",
                    "token_type": "Bearer", "expires_in": 35
                }))
            }
        })
        .mount(&remote)
        .await;
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh-one"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "renewed", "refresh_token": "refresh-two",
            "token_type": "Bearer", "expires_in": 3600
        })))
        .mount(&remote)
        .await;
    let directory = tempfile::tempdir()?;
    let store = directory.path().join("oauth/mcp.json");
    let client = McpOAuthClient::new(&resource, store.clone(), Some("public-client".into()))?;
    let login = client.begin(None).await?;
    let query: BTreeMap<_, _> = Url::parse(login.authorization_url().expose_secret())?
        .query_pairs()
        .into_owned()
        .collect();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.with_extension("json.lock"))?;
    lock.try_lock()?;
    let completion = tokio::spawn(login.finish(CancellationToken::new()));
    let mut callback = Url::parse(&query["redirect_uri"])?;
    callback
        .query_pairs_mut()
        .extend_pairs([("state", query["state"].as_str()), ("code", "one-use-code")]);
    assert_eq!(reqwest::get(callback).await?.status().as_u16(), 200);
    tokio::time::timeout(Duration::from_secs(5), response_sent.notified()).await?;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        !completion.is_finished(),
        "login must wait for the store lock"
    );
    drop(lock);
    tokio::time::timeout(Duration::from_secs(5), completion).await???;
    let token = client.access_token().await?.ok_or("missing grant")?;
    assert_eq!(token.expose_secret(), "renewed");
    Ok(())
}

#[tokio::test]
async fn resource_pkce_callback_and_refresh_are_isolated_and_cancel_safe()
-> Result<(), Box<dyn Error>> {
    let scenario = async {
        let remote = MockServer::start().await;
        let resource = format!("{}/mcp", remote.uri());
        Mock::given(path("/.well-known/oauth-protected-resource/mcp")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"resource":resource,"authorization_servers":[remote.uri()],"scopes_supported":["tools:read"]}))).mount(&remote).await;
        Mock::given(path("/.well-known/oauth-authorization-server")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"issuer":remote.uri(),"authorization_endpoint":format!("{}/authorize",remote.uri()),"token_endpoint":format!("{}/token",remote.uri()),"registration_endpoint":format!("{}/register",remote.uri()),"code_challenge_methods_supported":["S256"]}))).mount(&remote).await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(|request: &Request| {
                let Ok(body) = request.body_json::<Value>() else {
                    return ResponseTemplate::new(400);
                };
                assert_eq!(body["token_endpoint_auth_method"], "none");
                assert_eq!(body["client_name"], "Cocoa");
                ResponseTemplate::new(201).set_body_json(json!({"client_id":"public-mcp-client"}))
            })
            .expect(1)
            .mount(&remote)
            .await;
        let directory = tempfile::tempdir()?;
        let store = directory.path().join("oauth/mcp.json");
        let client = McpOAuthClient::new(&resource, store.clone(), None)?;
        assert!(client.access_token().await?.is_none());
        let login = client.begin(None).await?;
        let authorization = Url::parse(login.authorization_url().expose_secret())?;
        let query: BTreeMap<_, _> = authorization
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(query["resource"], resource);
        assert_eq!(query["scope"], "tools:read");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["client_id"], "public-mcp-client");
        let challenge = query["code_challenge"].clone();
        let expected_resource = resource.clone();
        let redirect = query["redirect_uri"].clone();
        let expected_redirect = redirect.clone();
        Mock::given(path("/token")).and(body_string_contains("grant_type=authorization_code")).respond_with(move |request: &Request| {
        let form: BTreeMap<_,_> = url::form_urlencoded::parse(&request.body).into_owned().collect();
        assert_eq!(form["resource"],expected_resource);
        assert_eq!(form["redirect_uri"],expected_redirect);
        assert_eq!(form["client_id"],"public-mcp-client");
        assert_eq!(form["code"],"one-use-code");
        assert_eq!(BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())),challenge);
        ResponseTemplate::new(200).set_body_json(json!({"access_token":"initial-mcp-secret","refresh_token":"refresh-one","token_type":"Bearer","expires_in":1}))
    }).expect(1).mount(&remote).await;
        let completion = tokio::spawn(login.finish(CancellationToken::new()));
        let http = reqwest::Client::new();
        let mut callback = Url::parse(&redirect)?;
        callback
            .query_pairs_mut()
            .extend_pairs([("state", "wrong"), ("code", "one-use-code")]);
        assert_eq!(
            http.get(callback.clone()).send().await?.status().as_u16(),
            400
        );
        callback.set_query(None);
        callback
            .query_pairs_mut()
            .extend_pairs([("state", query["state"].as_str()), ("code", "one-use-code")]);
        assert_eq!(http.get(callback).send().await?.status().as_u16(), 200);
        completion.await??;
        let initial: Value = serde_json::from_slice(&std::fs::read(&store)?)?;
        assert_eq!(
            initial["registration"]["tokens"]["access_token"],
            "initial-mcp-secret"
        );
        let expected_resource = resource.clone();
        let requested = std::sync::Arc::new(tokio::sync::Notify::new());
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let released = std::sync::Mutex::new(released);
        let observed = requested.clone();
        Mock::given(path("/token")).and(body_string_contains("grant_type=refresh_token")).respond_with(move |request: &Request| {
        let form: BTreeMap<_,_> = url::form_urlencoded::parse(&request.body).into_owned().collect();
        assert_eq!(form["resource"],expected_resource);
        assert_eq!(form["refresh_token"],"refresh-one");
        observed.notify_one();
        // Wiremock owns a separate server thread; the test releases its response after cancellation.
        assert!(released.lock().is_ok_and(|released| released.recv_timeout(Duration::from_secs(10)).is_ok()), "refresh response was not released");
        ResponseTemplate::new(200).set_body_json(json!({"access_token":"rotated-mcp-secret","refresh_token":"refresh-two","token_type":"Bearer","expires_in":3600}))
    }).expect(1).mount(&remote).await;
        let waiter = tokio::spawn({
            let client = client.clone();
            async move { client.access_token().await }
        });
        requested.notified().await;
        assert!(
            !waiter.is_finished(),
            "refresh must wait for response release"
        );
        waiter.abort();
        assert!(waiter.await.is_err_and(|error| error.is_cancelled()));
        release.send(())?;
        assert_eq!(
            client
                .access_token()
                .await?
                .ok_or("no token")?
                .expose_secret(),
            "rotated-mcp-secret"
        );
        let rotated: Value = serde_json::from_slice(&std::fs::read(&store)?)?;
        assert_eq!(
            rotated["registration"]["tokens"]["refresh_token"],
            "refresh-two"
        );
        let wrong_resource =
            McpOAuthClient::new(&format!("{}/other", remote.uri()), store.clone(), None)?;
        assert!(matches!(
            wrong_resource.access_token().await,
            Err(OAuthError::Changed)
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&store)?.permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(store.parent().ok_or("no parent")?)?
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        client.logout().await?;
        assert!(client.access_token().await?.is_none());
        remote.verify().await;
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(15), scenario).await?
}

#[tokio::test]
async fn cancelled_login_and_mismatched_discovery_never_save_tokens() -> Result<(), Box<dyn Error>>
{
    let remote = MockServer::start().await;
    Mock::given(path("/bad-resource")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"resource":"https://unrelated.example/mcp","authorization_servers":[remote.uri()]}))).mount(&remote).await;
    let directory = tempfile::tempdir()?;
    let store = directory.path().join("oauth/mcp.json");
    let client = McpOAuthClient::new(
        &format!("{}/mcp", remote.uri()),
        store.clone(),
        Some("pre-registered".into()),
    )?;
    assert!(matches!(
        client
            .begin(Some(&format!("{}/bad-resource", remote.uri())))
            .await,
        Err(OAuthError::Configuration)
    ));
    assert!(!store.exists());
    Mock::given(path("/resource")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"resource":format!("{}/mcp",remote.uri()),"authorization_servers":[remote.uri()]}))).mount(&remote).await;
    Mock::given(path("/.well-known/oauth-authorization-server")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"issuer":remote.uri(),"authorization_endpoint":format!("{}/authorize",remote.uri()),"token_endpoint":format!("{}/token",remote.uri()),"code_challenge_methods_supported":["S256"]}))).mount(&remote).await;
    let login = client
        .begin(Some(&format!("{}/resource", remote.uri())))
        .await?;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        login.finish(cancellation).await,
        Err(OAuthError::Cancelled)
    ));
    assert!(client.access_token().await?.is_none());
    assert!(
        remote
            .received_requests()
            .await
            .ok_or("no requests")?
            .iter()
            .all(|request| request.url.path() != "/register" && request.url.path() != "/token")
    );
    Ok(())
}
