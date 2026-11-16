//! Local MCP CLI. Configuration persists even if connection or browser login fails.

use crate::config::{self, McpCommand};
use cocoa::mcp::{McpError, McpManager, Server, presets::PRESETS};
use tokio_util::sync::CancellationToken;

pub(crate) async fn run(command: McpCommand) -> Result<(), config::ConfigError> {
    if let McpCommand::Presets { json } = command {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(PRESETS).map_err(|_| McpError::Protocol)?
            );
        } else {
            for preset in PRESETS {
                println!(
                    "{}\t{}\t{}\n  {}\n  {}",
                    preset.name,
                    preset.url,
                    preset.authentication,
                    preset.setup,
                    preset.documentation
                );
            }
        }
        return Ok(());
    }
    let manager = config::mcp_manager()?;
    match command {
        McpCommand::Presets { .. } => unreachable!("presets do not load server configuration"),
        McpCommand::Add {
            name,
            url,
            preset,
            client_id,
            header_env,
            no_auth,
            no_connect,
        } => {
            let url = match url {
                Some(url) => url,
                None => {
                    let preset = PRESETS
                        .iter()
                        .find(|entry| entry.name == preset.as_deref().unwrap_or(&name))
                        .ok_or(McpError::Preset)?;
                    eprintln!("{}\n{}", preset.setup, preset.documentation);
                    preset.url.to_owned()
                }
            };
            let mut headers = std::collections::BTreeMap::new();
            for header in header_env {
                let (name, reference) = header.split_once('=').ok_or(McpError::Configuration)?;
                if headers
                    .insert(name.to_owned(), reference.to_owned())
                    .is_some()
                {
                    return Err(McpError::Configuration.into());
                }
            }
            let server = Server::new(&url, client_id)?.with_headers_env(headers)?;
            config::save_mcp(name.clone(), config::McpEdit::Add(server))?;
            eprintln!("Saved MCP server. Configuration remains saved if connection fails.");
            if no_connect {
                return Ok(());
            }
            manager.configure(config::mcp_servers()?)?;
            match manager.connect(&name).await {
                Err(McpError::Auth { .. }) if !no_auth => authenticate(&manager, &name).await?,
                Err(McpError::Auth { .. }) => {
                    eprintln!("Authentication required. Use cocoa mcp auth {name} or /mcp.")
                }
                result => result?,
            }
            print_status(&manager)?;
        }
        McpCommand::Auth { name } => {
            if let Err(error) = manager.connect(&name).await
                && !matches!(error, McpError::Auth { .. } | McpError::OAuth(_))
            {
                return Err(error.into());
            }
            authenticate(&manager, &name).await?;
            print_status(&manager)?;
        }
        McpCommand::List => {
            manager.connect_all().await?;
            print_status(&manager)?;
        }
        McpCommand::Tools { name } => {
            manager.connect(&name).await?;
            let server = manager
                .servers()?
                .into_iter()
                .find(|server| server.name == name)
                .ok_or(McpError::Unavailable)?;
            let tools: Vec<_> = server.tools.into_iter().map(|tool| {
                serde_json::json!({"callable":cocoa::mcp::callable(&name, &tool.name),"tool":tool})
            }).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&tools).map_err(|_| McpError::Protocol)?
            );
        }
        McpCommand::Remove { name } => {
            manager.logout(&name).await?;
            config::save_mcp(name, config::McpEdit::Remove)?;
            eprintln!(
                "Removed MCP grant and local credentials. Existing effects remain; remote OAuth consent is unchanged."
            );
        }
        command @ (McpCommand::Enable { .. } | McpCommand::Disable { .. }) => {
            let enabled = matches!(command, McpCommand::Enable { .. });
            let name = match command {
                McpCommand::Enable { name } | McpCommand::Disable { name } => name,
                _ => unreachable!(),
            };
            let mut servers = config::mcp_servers()?;
            let mut server = servers.remove(&name).ok_or(McpError::Unavailable)?;
            server.set_enabled(enabled);
            config::save_mcp(name, config::McpEdit::Replace(server))?;
            eprintln!("Saved MCP grant state.");
        }
        McpCommand::Logout { name } => {
            manager.logout(&name).await?;
            eprintln!(
                "Forgot saved MCP OAuth credentials. Environment headers and remote consent are unchanged."
            );
        }
    }
    Ok(())
}

async fn authenticate(manager: &McpManager, name: &str) -> Result<(), McpError> {
    let cancellation = CancellationToken::new();
    let authorize = async {
        let login = manager.begin_auth(name).await?;
        login.open_browser().await?;
        eprintln!("Complete MCP sign-in in your browser. Ctrl+C cancels.");
        login.finish(cancellation.clone()).await?;
        manager.connect(name).await
    };
    tokio::select! {
        biased;
        signal = tokio::signal::ctrl_c() => {
            cancellation.cancel();
            signal.map_err(|_| McpError::Signal)?;
            Err(cocoa_oauth::OAuthError::Cancelled.into())
        },
        result = authorize => result,
    }
}

fn print_status(manager: &McpManager) -> Result<(), McpError> {
    for server in manager.servers()? {
        println!("{}\t{}", server.name, server.status);
    }
    Ok(())
}
