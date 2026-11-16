//! HTTP MCP and generated JavaScript contracts through public connection/engine APIs.

#[path = "support/process.rs"]
mod process;

use process::CommandExt;
use std::{collections::BTreeMap, error::Error, sync::Arc, time::Duration};

use async_trait::async_trait;
use cocoa::mcp::{McpError, McpManager, SearchProvider, Server};
use cocoa_core::{
    capability::{
        CapabilityBinding, CapabilityDescription, CapabilityError, CapabilityHost,
        CapabilityObserver,
    },
    context::BoundedContext,
    engine::{Engine, EngineAdapters, EngineSettings, Limits, RunRequest},
    execution::{Cell, CellResponse, WorkLimits},
    model::{ModelAction, ModelError, ModelProvider, ModelRequest, ModelResponse},
    store::{InMemoryStore, RecordId, RecordKind, Store, StoreLimits},
};
use cocoa_quickjs::{QuickJsExecutor, QuickJsOptions};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

#[cfg(unix)]
#[path = "support/terminal.rs"]
mod terminal;

#[test]
fn server_urls_allow_https_and_literal_loopback_http_only() -> Result<(), Box<dyn Error>> {
    for url in [
        "https://example.com/mcp",
        "http://127.0.0.1:8080/mcp",
        "http://127.23.45.67/mcp",
        "http://[::1]:8080/mcp",
        "http://[0:0:0:0:0:0:0:1]/mcp",
    ] {
        let server = Server::new(url, None)?;
        assert_eq!(server.url(), url);
        let restored: Server = serde_json::from_value(serde_json::to_value(&server)?)?;
        assert_eq!(restored, server);
    }
    for url in [
        "http://localhost/mcp",
        "http://example.com/mcp",
        "http://127.0.0.1.example.com/mcp",
        "http://192.168.1.1/mcp",
        "http://0.0.0.0/mcp",
        "http://[::]/mcp",
        "http://[::2]/mcp",
        "http://[2001:db8::1]/mcp",
        "http://[::ffff:127.0.0.1]/mcp",
        "ftp://127.0.0.1/mcp",
        "https://user:password@example.com/mcp",
        "http://user:password@[::1]/mcp",
        "http://[::1]/mcp?token=private",
        "http://[::1]/mcp#fragment",
    ] {
        assert!(matches!(
            Server::new(url, None),
            Err(McpError::Configuration)
        ));
    }
    Ok(())
}

#[test]
fn cli_presets_are_offline_defaults_and_save_independent_grants() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let run = |arguments: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .env("HOME", home.path())
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .output_bounded()
    };
    let output = run(&["mcp", "presets", "--json"])?;
    assert!(output.status.success());
    let presets: Vec<Value> = serde_json::from_slice(&output.stdout)?;
    assert_eq!(presets.len(), 25);
    assert!(!home.path().join(".config/cocoa").exists());
    let mut names = std::collections::BTreeSet::new();
    for preset in &presets {
        assert!(names.insert(preset["name"].as_str().ok_or("missing preset name")?));
        Server::new(preset["url"].as_str().ok_or("missing preset URL")?, None)?;
        for field in ["title", "authentication", "setup", "documentation"] {
            assert!(
                !preset[field]
                    .as_str()
                    .ok_or("missing preset metadata")?
                    .is_empty()
            );
        }
    }
    assert!(run(&["mcp", "presets"])?.status.success());
    let output = run(&["mcp", "add", "unknown", "--no-connect"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown MCP preset"));
    assert!(!home.path().join(".config/cocoa/config.json").exists());

    for (name, preset, expected_url) in [
        ("notion", None, "https://mcp.notion.com/mcp"),
        ("work-docs", Some("notion"), "https://mcp.notion.com/mcp"),
        ("github", None, "https://api.githubcopilot.com/mcp/"),
        ("vercel", None, "https://mcp.vercel.com"),
        ("paypal", None, "https://mcp.paypal.com/http"),
    ] {
        let mut arguments = vec!["mcp", "add", name, "--no-connect"];
        if let Some(preset) = preset {
            arguments.extend(["--preset", preset]);
        }
        let output = run(&arguments)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let settings: Value = serde_json::from_slice(&std::fs::read(
            home.path().join(".config/cocoa/config.json"),
        )?)?;
        assert_eq!(settings["mcp_servers"][name]["url"], expected_url);
        assert!(settings.get("web_search").is_none());
    }
    let path = home.path().join(".config/cocoa/config.json");
    let saved = std::fs::read(&path)?;
    assert!(
        !run(&["mcp", "add", "notion", "--no-connect"])?
            .status
            .success()
    );
    assert!(
        !run(&[
            "mcp",
            "add",
            "other",
            "https://example.com/mcp",
            "--preset",
            "notion",
            "--no-connect"
        ])?
        .status
        .success()
    );
    assert_eq!(std::fs::read(&path)?, saved);
    assert!(
        run(&[
            "mcp",
            "add",
            "slack",
            "https://example.com/custom",
            "--no-connect"
        ])?
        .status
        .success()
    );
    let settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(
        settings["mcp_servers"]["slack"]["url"],
        "https://example.com/custom"
    );
    assert_ne!(
        settings["mcp_servers"]["notion"]["id"],
        settings["mcp_servers"]["work-docs"]["id"]
    );
    assert!(!home.path().join(".config/cocoa/oauth").exists());
    std::fs::write(&path, b"invalid config")?;
    assert!(run(&["mcp", "presets", "--json"])?.status.success());
    assert_eq!(std::fs::read(&path)?, b"invalid config");
    Ok(())
}

#[tokio::test]
async fn cli_persists_grants_without_a_provider_and_preserves_configuration()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let remote = MockServer::start().await;
    Mock::given(method("POST")).respond_with(|request: &Request| {
        let Ok(body) = request.body_json::<Value>() else { return ResponseTemplate::new(400) };
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":{"tools":[{"name":"search","inputSchema":{"type":"object"}}]}}))
    }).mount(&remote).await;
    let url = format!("{}/mcp", remote.uri());
    tokio::task::spawn_blocking(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let home = tempfile::tempdir()?;
        let run = |arguments: &[&str]| {
            std::process::Command::new(env!("CARGO_BIN_EXE_cocoa"))
                .env_clear()
                .env("HOME", home.path())
                .args(arguments)
                .stdin(std::process::Stdio::null())
                .output_bounded()
        };
        let output = run(&["mcp", "add", "docs", &url])?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Connected · 1 tools"));
        let path = home.path().join(".config/cocoa/config.json");
        let before = std::fs::read(&path)?;
        assert!(!run(&["mcp", "add", "docs", &url])?.status.success());
        assert_eq!(std::fs::read(&path)?, before);
        let output = run(&["mcp", "tools", "docs"])?;
        assert!(output.status.success());
        let tools: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(tools[0]["callable"], "mcp.docs.search");
        let before = std::fs::read(&path)?;
        assert!(!run(&["search", "mcp", "missing"])?.status.success());
        assert_eq!(std::fs::read(&path)?, before);
        for provider in ["disabled", "exa"] {
            assert!(run(&["search", provider])?.status.success());
            let settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            assert_eq!(settings["web_search"], json!({"provider":provider}));
        }
        assert!(run(&["search", "mcp", "docs"])?.status.success());
        assert!(run(&["mcp", "disable", "docs"])?.status.success());
        let before = std::fs::read(&path)?;
        assert!(!run(&["search", "mcp", "docs"])?.status.success());
        assert_eq!(std::fs::read(&path)?, before);
        assert!(String::from_utf8_lossy(&run(&["mcp", "list"])?.stdout).contains("Disabled"));
        assert!(run(&["mcp", "logout", "docs"])?.status.success());
        assert!(
            run(&[
                "alias",
                "medium",
                "--provider",
                "ollama",
                "--model",
                "unused"
            ])?
            .status
            .success()
        );
        let settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(settings["mcp_servers"]["docs"]["url"], url);
        assert_eq!(settings["mcp_servers"]["docs"]["enabled"], false);
        assert_eq!(
            settings["web_search"],
            json!({"provider":"mcp","server":"docs"})
        );
        assert!(run(&["mcp", "enable", "docs"])?.status.success());
        assert!(run(&["mcp", "remove", "docs"])?.status.success());
        let settings: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert!(settings.get("mcp_servers").is_none());
        assert_eq!(settings["aliases"]["medium"][0]["model"], "unused");
        assert_eq!(
            settings["web_search"],
            json!({"provider":"mcp","server":"docs"})
        );
        assert!(run(&["search", "exa"])?.status.success());
        assert!(
            !run(&["mcp", "add", "bad", "http://untrusted.example/mcp"])?
                .status
                .success()
        );
        Ok(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn cli_uses_environment_headers_without_persisting_keys_or_opening_oauth()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let remote = MockServer::start().await;
    Mock::given(method("POST")).respond_with(|request: &Request| {
        let Ok(body) = request.body_json::<Value>() else { return ResponseTemplate::new(400) };
        let accepted = match request.url.path() {
            "/exa" => request.headers.get("x-api-key").is_some_and(|value| value == "fixture-exa-key"),
            "/firecrawl" => request.headers.get("authorization").is_some_and(|value| value == "Bearer fixture-firecrawl-key"),
            "/unauthorized" => return ResponseTemplate::new(401),
            _ => false,
        };
        if !accepted { return ResponseTemplate::new(401) }
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":{"tools":[{"name":"search","inputSchema":{"type":"object"}}]}}))
    }).mount(&remote).await;
    let endpoint = remote.uri();
    tokio::task::spawn_blocking(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let home = tempfile::tempdir()?;
        let run = |arguments: &[&str]| {
            std::process::Command::new(env!("CARGO_BIN_EXE_cocoa"))
                .env_clear()
                .env("HOME", home.path())
                .env("EXA_KEY", "fixture-exa-key")
                .env("FIRECRAWL_AUTH", "Bearer fixture-firecrawl-key")
                .env("INVALID_KEY", "invalid\nheader")
                .args(arguments)
                .stdin(std::process::Stdio::null())
                .output_bounded()
        };
        for (name, header) in [
            ("exa", "x-api-key=EXA_KEY"),
            ("firecrawl", "Authorization=FIRECRAWL_AUTH"),
        ] {
            let url = format!("{endpoint}/{name}");
            let output = run(&["mcp", "add", name, &url, "--header-env", header])?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("Connected · 1 tools"));
            assert!(run(&["mcp", "tools", name])?.status.success());
            let settings = std::fs::read_to_string(home.path().join(".config/cocoa/config.json"))?;
            assert!(!settings.contains("fixture-exa-key"));
            assert!(!settings.contains("fixture-firecrawl-key"));
            assert!(!run(&["mcp", "auth", name])?.status.success());
            assert!(!home.path().join(".config/cocoa/oauth").exists());
        }
        let url = format!("{endpoint}/exa");
        for (name, header) in [
            ("missing", "x-api-key=MISSING_KEY"),
            ("invalid", "x-api-key=INVALID_KEY"),
        ] {
            let output = run(&["mcp", "add", name, &url, "--header-env", header])?;
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("header credentials"));
        }
        let output = run(&[
            "mcp",
            "add",
            "rejected",
            &format!("{endpoint}/unauthorized"),
            "--header-env",
            "x-api-key=EXA_KEY",
        ])?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("HTTP 401"));
        assert!(!home.path().join(".config/cocoa/oauth").exists());
        let saved = std::fs::read(home.path().join(".config/cocoa/config.json"))?;
        let output = run(&["mcp", "add", "bad", &url, "--header-env", "Host=EXA_KEY"])?;
        assert!(!output.status.success());
        assert_eq!(
            std::fs::read(home.path().join(".config/cocoa/config.json"))?,
            saved
        );
        Ok(())
    })
    .await??;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn centered_mcp_menu_adds_filters_inspects_disables_and_removes_servers()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let remote = MockServer::start().await;
    Mock::given(method("POST")).respond_with(|request: &Request| {
        let Ok(body) = request.body_json::<Value>() else { return ResponseTemplate::new(400) };
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":{"tools":[{"name":"search_docs","description":"Search exact documents", "inputSchema":{"type":"object","properties":{"query":{"type":"string"}}}}]}}))
    }).mount(&remote).await;
    let url = format!("{}/mcp", remote.uri());
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let home = tempfile::tempdir()?;
        std::fs::create_dir_all(home.path().join(".config/cocoa"))?;
        std::fs::write(home.path().join(".config/cocoa/config.json"),serde_json::to_vec(&json!({"providers":{"ollama":{}},"aliases":{"medium":[{"provider":"ollama","model":"unused"}]},"default_alias":"medium"}))?)?;
        let mut session = terminal::Terminal::spawn(home.path(), &["mcp"],90,24,None)?;
        session.wait_text("+ Add server")?;
        session.wait_text("/ Filter")?;
        session.wait_text("↑↓ select · Enter choose · Esc back")?;
        session.key(b"\r")?;
        session.wait_text("Add server › 25 presets")?;
        session.wait_text("› Custom server")?;
        session.text("private-server")?;
        session.wait_text("/ private-server")?;
        session.wait_text("› Custom server")?;
        session.key(b"\r")?;
        session.wait_text("Add server › Name")?;
        session.command("discarded")?;
        session.wait_text("discarded › HTTP URL")?;
        session.key(b"\x1b")?;
        session.wait_text("Add server › Name")?;
        session.key(b"\x03")?;
        session.until(|session| !session.contains("Add server") && !session.contains("MCP servers"))?;
        session.command("/mcp")?;
        session.wait_text("MCP servers")?;
        session.key(b"\r")?;
        session.wait_text("Add server › 25 presets")?;
        session.text("NoTiOn")?;
        session.wait_text("Notion")?;
        session.until(|session| !session.contains("Airtable"))?;
        session.key(b"\r")?;
        session.wait_text("Add server › Name")?;
        session.wait_text("› notion")?;
        session.key(b"\r")?;
        session.wait_text("https://mcp.notion.com/mcp")?;
        session.wait_text("workspace/pages")?;
        session.key(b"\x1b")?;
        session.wait_text("Add server › Name")?;
        session.wait_text("› notion")?;
        let settings: Value = serde_json::from_slice(&std::fs::read(home.path().join(".config/cocoa/config.json"))?)?;
        assert!(settings.get("mcp_servers").is_none());
        session.key(b"\x15")?;
        session.command("docs")?;
        session.wait_text("docs › HTTP URL")?;
        session.wait_text("https://mcp.notion.com/mcp")?;
        session.key(b"\x15")?;
        session.command(&url)?;
        session.wait_text("Connected · 1 tools")?;
        let settings: Value = serde_json::from_slice(&std::fs::read(home.path().join(".config/cocoa/config.json"))?)?;
        assert_eq!(settings["mcp_servers"]["docs"]["url"], url);
        assert!(settings.get("mcp_servers").and_then(|servers| servers.get("notion")).is_none());
        session.key(b"\r")?;
        session.wait_text("search_docs")?;
        session.command("exact")?;
        session.wait_text("mcp.docs.search_docs")?;
        session.key(b"\x1b")?;
        session.key(b"\x1b")?;
        session.wait_text("MCP › docs")?;
        session.key(b"\x1b[H\x1b[B\x1b[B\x1b[B\r")?;
        session.wait_text("Disabled")?;
        session.key(b"\x1b")?;
        session.wait_text("MCP servers")?;
        session.text("missing")?;
        session.until(|session| session.contains("/ missing") && !session.screen().contains("› docs"))?;
        session.key(b"\x15")?;
        session.key(b"\r")?;
        session.wait_text("MCP › docs")?;
        session.key(b"\x1b[F\r")?;
        session.wait_text("Remove › docs")?;
        session.key(b"\r")?;
        session.wait_text("MCP servers")?;
        let settings: Value = serde_json::from_slice(&std::fs::read(home.path().join(".config/cocoa/config.json"))?)?;
        assert!(settings.get("mcp_servers").is_none());
        session.key(b"\x1b")?;
        session.command("/login chatgpt")?;
        session.wait_text("Subscriptions")?;
        session.wait_text("/ Filter")?;
        session.wait_text("+ Add subscription")?;
        session.wait_text("↑↓ select · Enter choose · Esc back")?;
        session.key(b"\r")?;
        session.wait_text("No API fallback.")?;
        session.key(b"\x03")?;
        session.until(|session| !session.contains("No API fallback.") && !session.contains("Subscriptions"))?;
        session.command("exit")?;
        session.finish(0)?;
        Ok(())
    }).await??;
    Ok(())
}

#[tokio::test]
async fn selected_search_uses_native_mcp_tools_and_never_falls_back_when_unavailable()
-> Result<(), Box<dyn Error>> {
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            if request.url.path() == "/failed" {
                return ResponseTemplate::new(503);
            }
            let Ok(body) = request.body_json::<Value>() else {
                return ResponseTemplate::new(400);
            };
            let result = match body["method"].as_str() {
                Some("tools/list") => json!({"tools":[{"name":"lookup","inputSchema":{"type":"object","required":["phrase"],"properties":{"phrase":{"type":"string"}}}}]}),
                Some("tools/call") => {
                    assert_eq!(body["params"]["arguments"], json!({"phrase":"native search"}));
                    json!({"content":[{"type":"text","text":request.url.path()}],"structuredContent":{"original":true},"isError":false})
                }
                _ => return ResponseTemplate::new(400),
            };
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
        })
        .mount(&remote)
        .await;
    let home = tempfile::tempdir()?;
    let manager = McpManager::new(home.path().join("credentials"))?;
    let host = manager.bind(Arc::new(Empty));
    manager.set_search_provider(SearchProvider::Mcp {
        server: "selected".into(),
    })?;
    for state in ["connected", "disabled", "failed", "missing"] {
        let other = Server::new(&format!("{}/other", remote.uri()), None)?;
        let mut servers = BTreeMap::from([("other".into(), other)]);
        if state != "missing" {
            let path = if state == "failed" {
                "failed"
            } else {
                "selected"
            };
            let mut selected = Server::new(&format!("{}/{path}", remote.uri()), None)?;
            if state == "disabled" {
                selected.set_enabled(false);
            }
            servers.insert("selected".into(), selected);
        }
        manager.configure(servers)?;
        manager.connect_all().await?;
        let descriptions = host.descriptions();
        assert!(
            !descriptions
                .iter()
                .any(|tool| tool.name == "cocoa.web_search")
        );
        let discovery = descriptions
            .iter()
            .find(|tool| tool.name == "cocoa.mcp_tools")
            .ok_or("missing discovery")?;
        assert!(
            discovery
                .description
                .contains("Selected web search provider is MCP server \"selected\"")
        );
        assert!(matches!(
            host.call(
                "cocoa.web_search",
                json!({"query":"no fallback"}),
                limits(),
                CancellationToken::new()
            )
            .await,
            Err(CapabilityError::Rejected { .. })
        ));
        let tools = host
            .call(
                "cocoa.mcp_tools",
                json!({"server":"selected"}),
                limits(),
                CancellationToken::new(),
            )
            .await?;
        assert_eq!(
            tools["tools"].as_array().ok_or("missing tools")?.len(),
            usize::from(state == "connected")
        );
        if state == "connected" {
            assert_eq!(
                tools["tools"][0]["inputSchema"]["required"],
                json!(["phrase"])
            );
        }
        for server in if state == "connected" {
            vec!["selected", "other"]
        } else {
            vec!["other"]
        } {
            let binding = host
                .bindings()
                .into_iter()
                .find(|binding| binding.path == ["mcp", server, "lookup"])
                .ok_or("missing native function")?;
            let result = host
                .call(
                    &binding.name,
                    json!({"phrase":"native search"}),
                    limits(),
                    CancellationToken::new(),
                )
                .await?;
            assert_eq!(
                result,
                json!({"content":[{"type":"text","text":format!("/{server}")}],"structuredContent":{"original":true},"isError":false})
            );
        }
    }
    manager.set_search_provider(SearchProvider::Disabled)?;
    assert!(
        !host
            .descriptions()
            .iter()
            .any(|tool| tool.description.contains("Selected web search provider"))
    );
    manager.set_search_provider(SearchProvider::Exa)?;
    assert!(
        host.descriptions()
            .iter()
            .any(|tool| tool.name == "cocoa.web_search")
    );
    Ok(())
}

#[tokio::test]
async fn generated_functions_preserve_names_results_and_variables_but_not_revoked_grants()
-> Result<(), Box<dyn Error>> {
    let remote = MockServer::start().await;
    Mock::given(method("POST")).and(path("/mcp")).respond_with(|request: &Request| {
        let Ok(body) = request.body_json::<Value>() else { return ResponseTemplate::new(400) };
        assert_eq!(request.headers["mcp-protocol-version"], "2026-07-28");
        assert_eq!(request.headers["accept"], "application/json, text/event-stream");
        assert_eq!(body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"], json!({}));
        assert!(request.headers.get("authorization").is_none());
        let result = match body["method"].as_str() {
            Some("tools/list") if body["params"].get("cursor").is_none() => json!({"tools":[{"name":"find-pages","description":"Find asymmetric pages", "inputSchema":{"type":"object","properties":{"tenant":{"type":"string","x-mcp-header":"Tenant"}}}}], "nextCursor":"second"}),
            Some("tools/list") => {
                assert_eq!(body["params"]["cursor"], "second");
                json!({"tools":[{"name":"__proto__","inputSchema":{"type":"object"}},{"name":"quote\".name","inputSchema":{"type":"object"}}]})
            }
            Some("tools/call") => {
                assert_eq!(body["params"]["name"], "find-pages");
                assert_eq!(request.headers["mcp-name"], "find-pages");
                assert_eq!(request.headers["mcp-param-tenant"], "=?base64?zrs=?=");
                assert_eq!(body["params"]["arguments"], json!({"tenant":"λ","query":"asymmetric"}));
                let result = json!({"content":[{"type":"text","text":"not found"}],"structuredContent":{"count":0},"isError":true,"_meta":{"kept":17}});
                return ResponseTemplate::new(200).set_body_raw(format!("data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{}}}}\r\n\r\ndata: {}\r\n\r\n", json!({"jsonrpc":"2.0","id":body["id"],"result":result})).into_bytes(), "text/event-stream");
            }
            _ => return ResponseTemplate::new(400),
        };
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
    }).mount(&remote).await;
    let directory = tempfile::tempdir()?;
    let manager = McpManager::new(directory.path().join("credentials"))?;
    let mut grant = Server::new(&format!("{}/mcp", remote.uri()), None)?;
    manager.configure(BTreeMap::from([("design-docs".into(), grant.clone())]))?;
    manager.connect("design-docs").await?;
    assert_eq!(manager.servers()?[0].status, "Connected · 3 tools");
    let host = manager.bind(Arc::new(Empty));
    assert!(
        host.descriptions()
            .iter()
            .any(|tool| tool.name == "cocoa.web_search")
    );
    manager.set_search_provider(SearchProvider::Disabled)?;
    assert!(
        !host
            .descriptions()
            .iter()
            .any(|tool| tool.name == "cocoa.web_search")
    );
    assert!(matches!(
        host.call(
            "cocoa.web_search",
            json!({"query":"disabled"}),
            limits(),
            CancellationToken::new()
        )
        .await,
        Err(CapabilityError::Rejected { .. })
    ));
    assert_eq!(
        host.descriptions().len(),
        1,
        "catalogues must not inflate every model request"
    );
    let discovery = host
        .call(
            "cocoa.mcp_tools",
            json!({"server":"design-docs","query":"asymmetric"}),
            limits(),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(
        discovery["tools"].as_array().ok_or("missing tools")?.len(),
        1
    );
    assert_eq!(
        discovery["tools"][0]["callable"],
        "mcp[\"design-docs\"][\"find-pages\"]"
    );
    let store = Arc::new(InMemoryStore::new(StoreLimits {
        record_bytes: 32768,
    })?);
    let mut engine = Engine::new(
        EngineSettings::new(engine_limits())?,
        EngineAdapters {
            model: Arc::new(Script),
            executor: Box::new(
                QuickJsExecutor::new(QuickJsOptions {
                    memory_bytes: 16 * 1024 * 1024,
                    stack_bytes: 256 * 1024,
                    feedback: cocoa_quickjs::FeedbackLimits {
                        output_bytes: 65536,
                        printed_bytes: 8192,
                        error_bytes: 2048,
                        print_calls: 32,
                    },
                })
                .await?,
            ),
            capabilities: host,
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    engine
        .run(
            RunRequest {
                input: "setup".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    let prints = printed(&store).await?;
    assert_eq!(
        prints,
        vec![json!([
            true,
            0,
            17,
            "not found",
            true,
            "function",
            ["find-pages", "__proto__", "quote\".name"]
        ])]
    );
    grant.set_enabled(false);
    manager.configure(BTreeMap::from([("design-docs".into(), grant.clone())]))?;
    engine
        .run(
            RunRequest {
                input: "revoked".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    let prints = printed(&store).await?;
    assert_eq!(prints[1], json!("revoked"));
    assert_eq!(prints[2], json!([37, []]));
    grant.set_enabled(true);
    let mut replacement = serde_json::to_value(&grant)?;
    replacement["url"] = format!("{}/replacement", remote.uri()).into();
    manager.configure(BTreeMap::from([(
        "design-docs".into(),
        serde_json::from_value(replacement)?,
    )]))?;
    manager.configure(BTreeMap::from([("design-docs".into(), grant)]))?;
    manager.connect("design-docs").await?;
    engine
        .run(
            RunRequest {
                input: "revoked".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    let prints = printed(&store).await?;
    assert_eq!(
        prints[3],
        json!("revoked"),
        "retained functions cannot regain access after reconfiguration"
    );
    assert_eq!(prints[4], json!([37, ["design-docs"]]));
    manager.configure(BTreeMap::new())?;
    manager.configure(BTreeMap::from([(
        "design-docs".into(),
        Server::new(&format!("{}/mcp", remote.uri()), None)?,
    )]))?;
    manager.connect("design-docs").await?;
    engine
        .run(
            RunRequest {
                input: "revoked".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    let prints = printed(&store).await?;
    assert_eq!(
        prints[5],
        json!("revoked"),
        "retained functions must not retarget a replacement grant"
    );
    assert_eq!(prints[6], json!([37, ["design-docs"]]));
    let requests = remote
        .received_requests()
        .await
        .ok_or("requests unavailable")?;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request
                .body_json::<Value>()
                .is_ok_and(|body| body["method"] == "tools/call"))
            .count(),
        1
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn legacy_handshake_uses_negotiated_session_and_never_replays_failed_tool_calls()
-> Result<(), Box<dyn Error>> {
    let remote = MockServer::start().await;
    Mock::given(method("POST")).respond_with(|request: &Request| {
        let Ok(body) = request.body_json::<Value>() else { return ResponseTemplate::new(400) };
        if request.headers["mcp-protocol-version"] == "2026-07-28" { return ResponseTemplate::new(400); }
        let result = match body["method"].as_str() {
            Some("initialize") => return ResponseTemplate::new(200).insert_header("mcp-session-id","session-opaque").set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}})),
            Some("notifications/initialized") => { assert_eq!(request.headers["mcp-session-id"],"session-opaque"); return ResponseTemplate::new(202); }
            Some("tools/list") => json!({"tools":[{"name":"mutate","inputSchema":{"type":"object"}}]}),
            Some("tools/call") => return ResponseTemplate::new(503).set_body_string("private-remote-diagnostic"),
            _ => return ResponseTemplate::new(400),
        };
        assert_eq!(request.headers["mcp-session-id"],"session-opaque");
        assert_eq!(request.headers["mcp-protocol-version"],"2025-06-18");
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
    }).mount(&remote).await;
    let directory = tempfile::tempdir()?;
    let manager = McpManager::new(directory.path().join("credentials"))?;
    manager.configure(BTreeMap::from([(
        "legacy".into(),
        Server::new(&remote.uri(), None)?,
    )]))?;
    manager.connect("legacy").await?;
    let host = manager.bind(Arc::new(Empty));
    let binding = host.bindings().into_iter().next().ok_or("no binding")?;
    let error = host
        .call(&binding.name, json!({}), limits(), CancellationToken::new())
        .await
        .err()
        .ok_or("expected failure")?;
    assert!(matches!(
        error,
        CapabilityError::Tool {
            effects: cocoa_core::capability::Effects::Unknown,
            ..
        }
    ));
    assert!(!error.to_string().contains("private-remote-diagnostic"));
    assert_eq!(
        remote
            .received_requests()
            .await
            .ok_or("no requests")?
            .iter()
            .filter(|request| request
                .body_json::<Value>()
                .is_ok_and(|body| body["method"] == "tools/call"))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn invalid_catalogues_and_auth_are_not_connected_and_zero_tools_are_valid()
-> Result<(), Box<dyn Error>> {
    let remote = MockServer::start().await;
    for (name, body, expected) in [
        ("empty", json!({"tools":[]}), "Connected · 0 tools"),
        (
            "duplicate",
            json!({"tools":[{"name":"a","inputSchema":{"type":"object"}},{"name":"a","inputSchema":{"type":"object"}}]}),
            "invalid or unsupported MCP response",
        ),
        (
            "unsafe_header",
            json!({"tools":[{"name":"a","inputSchema":{"type":"object","oneOf":[{"type":"object","properties":{"bad":{"type":"string","x-mcp-header":"Bad"}}}]}}]}),
            "invalid or unsupported MCP response",
        ),
    ] {
        Mock::given(path(format!("/{name}")))
            .respond_with(move |request: &Request| {
                let Ok(request) = request.body_json::<Value>() else {
                    return ResponseTemplate::new(400);
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc":"2.0","id":request["id"],"result":body}))
            })
            .mount(&remote)
            .await;
        let directory = tempfile::tempdir()?;
        let manager = McpManager::new(directory.path().join("credentials"))?;
        manager.configure(BTreeMap::from([(
            name.into(),
            Server::new(&format!("{}/{name}", remote.uri()), None)?,
        )]))?;
        let result = manager.connect(name).await;
        assert_eq!(result.is_ok(), name == "empty");
        assert_eq!(manager.servers()?[0].status, expected);
        assert!(manager.bind(Arc::new(Empty)).bindings().is_empty());
    }
    Mock::given(path("/auth"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!("Bearer resource_metadata=\"{}/metadata\"", remote.uri()),
        ))
        .mount(&remote)
        .await;
    let directory = tempfile::tempdir()?;
    let manager = McpManager::new(directory.path().join("credentials"))?;
    manager.configure(BTreeMap::from([(
        "auth".into(),
        Server::new(&format!("{}/auth", remote.uri()), None)?,
    )]))?;
    assert!(matches!(
        manager.connect("auth").await,
        Err(McpError::Auth { metadata: Some(_) })
    ));
    assert_eq!(manager.servers()?[0].status, "Auth required");
    Mock::given(path("/metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"resource":format!("{}/auth",remote.uri()),"authorization_servers":[remote.uri()]})))
        .mount(&remote).await;
    Mock::given(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issuer":remote.uri(),"authorization_endpoint":format!("{}/authorize",remote.uri()),"token_endpoint":format!("{}/token",remote.uri()),"registration_endpoint":format!("{}/register",remote.uri()),"code_challenge_methods_supported":["S256"]})))
        .mount(&remote).await;
    Mock::given(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"client_id":"fixture-mcp"})))
        .mount(&remote)
        .await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"fixture-resource-token","token_type":"Bearer","expires_in":3600})))
        .mount(&remote).await;
    Mock::given(path("/auth"))
        .and(wiremock::matchers::header("Authorization", "Bearer fixture-resource-token"))
        .respond_with(|request: &Request| {
            let Ok(body) = request.body_json::<Value>() else { return ResponseTemplate::new(400) };
            let result = if body["method"] == "tools/call" {
                json!({"content":[{"type":"text","text":"authenticated result"}],"structuredContent":{"account":"mcp-only"}})
            } else {
                json!({"tools":[{"name":"search","inputSchema":{"type":"object"}}]})
            };
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
        }).with_priority(1).mount(&remote).await;
    let login = manager.begin_auth("auth").await?;
    let authorization = reqwest::Url::parse(login.authorization_url().expose_secret())?;
    let parameters: BTreeMap<_, _> = authorization.query_pairs().into_owned().collect();
    let mut callback = reqwest::Url::parse(&parameters["redirect_uri"])?;
    callback.query_pairs_mut().extend_pairs([
        ("state", parameters["state"].as_str()),
        ("code", "fixture-code"),
    ]);
    let completion = tokio::spawn(login.finish(CancellationToken::new()));
    assert_eq!(
        reqwest::Client::new().get(callback).send().await?.status(),
        200
    );
    completion.await??;
    manager.connect("auth").await?;
    assert_eq!(manager.servers()?[0].status, "Connected · 1 tools");
    let host = manager.bind(Arc::new(Empty));
    let binding = host
        .bindings()
        .into_iter()
        .next()
        .ok_or("missing authenticated tool")?;
    assert_eq!(
        host.call(&binding.name, json!({}), limits(), CancellationToken::new())
            .await?["structuredContent"]["account"],
        "mcp-only"
    );
    manager.logout("auth").await?;
    assert!(
        host.call(&binding.name, json!({}), limits(), CancellationToken::new())
            .await
            .is_err()
    );
    assert!(matches!(
        manager.connect("auth").await,
        Err(McpError::Auth { .. })
    ));
    for url in [
        "http://example.com/mcp",
        "https://user:password@example.com/mcp",
        "https://example.com/mcp?token=private",
        "https://example.com/mcp#fragment",
    ] {
        assert!(Server::new(url, None).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn wrapped_local_capabilities_receive_their_committed_operation_identity()
-> Result<(), Box<dyn Error>> {
    struct LocalHost;

    #[async_trait]
    impl CapabilityHost for LocalHost {
        fn descriptions(&self) -> Vec<CapabilityDescription> {
            vec![CapabilityDescription {
                name: "fixture.evidence".into(),
                description: "Return the committed identity and supplied tag.".into(),
                parameters: json!({"type":"object","properties":{"tag":{"type":"string"}},"required":["tag"]}),
            }]
        }

        fn bindings(&self) -> Vec<CapabilityBinding> {
            vec![CapabilityBinding {
                path: vec!["mcp".into(), "local".into(), "evidence".into()],
                name: "fixture.evidence".into(),
            }]
        }

        async fn call(
            &self,
            _: &str,
            _: Value,
            _: WorkLimits,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            Err(CapabilityError::Rejected {
                message: "Local acceptance requires committed operation evidence".into(),
            })
        }

        async fn call_started(
            &self,
            name: &str,
            arguments: Value,
            _: WorkLimits,
            _: CancellationToken,
            _: CapabilityObserver,
            operation_record: RecordId,
        ) -> Result<Value, CapabilityError> {
            if name != "fixture.evidence" {
                return Err(CapabilityError::Rejected {
                    message: "not granted".into(),
                });
            }
            Ok(json!({"record":i64::from(operation_record).to_string(),"tag":arguments["tag"]}))
        }
    }

    struct LocalScript;

    #[async_trait]
    impl ModelProvider for LocalScript {
        async fn respond(
            &self,
            request: ModelRequest,
            _: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            let completed = request
                .context
                .records
                .iter()
                .any(|record| matches!(record.kind, RecordKind::CellCompleted { .. }));
            Ok(ModelResponse {
                action: if completed {
                    ModelAction::Final {
                        answer: "settled".into(),
                    }
                } else {
                    ModelAction::Execute {
                        cells: vec![Cell {
                            code: "print(await mcp.local.evidence({tag:'exact input'}))".into(),
                        }],
                    }
                },
                continuation: None,
                usage: None,
            })
        }
    }

    let directory = tempfile::tempdir()?;
    let manager = McpManager::new(directory.path().join("credentials"))?;
    let host = manager.bind(Arc::new(LocalHost));
    let bindings = host.bindings();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].path, ["mcp", "local", "evidence"]);
    assert_eq!(bindings[0].name, "fixture.evidence");

    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let Ok(body) = request.body_json::<Value>() else {
                return ResponseTemplate::new(400);
            };
            let result = if request.url.path() == "/failed" {
                json!({"tools":[{"name":"incomplete"}]})
            } else {
                json!({"tools":[{"name":"search","inputSchema":{"type":"object"}}]})
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
        })
        .mount(&remote)
        .await;
    let grant = Server::new(&format!("{}/mcp", remote.uri()), None)?;
    manager.configure(BTreeMap::from([
        ("remote".into(), grant.clone()),
        (
            "failed".into(),
            Server::new(&format!("{}/failed", remote.uri()), None)?,
        ),
    ]))?;
    assert_eq!(host.bindings().len(), 1);
    assert!(matches!(
        manager.connect("failed").await,
        Err(McpError::Protocol)
    ));
    let bindings = host.bindings();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].name, "fixture.evidence");

    manager.connect("remote").await?;
    let bindings = host.bindings();
    assert_eq!(bindings.len(), 2);
    assert_eq!(bindings[0].path, ["mcp", "local", "evidence"]);
    assert_eq!(bindings[0].name, "fixture.evidence");
    assert_eq!(bindings[1].path, ["mcp", "remote", "search"]);
    assert!(bindings[1].name.starts_with("mcp:"));

    let store = Arc::new(InMemoryStore::new(StoreLimits {
        record_bytes: 32768,
    })?);
    let mut engine = Engine::new(
        EngineSettings::new(engine_limits())?,
        EngineAdapters {
            model: Arc::new(LocalScript),
            executor: Box::new(
                QuickJsExecutor::new(QuickJsOptions {
                    memory_bytes: 16 * 1024 * 1024,
                    stack_bytes: 256 * 1024,
                    feedback: cocoa_quickjs::FeedbackLimits {
                        output_bytes: 65536,
                        printed_bytes: 8192,
                        error_bytes: 2048,
                        print_calls: 32,
                    },
                })
                .await?,
            ),
            capabilities: host.clone(),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    let result = engine
        .run(
            RunRequest {
                input: "accept local work".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(result.answer, "settled");
    let snapshot = store.load().await?;
    let operation = snapshot
        .records
        .iter()
        .find(|stored| {
            matches!(&stored.record.kind, RecordKind::OperationStarted { name, arguments, .. }
            if name == "fixture.evidence" && arguments == &json!({"tag":"exact input"}))
        })
        .ok_or("missing committed local operation")?;
    assert_eq!(
        printed(&store).await?,
        vec![json!({
            "record":i64::from(operation.id).to_string(),
            "tag":"exact input",
        })]
    );
    engine.shutdown().await?;

    let mut disabled = grant;
    disabled.set_enabled(false);
    manager.configure(BTreeMap::from([("remote".into(), disabled)]))?;
    let bindings = host.bindings();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].name, "fixture.evidence");
    manager.configure(BTreeMap::new())?;
    let bindings = host.bindings();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].name, "fixture.evidence");
    Ok(())
}

struct Empty;

#[async_trait]
impl CapabilityHost for Empty {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        Vec::new()
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: WorkLimits,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Err(CapabilityError::Rejected {
            message: "not granted".into(),
        })
    }
}

struct Script;

#[async_trait]
impl ModelProvider for Script {
    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let mut completed = false;
        let mut setup = false;
        for record in request.context.records.iter().rev() {
            match &record.kind {
                RecordKind::CellCompleted { .. } => completed = true,
                RecordKind::InputAccepted { input } => {
                    setup = input.text() == "setup";
                    break;
                }
                _ => {}
            }
        }
        Ok(ModelResponse {
            action: if completed {
                ModelAction::Final {
                    answer: "settled".into(),
                }
            } else {
                ModelAction::Execute { cells:vec![Cell { code:if setup {
            "var saved = 37; var retained = mcp['design-docs']['find-pages']; var result = await retained({tenant:'λ',query:'asymmetric'}); print([result.isError,result.structuredContent.count,result._meta.kept,result.content[0].text,Object.getPrototypeOf(mcp['design-docs']) === null,typeof mcp['design-docs'].__proto__,Object.keys(mcp['design-docs'])]);"
        } else {
            "try { await retained({tenant:'λ',query:'asymmetric'}); print('wrong'); } catch { print('revoked'); } print([saved,Object.keys(mcp)]);"
        }.into() }] }
            },
            continuation: None,
            usage: None,
        })
    }
}

fn limits() -> WorkLimits {
    WorkLimits {
        output_bytes: 16384,
        deadline: tokio::time::Instant::now() + Duration::from_secs(3),
    }
}

fn engine_limits() -> Limits {
    Limits {
        input_bytes: 4096,
        output_bytes: 16384,
        record_bytes: 32768,
        context_bytes: 512000,
        pending_operations: 4,
        stall_timeout: Duration::from_secs(3),
        settlement_timeout: Duration::from_secs(1),
        shutdown_timeout: Duration::from_secs(1),
    }
}

async fn printed(store: &InMemoryStore) -> Result<Vec<Value>, Box<dyn Error>> {
    let mut printed = Vec::new();
    for record in store.load().await?.into_records() {
        if let RecordKind::CellCompleted { outcome, .. } = record.kind {
            match outcome {
                CellResponse::Completed { printed: values }
                | CellResponse::Failed {
                    printed: values, ..
                } => printed.extend(values),
                other => return Err(format!("cell failed: {other:?}").into()),
            }
        }
    }
    Ok(printed)
}
