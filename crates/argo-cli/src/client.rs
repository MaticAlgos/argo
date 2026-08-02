//! Daemon client used by the `argo` subcommands.
//!
//! Connects over the private Unix socket, auto-starting the daemon when it is not
//! yet running so a user never has to launch it manually.

use argo_core::error::{ArgoError, Result};
use argo_core::event::{RunEventKind, RunStatus};
use argo_core::ids::{AgentId, ConversationId, RunId};
use argo_core::session::SelectionChange;
use argo_core::{ArgoPaths, IPC_PROTOCOL_VERSION};
use argo_daemon::protocol::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// How long to wait for an auto-started daemon to begin listening.
const START_TIMEOUT_MS: u64 = 8_000;

/// How long a single request/response exchange may take.
///
/// Defends the terminal independently of the daemon's own deadline: if the daemon
/// wedges, `argo` must still return control rather than leaving the user with no
/// option but Ctrl-C.
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// How long to wait between streamed events before giving up.
///
/// This is an inactivity budget, not a total budget, so a long agentic turn that
/// keeps producing output is never cut short. Override with
/// `ARGO_STREAM_IDLE_TIMEOUT_MS`; `0` waits indefinitely.
fn stream_idle_timeout() -> Option<std::time::Duration> {
    let ms = std::env::var("ARGO_STREAM_IDLE_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(120_000);
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// A connected client session.
pub struct Client {
    writer: tokio::net::unix::OwnedWriteHalf,
    reader: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
}

impl Client {
    /// Connects, starting the daemon if it is not already listening.
    pub async fn connect(paths: &ArgoPaths) -> Result<Self> {
        match Self::try_connect(paths).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                if let Some(protocol) = argo_daemon::mismatched_daemon_protocol(&error) {
                    // A binary upgrade commonly leaves the previous detached
                    // daemon alive. Stop it through its own compatible handshake
                    // before starting this build.
                    argo_daemon::stop_older_daemon(paths, protocol, "argo-cli").await?;
                }
            }
        }

        spawn_daemon(paths)?;

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(START_TIMEOUT_MS);
        let mut last_error = None;
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            match Self::try_connect(paths).await {
                Ok(client) => return Ok(client),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error
            .unwrap_or_else(|| ArgoError::Process("the daemon did not start in time".into())))
    }

    async fn try_connect(paths: &ArgoPaths) -> Result<Self> {
        let stream = UnixStream::connect(paths.socket())
            .await
            .map_err(|e| ArgoError::Io(format!("connect to daemon: {e}")))?;
        let (read_half, writer) = stream.into_split();
        let mut client = Self {
            writer,
            reader: BufReader::new(read_half).lines(),
        };

        // Negotiate before issuing anything else, so a version mismatch surfaces
        // as one clear message rather than a confusing later failure.
        match client
            .request(Request::Hello {
                protocol: IPC_PROTOCOL_VERSION,
                client: format!("argo-cli/{}", env!("CARGO_PKG_VERSION")),
            })
            .await?
        {
            Response::Welcome { .. } => Ok(client),
            Response::Error {
                code,
                message,
                retryable,
            } => Err(ArgoError::remote(code, message, retryable)),
            other => Err(ArgoError::Protocol(format!(
                "unexpected handshake reply: {other:?}"
            ))),
        }
    }

    /// Sends a request and reads one reply.
    pub async fn request(&mut self, request: Request) -> Result<Response> {
        self.request_within(
            request,
            Some(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS)),
        )
        .await
    }

    /// Sends a request with a caller-selected reply budget.
    async fn request_within(
        &mut self,
        request: Request,
        budget: Option<std::time::Duration>,
    ) -> Result<Response> {
        let line = serde_json::to_string(&request)?;
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| ArgoError::Io(format!("send request: {e}")))?;
        self.next_response_within(budget).await
    }

    /// Reads the next reply, waiting at most `budget`.
    pub async fn next_response_within(
        &mut self,
        budget: Option<std::time::Duration>,
    ) -> Result<Response> {
        let read = self.reader.next_line();
        let line = match budget {
            Some(budget) => tokio::time::timeout(budget, read)
                .await
                .map_err(|_| ArgoError::Timeout(budget.as_millis() as u64))?,
            None => read.await,
        }
        .map_err(|e| ArgoError::Io(format!("read reply: {e}")))?
        .ok_or_else(|| ArgoError::Protocol("daemon closed the connection".into()))?;
        serde_json::from_str(&line)
            .map_err(|e| ArgoError::Protocol(format!("malformed reply: {e}")))
    }
}

/// Starts the daemon as a detached background process.
fn spawn_daemon(paths: &ArgoPaths) -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| ArgoError::Process(format!("locate the argo executable: {e}")))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--data-dir")
        .arg(paths.root())
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // Detach so the daemon outlives this short-lived CLI invocation.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command
        .spawn()
        .map_err(|e| ArgoError::Process(format!("start the daemon: {e}")))?;
    Ok(())
}

/// Prints environment and adapter status.
pub async fn doctor(paths: &ArgoPaths) -> Result<()> {
    println!("argo {}", env!("CARGO_PKG_VERSION"));
    println!("  data dir : {}", paths.root().display());
    println!("  database : {}", paths.database().display());
    println!("  socket   : {}", paths.socket().display());
    println!("  protocol : v{IPC_PROTOCOL_VERSION}");

    argo_runtime::validate()?;
    println!("  registry : {} adapters", argo_runtime::ADAPTERS.len());

    match Client::connect(paths).await {
        Ok(mut client) => {
            match client.request(Request::Ping).await? {
                Response::Ok => println!("  daemon   : running"),
                other => println!("  daemon   : unexpected reply {other:?}"),
            }
            print_agents(&mut client, false).await?;
        }
        Err(error) => {
            println!("  daemon   : not running ({error})");
            // Filesystem-only discovery keeps diagnostics side-effect free.
            println!();
            for info in argo_runtime::discover_all_lightweight() {
                print_agent_line(&info);
            }
        }
    }
    Ok(())
}

/// Checks for and, when requested, installs a newer published Argo build.
pub async fn update(check_only: bool, force: bool) -> Result<()> {
    let status = argo_runtime::update::check().await?;
    println!("current: v{}", status.current);
    println!("latest:  v{}", status.latest);

    if check_only {
        println!(
            "{}",
            if status.available() {
                "update available · run `argo update` to install it"
            } else {
                "Argo is up to date"
            }
        );
        return Ok(());
    }
    if !status.available() && !force {
        println!("Argo is already up to date");
        return Ok(());
    }

    if force && !status.available() {
        println!("reinstalling v{}…", status.latest);
    } else {
        println!("updating Argo to v{}…", status.latest);
    }
    argo_runtime::update::install_latest().await?;
    println!("update complete · restart Argo to use v{}", status.latest);
    Ok(())
}

/// Stops the daemon and removes only the installed Argo executable.
pub async fn uninstall(paths: &ArgoPaths) -> Result<()> {
    stop(paths).await?;
    let executable = argo_runtime::update::uninstall_current_executable()?;
    println!("uninstalled Argo from {}", executable.display());
    println!(
        "conversations and configuration were preserved in {}",
        paths.root().display()
    );
    Ok(())
}

/// Lists detected agents.
pub async fn agents(paths: &ArgoPaths, refresh: bool) -> Result<()> {
    let mut client = Client::connect(paths).await?;
    print_agents(&mut client, refresh).await
}

async fn print_agents(client: &mut Client, refresh: bool) -> Result<()> {
    match client.request(Request::ListAgents { refresh }).await? {
        Response::Agents { agents } => {
            for info in &agents {
                print_agent_line(info);
            }
            Ok(())
        }
        Response::Error {
            code,
            message,
            retryable,
        } => Err(ArgoError::remote(code, message, retryable)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

fn print_agent_line(info: &argo_runtime::AgentInfo) {
    let mark = if info.available { "✓" } else { "·" };
    let version = info.version.as_deref().unwrap_or("unknown version");
    println!("{mark} {:<12} {}", info.id, version);
    if info.available {
        let models: Vec<&str> = info.models.iter().map(|m| m.id.as_str()).take(6).collect();
        println!("    models: {}", models.join(", "));
    }
    for diagnostic in &info.diagnostics {
        println!("    note: {diagnostic}");
    }
}

/// Lists conversations.
pub async fn chats(paths: &ArgoPaths, root: Option<String>) -> Result<()> {
    let root = resolve_root(root)?;
    let mut client = Client::connect(paths).await?;
    match client.request(Request::ListConversations { root }).await? {
        Response::Conversations { conversations } => {
            if conversations.is_empty() {
                println!("no conversations yet");
            }
            for summary in conversations {
                let description = summary.description.clone();
                let title = summary.title.unwrap_or_else(|| "(untitled)".into());
                let agent = summary.selected_agent_id.unwrap_or_else(|| "-".into());
                println!(
                    "{}  {:<28} agent={} messages={} sessions=[{}]",
                    summary.id,
                    title,
                    agent,
                    summary.message_count,
                    summary.agents_with_sessions.join(",")
                );
                if let Some(description) = description.filter(|value| !value.trim().is_empty()) {
                    println!("  {}", description);
                }
            }
            Ok(())
        }
        Response::Error {
            code,
            message,
            retryable,
        } => Err(ArgoError::remote(code, message, retryable)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

/// Deletes stored conversations, either for one workspace or globally.
pub async fn clear_history(paths: &ArgoPaths, all: bool, root: Option<String>) -> Result<()> {
    let root = if all { None } else { Some(resolve_root(root)?) };
    let mut client = Client::connect(paths).await?;
    match client.request(Request::ClearConversations { root }).await? {
        Response::Cleared { count } => {
            println!("cleared {count} conversation(s)");
            Ok(())
        }
        Response::Error {
            code,
            message,
            retryable,
        } => Err(ArgoError::remote(code, message, retryable)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

/// Creates a conversation and prints its id.
pub async fn new_conversation(
    paths: &ArgoPaths,
    root: Option<String>,
    title: Option<String>,
) -> Result<()> {
    let root = resolve_root(root)?;
    let mut client = Client::connect(paths).await?;
    match client
        .request(Request::NewConversation { root, title })
        .await?
    {
        Response::Conversation { summary, .. } => {
            println!("{}", summary.id);
            Ok(())
        }
        Response::Error {
            code,
            message,
            retryable,
        } => Err(ArgoError::remote(code, message, retryable)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

/// Records the agent/model/reasoning applied to the next turn.
pub async fn select(
    paths: &ArgoPaths,
    conversation_id: &str,
    agent: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
) -> Result<()> {
    if agent.is_none() && model.is_none() && reasoning.is_none() {
        return Err(ArgoError::Invalid(
            "specify at least one of --agent, --model, or --reasoning".into(),
        ));
    }
    let mut client = Client::connect(paths).await?;
    match client
        .request(Request::Select {
            conversation_id: ConversationId::new(conversation_id),
            change: SelectionChange {
                agent_id: agent.map(AgentId::new),
                model,
                reasoning,
            },
        })
        .await?
    {
        Response::Conversation { summary, .. } => {
            println!(
                "agent={} model={} reasoning={}",
                summary.selected_agent_id.unwrap_or_else(|| "-".into()),
                summary.selected_model.unwrap_or_else(|| "default".into()),
                summary.selected_reasoning.unwrap_or_else(|| "-".into()),
            );
            println!("applies to the next message in this conversation");
            Ok(())
        }
        Response::Error {
            code,
            message,
            retryable,
        } => Err(ArgoError::remote(code, message, retryable)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

/// Sets or reports the execution mode.
pub async fn mode(paths: &ArgoPaths, conversation_id: &str, mode: Option<String>) -> Result<()> {
    let mut client = Client::connect(paths).await?;

    // With no value this is a query, so avoid writing anything.
    let request = match mode {
        Some(mode) => Request::SetMode {
            conversation_id: ConversationId::new(conversation_id),
            mode: Some(mode),
        },
        None => Request::GetConversation {
            conversation_id: ConversationId::new(conversation_id),
        },
    };

    match client.request(request).await? {
        Response::Conversation { summary, .. } => {
            let current = summary
                .selected_mode
                .as_deref()
                .and_then(argo_core::mode::AgentMode::parse)
                .unwrap_or_default();
            println!("mode: {} — {}", current.label(), current.detail());
            if let Some(agent) = summary.selected_agent_id.as_deref() {
                if let Some(def) = argo_runtime::find(agent) {
                    let available: Vec<&str> = def
                        .capabilities
                        .modes
                        .available()
                        .iter()
                        .map(|m| m.id())
                        .collect();
                    println!("{agent} supports: {}", available.join(", "));
                }
            }
            Ok(())
        }
        Response::Error {
            code,
            message,
            retryable,
        } => Err(ArgoError::remote(code, message, retryable)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

/// Path of the canonical MCP registry.
fn mcp_path(paths: &ArgoPaths) -> std::path::PathBuf {
    paths.root().join("mcp.json")
}

/// Lists configured MCP servers.
///
/// Runs in-process: the registry is a file, so this works with the daemon down.
pub async fn mcp_list(paths: &ArgoPaths) -> Result<()> {
    let registry = argo_resources::McpRegistry::load(&mcp_path(paths))?;
    if registry.servers.is_empty() {
        println!("no MCP servers configured");
        println!("add one:    argo mcp add <name> --url <endpoint>");
        println!("or import:  argo mcp import");
        return Ok(());
    }
    println!(
        "{} server(s), shared with every agent that supports MCP:",
        registry.servers.len()
    );
    for server in &registry.servers {
        let state = if server.enabled {
            "enabled "
        } else {
            "disabled"
        };
        let detail = match &server.transport {
            argo_resources::McpTransport::Local { command, .. } => command.join(" "),
            argo_resources::McpTransport::Remote { url, .. } => url.clone(),
        };
        println!("  {:<20} {state}  {detail}", server.name);
    }
    Ok(())
}

/// Adds an MCP server.
pub async fn mcp_add(
    paths: &ArgoPaths,
    name: String,
    url: Option<String>,
    headers: Vec<String>,
    command: Vec<String>,
) -> Result<()> {
    // A server is either remote or local; accepting both would be ambiguous.
    let transport = match (url, command.is_empty()) {
        (Some(url), true) => {
            let parsed = headers
                .iter()
                .filter_map(|raw| {
                    let (key, value) = raw.split_once(':')?;
                    Some((key.trim().to_string(), value.trim().to_string()))
                })
                .collect();
            argo_resources::McpTransport::Remote {
                url,
                headers: parsed,
            }
        }
        (None, false) => argo_resources::McpTransport::Local {
            command,
            environment: vec![],
        },
        (Some(_), false) => {
            return Err(ArgoError::Invalid(
                "give either --url or a command after `--`, not both".into(),
            ))
        }
        (None, true) => {
            return Err(ArgoError::Invalid(
                "give --url <endpoint> for a remote server, or `-- <command>` for a local one"
                    .into(),
            ))
        }
    };

    let path = mcp_path(paths);
    let mut registry = argo_resources::McpRegistry::load(&path)?;
    registry.upsert(argo_resources::McpServer {
        name: name.clone(),
        transport,
        enabled: true,
    })?;
    registry.save(&path)?;
    println!("added '{name}'");
    println!("every agent that supports MCP now receives it, including ones");
    println!("where it was never configured.");
    Ok(())
}

/// Imports servers from other agents' configs.
pub async fn mcp_import(paths: &ArgoPaths, yes: bool) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| ArgoError::Io("HOME is not set".into()))?;
    let found = argo_resources::discover_importable(&home);
    if found.is_empty() {
        println!("no MCP servers found in other agents' configs");
        return Ok(());
    }

    println!("found {} server(s):", found.len());
    for entry in &found {
        let detail = match &entry.server.transport {
            argo_resources::McpTransport::Local { command, .. } => command.join(" "),
            argo_resources::McpTransport::Remote { url, .. } => url.clone(),
        };
        println!(
            "  {:<20} {:<34} from {}",
            entry.server.name, detail, entry.source
        );
    }

    if !yes {
        println!();
        println!("re-run with --yes to add them");
        return Ok(());
    }

    let path = mcp_path(paths);
    let mut registry = argo_resources::McpRegistry::load(&path)?;
    let mut added = 0usize;
    for entry in found {
        registry.upsert(entry.server)?;
        added += 1;
    }
    registry.save(&path)?;
    println!("\nimported {added} server(s)");
    Ok(())
}

/// Logs in to an OAuth-protected MCP server.
///
/// One login serves every agent: the token is attached when the server is handed
/// to a CLI, so agents that cannot authenticate a server themselves still get it.
pub async fn mcp_login(paths: &ArgoPaths, name: &str) -> Result<()> {
    let registry = argo_resources::McpRegistry::load(&mcp_path(paths))?;
    let server = registry
        .servers
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| ArgoError::not_found("mcp server", name))?;

    let url = match &server.transport {
        argo_resources::McpTransport::Remote { url, .. } => url.clone(),
        argo_resources::McpTransport::Local { .. } => {
            return Err(ArgoError::Invalid(format!(
                "'{name}' is a local server; it runs as a child process and needs no login"
            )))
        }
    };

    println!("logging in to '{name}' ({url})");
    let store = argo_resources::oauth::token_store_path(paths.root());
    let mut announce = |message: &str| println!("  {message}");
    argo_resources::oauth::login(name, &url, &store, &mut announce).await?;

    println!();
    println!("logged in. '{name}' is now available to every agent that supports MCP,");
    println!("including ones that cannot authenticate it themselves.");
    println!("verify with:  argo mcp check");
    Ok(())
}

/// Forgets stored credentials for a server.
pub async fn mcp_logout(paths: &ArgoPaths, name: &str) -> Result<()> {
    let path = argo_resources::oauth::token_store_path(paths.root());
    let mut store = argo_resources::oauth::TokenStore::load(&path)?;
    if store.tokens.remove(name).is_none() {
        println!("no stored credentials for '{name}'");
        return Ok(());
    }
    store.save(&path)?;
    println!("forgot credentials for '{name}'");
    Ok(())
}

/// Probes each configured MCP server and reports why it is not working.
///
/// Uses `curl` rather than linking an HTTP stack: this is a diagnostic, and the
/// alternative is a TLS dependency for the whole workspace for one command.
pub async fn mcp_check(paths: &ArgoPaths) -> Result<()> {
    let registry = argo_resources::McpRegistry::load(&mcp_path(paths))?;
    if registry.servers.is_empty() {
        println!("no MCP servers configured");
        return Ok(());
    }

    let mut needs_auth: Vec<String> = Vec::new();
    for server in &registry.servers {
        match &server.transport {
            argo_resources::McpTransport::Local { command, .. } => {
                let binary = command.first().cloned().unwrap_or_default();
                let found = which(&binary);
                println!(
                    "  {:<18} {}",
                    server.name,
                    if found {
                        format!("ok — local, {binary}")
                    } else {
                        format!("NOT FOUND — '{binary}' is not on PATH")
                    }
                );
            }
            argo_resources::McpTransport::Remote { url, headers } => {
                // Probe exactly as a turn would, including any token Argo holds.
                let authorized = match argo_resources::oauth::stored_access_token(
                    &server.name,
                    &argo_resources::oauth::token_store_path(paths.root()),
                ) {
                    Some((token, _)) => argo_resources::with_bearer_token(server, &token),
                    None => server.clone(),
                };
                let headers = match &authorized.transport {
                    argo_resources::McpTransport::Remote { headers, .. } => headers.clone(),
                    _ => headers.clone(),
                };
                let (status, body) = probe_remote(url, &headers).await;
                let verdict = match status {
                    Some(code) if (200..300).contains(&code) => "ok".to_string(),
                    Some(401) | Some(403) => {
                        needs_auth.push(server.name.clone());
                        let detail = body
                            .split('"')
                            .find(|part| part.len() > 12 && part.contains(' '))
                            .unwrap_or("authentication required");
                        format!("UNAUTHORIZED — {}", detail.trim())
                    }
                    Some(code) => format!("HTTP {code}"),
                    None => "UNREACHABLE — no response".to_string(),
                };
                println!("  {:<18} {verdict}", server.name);
            }
        }
    }

    if !needs_auth.is_empty() {
        println!();
        println!(
            "{} server(s) need authentication. Argo can do this for you:",
            needs_auth.len()
        );
        println!();
        for name in &needs_auth {
            println!("  argo mcp login {name}");
        }
        println!();
        println!("One login covers every agent, including CLIs that cannot authenticate");
        println!("the server themselves. For a server issuing static tokens instead:");
        println!(
            "  argo mcp add <name> --url <url> --header 'Authorization: Bearer {{env:TOKEN}}'"
        );
    }
    Ok(())
}

/// True when `binary` resolves on `PATH`.
fn which(binary: &str) -> bool {
    if binary.contains('/') {
        return std::path::Path::new(binary).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).exists()))
        .unwrap_or(false)
}

/// Sends an MCP `initialize` and returns the status code and body.
async fn probe_remote(url: &str, headers: &[(String, String)]) -> (Option<u16>, String) {
    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"argo","version":"0.1.0"}}}"#;

    let mut command = tokio::process::Command::new("curl");
    command
        .arg("-sS")
        .args(["-m", "12"])
        .args(["-o", "-"])
        .args(["-w", "\n%{http_code}"])
        .args(["-X", "POST"])
        .arg(url)
        .args(["-H", "Content-Type: application/json"])
        .args(["-H", "Accept: application/json, text/event-stream"])
        .args(["-d", INIT]);
    // Header values may reference the environment, exactly as a real turn does.
    for (name, value) in headers {
        command.args(["-H", &format!("{name}: {value}")]);
    }

    let Ok(output) = command.output().await else {
        return (None, String::new());
    };
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let (body, code) = match text.rsplit_once('\n') {
        Some((body, code)) => (body.to_string(), code.trim().parse::<u16>().ok()),
        None => (text, None),
    };
    (code, body)
}

/// Removes an MCP server.
pub async fn mcp_remove(paths: &ArgoPaths, name: &str) -> Result<()> {
    let path = mcp_path(paths);
    let mut registry = argo_resources::McpRegistry::load(&path)?;
    if !registry.remove(name) {
        return Err(ArgoError::not_found("mcp server", name));
    }
    registry.save(&path)?;
    println!("removed '{name}'");
    Ok(())
}

/// Lists discovered skills.
///
/// Runs in-process: discovery is a filesystem read, so it works even when the
/// daemon is down.
pub async fn skills(_paths: &ArgoPaths, root: Option<String>) -> Result<()> {
    let root = resolve_root(root)?;
    let argo_paths = ArgoPaths::resolve()?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let found = argo_resources::discover(
        std::path::Path::new(&root),
        &argo_paths.user_skills(),
        home.as_deref(),
    )?;

    if found.is_empty() {
        println!("no skills found");
        println!("looked in .argo/skills, .claude/skills, .agents/skills, .opencode/skills, .kiro/skills");
        return Ok(());
    }
    println!("{} skills available to every agent:", found.len());
    for skill in &found {
        let description = skill
            .description
            .split(['.', '\n'])
            .next()
            .unwrap_or_default()
            .trim();
        println!(
            "  {:<28} {:<18} {}",
            skill.name,
            skill.origin.label(),
            description
        );
        for shadowed in &skill.shadows {
            println!("      shadows: {shadowed}");
        }
    }
    Ok(())
}

/// Prints a conversation.
pub async fn show(paths: &ArgoPaths, conversation_id: &str) -> Result<()> {
    let mut client = Client::connect(paths).await?;
    match client
        .request(Request::GetConversation {
            conversation_id: ConversationId::new(conversation_id),
        })
        .await?
    {
        Response::Conversation { summary, messages } => {
            let description = summary.description.clone();
            println!(
                "# {} ({} messages)",
                summary.title.unwrap_or_else(|| summary.id.to_string()),
                summary.message_count
            );
            if let Some(description) = description.filter(|value| !value.trim().is_empty()) {
                println!("\n{description}");
            }
            for message in messages {
                let who = match message.agent_id {
                    Some(agent) => format!("{} ({agent})", message.role),
                    None => message.role,
                };
                println!("\n## {who}\n{}", message.text);
            }
            Ok(())
        }
        Response::Error {
            code,
            message,
            retryable,
        } => Err(ArgoError::remote(code, message, retryable)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

/// Shows what the next turn would send.
pub async fn context(paths: &ArgoPaths, conversation_id: &str, prompt: &str) -> Result<()> {
    let mut client = Client::connect(paths).await?;
    match client
        .request(Request::PreviewContext {
            conversation_id: ConversationId::new(conversation_id),
            prompt: prompt.to_string(),
        })
        .await?
    {
        Response::ContextPreview {
            resuming,
            reason,
            body,
        } => {
            if resuming {
                println!("# resuming the agent's own session: only the new message is sent\n");
            } else {
                let why = reason.unwrap_or_else(|| "no saved session for this agent".into());
                println!("# fresh session ({why}): sending the remaining context\n");
            }
            println!("{body}");
            Ok(())
        }
        Response::Error {
            code,
            message,
            retryable,
        } => Err(ArgoError::remote(code, message, retryable)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}

/// Delegates one self-contained task to another CLI and prints its report.
///
/// Agent-launched commands inherit these ids from the active turn. Explicit flags
/// remain available for automation, but a parent conversation is always required
/// so the child transcript cannot become unreachable.
pub async fn delegate(
    paths: &ArgoPaths,
    parent_conversation_id: Option<String>,
    parent_run_id: Option<String>,
    agent: String,
    model: Option<String>,
    task: String,
) -> Result<()> {
    let parent_conversation_id = parent_conversation_id
        .or_else(|| std::env::var(argo_daemon::mcp::CONVERSATION_ENV).ok())
        .ok_or_else(|| {
            ArgoError::Invalid(
                "no parent conversation; run inside an Argo turn or pass --parent-conversation-id"
                    .to_string(),
            )
        })?;
    let parent_run_id = parent_run_id
        .or_else(|| std::env::var(argo_daemon::mcp::RUN_ENV).ok())
        .map(RunId::new);

    let mut client = Client::connect(paths).await?;
    match client
        .request_within(
            Request::Delegate {
                parent_conversation_id: ConversationId::new(parent_conversation_id),
                parent_run_id,
                agent_id: AgentId::new(agent),
                model,
                task,
                timeout_ms: None,
            },
            None,
        )
        .await?
    {
        Response::DelegateResult {
            conversation_id,
            run_id,
            agent_id,
            ok,
            output,
        } => {
            println!(
                "[subagent {agent_id} · conversation {conversation_id} · run {run_id} · {}]",
                if ok { "completed" } else { "failed" }
            );
            println!("\n{output}");
            if ok {
                Ok(())
            } else {
                Err(ArgoError::Invalid(format!(
                    "subagent {agent_id} did not complete"
                )))
            }
        }
        Response::Error {
            code,
            message,
            retryable,
        } => Err(ArgoError::remote(code, message, retryable)),
        other => Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }
}
pub async fn send(
    paths: &ArgoPaths,
    conversation_id: Option<String>,
    root: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    message: String,
) -> Result<()> {
    let root = resolve_root(root)?;
    let mut client = Client::connect(paths).await?;

    let conversation_id = match conversation_id {
        Some(id) => ConversationId::new(id),
        None => match client
            .request(Request::NewConversation {
                root: root.clone(),
                title: Some(first_line(&message)),
            })
            .await?
        {
            Response::Conversation { summary, .. } => {
                println!("# conversation {}", summary.id);
                summary.id
            }
            Response::Error {
                code,
                message,
                retryable,
            } => return Err(ArgoError::remote(code, message, retryable)),
            other => return Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
        },
    };

    // Selection is recorded first, then applied by the turn — the same path the
    // TUI's `/agent` and `/model` commands use.
    if agent.is_some() || model.is_some() {
        let change = SelectionChange {
            agent_id: agent.map(AgentId::new),
            model,
            reasoning: None,
        };
        match client
            .request(Request::Select {
                conversation_id: conversation_id.clone(),
                change,
            })
            .await?
        {
            Response::Conversation { .. } => {}
            Response::Error {
                code,
                message,
                retryable,
            } => return Err(ArgoError::remote(code, message, retryable)),
            other => return Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
        }
    }

    let (run_id, resumed) = match client
        .request(Request::SendMessage {
            conversation_id,
            prompt: message,
        })
        .await?
    {
        Response::RunStarted {
            run_id,
            agent_id,
            model,
            resumed,
            context_transfer_reason,
            conversation: _,
        } => {
            let model = model.unwrap_or_else(|| "default".into());
            println!(
                "# {agent_id} ({model}){}",
                if resumed {
                    ", resumed".to_string()
                } else if let Some(reason) = context_transfer_reason {
                    format!(", fresh session with context — {reason}")
                } else {
                    ", fresh session with context".to_string()
                }
            );
            (run_id, resumed)
        }
        Response::Error {
            code,
            message,
            retryable,
        } => return Err(ArgoError::remote(code, message, retryable)),
        other => return Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    };
    let _ = resumed;

    stream_events(paths, run_id).await
}

/// Follows a run to completion on a second connection.
///
/// A separate connection keeps the streaming read loop from interleaving with
/// request/response traffic on the first one.
async fn stream_events(paths: &ArgoPaths, run_id: RunId) -> Result<()> {
    let mut client = Client::connect(paths).await?;
    let line = serde_json::to_string(&Request::Subscribe {
        run_id: run_id.clone(),
        after_seq: 0,
    })?;
    client
        .writer
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|e| ArgoError::Io(format!("subscribe: {e}")))?;

    let idle = stream_idle_timeout();
    let mut status = RunStatus::Running;
    loop {
        match client.next_response_within(idle).await {
            Ok(Response::Event { event }) => match event.kind {
                RunEventKind::TextDelta { text } => {
                    print!("{text}");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
                RunEventKind::ToolStarted { name, .. } => eprintln!("  · {name}"),
                RunEventKind::FileWritten { path } => eprintln!("  · wrote {path}"),
                RunEventKind::SessionReseeded { reason } => {
                    eprintln!("  · {reason}; retrying with full context")
                }
                RunEventKind::Error { message, .. } => eprintln!("  ! {message}"),
                RunEventKind::RunFinished {
                    status: final_status,
                    usage,
                } => {
                    status = final_status;
                    if let Some(input) = usage.input {
                        eprintln!("  · tokens in={} out={}", input, usage.output.unwrap_or(0));
                    }
                }
                _ => {}
            },
            Ok(Response::StreamEnd { .. }) => break,
            Ok(Response::Error { message, .. }) => return Err(ArgoError::Invalid(message)),
            Ok(_) => continue,
            Err(ArgoError::Timeout(ms)) => {
                // The daemon still owns the run; report the stall instead of
                // pretending the turn failed.
                eprintln!(
                    "\nargo: no output for {}s. The turn is still running in the daemon; \
                     re-attach with `argo show <conversation-id>` or stop it with `argo stop`.",
                    ms / 1000
                );
                return Err(ArgoError::Timeout(ms));
            }
            Err(error) => return Err(error),
        }
    }

    println!();
    match status {
        RunStatus::Succeeded => Ok(()),
        RunStatus::Cancelled => Err(ArgoError::Cancelled),
        _ => Err(ArgoError::Invalid("the turn did not complete".into())),
    }
}

/// Stops the daemon.
pub async fn stop(paths: &ArgoPaths) -> Result<()> {
    match Client::try_connect(paths).await {
        Ok(mut client) => {
            client.request(Request::Shutdown).await?;
            println!("daemon stopped");
            Ok(())
        }
        Err(_) => {
            println!("daemon is not running");
            Ok(())
        }
    }
}

/// Resolves a workspace root, defaulting to the current directory.
fn resolve_root(root: Option<String>) -> Result<String> {
    match root {
        Some(root) => Ok(root),
        None => Ok(std::env::current_dir()
            .map_err(|e| ArgoError::Io(format!("resolve current directory: {e}")))?
            .to_string_lossy()
            .to_string()),
    }
}

/// First line of `text`, bounded, for use as a conversation title.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= 60 {
        return line.to_string();
    }
    line.chars().take(57).collect::<String>() + "..."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_idle_timeout_is_bounded_by_default_and_disablable() {
        std::env::remove_var("ARGO_STREAM_IDLE_TIMEOUT_MS");
        assert!(
            stream_idle_timeout().is_some(),
            "must not wait forever by default"
        );
        std::env::set_var("ARGO_STREAM_IDLE_TIMEOUT_MS", "0");
        assert!(stream_idle_timeout().is_none(), "0 means wait indefinitely");
        std::env::set_var("ARGO_STREAM_IDLE_TIMEOUT_MS", "1500");
        assert_eq!(
            stream_idle_timeout(),
            Some(std::time::Duration::from_millis(1500))
        );
        std::env::remove_var("ARGO_STREAM_IDLE_TIMEOUT_MS");
    }

    #[test]
    fn titles_are_bounded_to_one_short_line() {
        assert_eq!(first_line("fix the bug\nmore detail"), "fix the bug");
        let long = "x".repeat(200);
        let title = first_line(&long);
        assert_eq!(title.chars().count(), 60);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn blank_messages_produce_an_empty_title_rather_than_panicking() {
        assert_eq!(first_line(""), "");
        assert_eq!(first_line("\n\n"), "");
    }

    #[test]
    fn root_defaults_to_the_current_directory() {
        let resolved = resolve_root(None).expect("resolve");
        assert!(!resolved.is_empty());
        assert_eq!(
            resolve_root(Some("/explicit".into())).expect("resolve"),
            "/explicit"
        );
    }
}
