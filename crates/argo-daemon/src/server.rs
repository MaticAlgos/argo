//! The socket server.
//!
//! Accepts newline-delimited JSON on a private Unix socket, dispatches requests
//! against shared state, and streams run events back. Each connection is handled
//! independently so a slow client cannot stall the daemon.

use crate::engine::{build_context_package, run_turn, SharedStore, TurnRequest};
use crate::lock::InstanceLock;
use crate::protocol::{ConversationSummary, MessageView, Request, Response};
use argo_core::error::{ArgoError, Result};
use argo_core::event::{RunEvent, RunEventKind, RunStatus};
use argo_core::ids::{AgentId, ConversationId, MessageId, RunId};
use argo_core::message::{ContentBlock, ToolCall, ToolStatus};
use argo_core::session::{evaluate_resume, ResumeInputs, SelectionChange};
use argo_core::{ArgoPaths, IPC_PROTOCOL_VERSION};
use argo_runtime::{exec::CancelToken, AgentInfo};
use argo_store::{Conversation, Store};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};

/// Capacity of the per-daemon event fan-out channel.
///
/// Sized so a briefly stalled client does not immediately lose events; a client
/// that falls further behind recovers by re-subscribing with its last cursor,
/// which the store can always satisfy.
const EVENT_CHANNEL_CAPACITY: usize = 4096;

/// Default ceiling on one turn.
///
/// Without a deadline a stalled CLI — an ACP agent that never answers its
/// handshake, say — hangs the turn forever with no way out but Ctrl-C. Generous
/// enough for long agentic work, bounded enough to fail observably.
/// Override with `ARGO_TURN_TIMEOUT_MS`.
const DEFAULT_TURN_TIMEOUT_MS: u64 = 15 * 60 * 1_000;

/// Path of the canonical MCP registry.
fn mcp_registry_path(paths: &ArgoPaths) -> std::path::PathBuf {
    paths.root().join("mcp.json")
}

/// Discovers skills and plans MCP injection for one turn.
///
/// Failures here are downgraded to warnings: a malformed skill on disk should not
/// make the agent unusable, it should just not be offered.
fn resolve_resources(
    paths: &ArgoPaths,
    workspace_root: &str,
    injection: argo_core::runtime::McpInjection,
    run_hint: &str,
) -> (
    Vec<String>,
    argo_resources::McpInjectionPlan,
    Vec<serde_json::Value>,
) {
    resolve_resources_with(paths, workspace_root, injection, run_hint, true)
}

/// As [`resolve_resources`], but able to withhold Argo's own delegation tools.
///
/// Withholding them is how a subagent at the depth cap is stopped from recursing
/// without also losing the user's MCP servers.
fn resolve_resources_with(
    paths: &ArgoPaths,
    workspace_root: &str,
    injection: argo_core::runtime::McpInjection,
    run_hint: &str,
    offer_delegation: bool,
) -> (
    Vec<String>,
    argo_resources::McpInjectionPlan,
    Vec<serde_json::Value>,
) {
    let workspace = std::path::Path::new(workspace_root);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);

    let skills = match argo_resources::discover(workspace, &paths.user_skills(), home.as_deref()) {
        Ok(skills) => skills,
        Err(error) => {
            tracing::warn!(%error, "skill discovery failed; continuing without skills");
            Vec::new()
        }
    };

    let staged = match argo_resources::stage(workspace, &skills) {
        Ok(staged) => staged,
        Err(error) => {
            tracing::warn!(%error, "skill staging failed; continuing without skills");
            Vec::new()
        }
    };
    // Name, description, and path: enough for the model to choose a skill and
    // find it, without inlining any skill bodies into the turn.
    let skill_names: Vec<String> = staged
        .iter()
        .map(|entry| {
            let description = skills
                .iter()
                .find(|s| s.name == entry.name)
                .map(|s| s.description.as_str())
                .unwrap_or_default();
            let description = description
                .split(['.', '\n'])
                .next()
                .unwrap_or(description)
                .trim();
            format!(
                "{} — {} ({}/SKILL.md)",
                entry.name, description, entry.relative
            )
        })
        .collect();

    let mut registry = match argo_resources::McpRegistry::load(&mcp_registry_path(paths)) {
        Ok(registry) => registry,
        Err(error) => {
            tracing::warn!(%error, "mcp registry unreadable; continuing without mcp servers");
            argo_resources::McpRegistry::default()
        }
    };

    // Argo's own delegation server, so an agent that can host MCP can hand work to
    // a different CLI. The parent conversation travels in the environment because
    // the child must attach somewhere inspectable.
    if injection.hosts_delegation() && offer_delegation {
        if let Ok(exe) = std::env::current_exe() {
            let _ = registry.upsert(argo_resources::McpServer {
                name: "argo".to_string(),
                transport: argo_resources::McpTransport::Local {
                    command: vec![
                        exe.to_string_lossy().to_string(),
                        "--data-dir".to_string(),
                        paths.root().to_string_lossy().to_string(),
                        "mcp-server".to_string(),
                    ],
                    environment: vec![(
                        crate::mcp::CONVERSATION_ENV.to_string(),
                        run_hint.to_string(),
                    )],
                },
                enabled: true,
            });
        }
    }
    // Attach any token Argo holds from `argo mcp login`, so a server the user
    // authenticated once reaches every agent — including CLIs that cannot perform
    // an OAuth flow themselves.
    let auth_store = argo_resources::oauth::token_store_path(paths.root());
    registry.servers = registry
        .servers
        .iter()
        .map(
            |server| match argo_resources::oauth::stored_access_token(&server.name, &auth_store) {
                Some((token, stale)) => {
                    if stale {
                        tracing::warn!(
                            server = %server.name,
                            "stored MCP token has expired; run `argo mcp login` again"
                        );
                    }
                    argo_resources::with_bearer_token(server, &token)
                }
                None => server.clone(),
            },
        )
        .collect();

    let plan =
        match argo_resources::mcp::plan_injection(&registry, injection, &paths.staging(), run_hint)
        {
            Ok(plan) => plan,
            Err(error) => {
                tracing::warn!(%error, "mcp injection failed; continuing without mcp servers");
                argo_resources::McpInjectionPlan::default()
            }
        };
    let descriptors = plan.descriptors.clone();
    (skill_names, plan, descriptors)
}

/// Discovers project instruction files for a workspace.
///
/// A malformed or unreadable file is skipped rather than failing the turn.
fn resolve_instructions(workspace_root: &str) -> Option<String> {
    match argo_resources::instructions::discover(std::path::Path::new(workspace_root)) {
        Ok(found) if found.is_empty() => None,
        Ok(found) => {
            let rendered = argo_resources::instructions::render_prompt_section(&found);
            (!rendered.is_empty()).then_some(rendered)
        }
        Err(error) => {
            tracing::warn!(%error, "project instruction discovery failed");
            None
        }
    }
}

/// Resolves the per-turn deadline.
fn turn_timeout_ms() -> Option<u64> {
    match std::env::var("ARGO_TURN_TIMEOUT_MS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            // An explicit 0 disables the deadline for users who want to wait.
            Ok(0) => None,
            Ok(value) => Some(value),
            Err(_) => Some(DEFAULT_TURN_TIMEOUT_MS),
        },
        Err(_) => Some(DEFAULT_TURN_TIMEOUT_MS),
    }
}

/// Shared daemon state.
pub struct Daemon {
    store: SharedStore,
    paths: ArgoPaths,
    agents: Mutex<Option<Vec<AgentInfo>>>,
    running: Mutex<HashMap<RunId, CancelToken>>,
    events: broadcast::Sender<argo_core::event::RunEvent>,
    shutdown: broadcast::Sender<()>,
}

impl Daemon {
    /// Builds daemon state over an existing store.
    pub fn new(store: Store, paths: ArgoPaths) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (shutdown, _) = broadcast::channel(1);
        Self {
            store: Arc::new(StdMutex::new(store)),
            paths,
            agents: Mutex::new(None),
            running: Mutex::new(HashMap::new()),
            events,
            shutdown,
        }
    }

    /// Opens the store at the resolved path and reconciles interrupted runs.
    pub async fn bootstrap(paths: ArgoPaths) -> Result<Self> {
        paths.ensure_dirs()?;
        argo_runtime::validate()?;
        let store = Store::open(paths.database())?;
        store.verify()?;

        // Any run still marked in flight belongs to a dead process: nothing is
        // streaming into it, so leaving it "running" would show a conversation as
        // permanently busy.
        let orphans = store.list_unfinished_runs()?;
        for run in &orphans {
            store.append_event(
                &run.id,
                argo_core::event::RunEventKind::Diagnostic {
                    code: "RUN_INTERRUPTED".into(),
                    detail: "the daemon restarted while this turn was in flight".into(),
                },
            )?;
            store.finish_run(
                &run.id,
                RunStatus::Failed,
                Some((
                    "RUN_INTERRUPTED",
                    "the daemon restarted while this turn was in flight",
                )),
            )?;
        }
        if !orphans.is_empty() {
            tracing::info!(count = orphans.len(), "reconciled interrupted runs");
        }

        let repaired_titles = store.backfill_missing_titles()?;
        if repaired_titles > 0 {
            tracing::info!(
                count = repaired_titles,
                "backfilled missing conversation titles"
            );
        }

        Ok(Self::new(store, paths))
    }

    /// Locks the store for one brief operation.
    fn store(&self) -> Result<std::sync::MutexGuard<'_, Store>> {
        self.store
            .lock()
            .map_err(|_| ArgoError::Store("store lock poisoned by a previous panic".into()))
    }

    /// Detected adapters, probing on first use or when `refresh` is set.
    async fn agent_inventory(&self, refresh: bool) -> Vec<AgentInfo> {
        if !refresh {
            if let Some(cached) = self.agents.lock().await.clone() {
                return cached;
            }
        }
        let detected = argo_runtime::detect_all().await;
        *self.agents.lock().await = Some(detected.clone());
        detected
    }

    /// Resolves which agent and model a conversation's next turn should use.
    ///
    /// Falls back to the first available adapter so a fresh conversation works
    /// without the user having to run `/agent` first.
    async fn resolve_selection(
        &self,
        conversation: &Conversation,
    ) -> Result<(AgentInfo, Option<String>, Option<String>)> {
        let agents = self.agent_inventory(false).await;

        let chosen =
            match &conversation.selected_agent_id {
                Some(id) => agents.iter().find(|a| &a.id == id).ok_or_else(|| {
                    ArgoError::AgentUnavailable {
                        agent: id.clone(),
                        reason: "not present in the adapter registry".into(),
                    }
                })?,
                None => agents.iter().find(|a| a.available).ok_or_else(|| {
                    ArgoError::AgentUnavailable {
                        agent: "any".into(),
                        reason: "no supported coding CLI was detected on PATH".into(),
                    }
                })?,
            };

        if !chosen.available {
            return Err(ArgoError::AgentUnavailable {
                agent: chosen.id.clone(),
                reason: chosen
                    .diagnostics
                    .first()
                    .cloned()
                    .unwrap_or_else(|| format!("install it from {}", chosen.install_url)),
            });
        }

        // A model recorded for a different agent is not portable, so only keep a
        // selection the chosen adapter actually offers.
        let model = conversation
            .selected_model
            .clone()
            .filter(|m| chosen.models.iter().any(|option| &option.id == m));
        let reasoning = conversation
            .selected_reasoning
            .clone()
            .filter(|r| chosen.reasoning.iter().any(|option| &option.id == r));

        Ok((chosen.clone(), model, reasoning))
    }

    /// Builds a summary for one conversation.
    async fn summarize(&self, conversation: &Conversation) -> Result<ConversationSummary> {
        let store = self.store()?;
        let messages = store.list_messages(&conversation.id)?;
        let sessions = store.list_agent_sessions(&conversation.id)?;
        Ok(ConversationSummary {
            id: conversation.id.clone(),
            title: conversation.title.clone(),
            selected_agent_id: conversation.selected_agent_id.clone(),
            selected_model: conversation.selected_model.clone(),
            selected_reasoning: conversation.selected_reasoning.clone(),
            selected_mode: conversation.selected_mode.clone(),
            message_count: messages.len(),
            agents_with_sessions: sessions.iter().map(|s| s.agent_id.to_string()).collect(),
            parent_conversation_id: conversation.parent_conversation_id.clone(),
            updated_at: conversation.updated_at,
        })
    }
}

/// Renders a stored message for display, recovering old structured activity from
/// durable run events when the historical message predates block persistence.
fn message_view(message: &argo_core::message::Message, events: &[RunEvent]) -> MessageView {
    let recovered = blocks_from_events(events);
    let blocks = if recovered.is_empty() {
        message.blocks.clone()
    } else {
        recovered
    };
    MessageView {
        id: message.id.to_string(),
        role: match message.role {
            argo_core::message::Role::User => "user",
            argo_core::message::Role::Assistant => "assistant",
            argo_core::message::Role::System => "system",
        }
        .to_string(),
        text: message.transferable_text(),
        blocks,
        agent_id: message.agent_id.as_ref().map(|a| a.to_string()),
        model: message.model.clone(),
        created_at: message.created_at,
    }
}

fn blocks_from_events(events: &[RunEvent]) -> Vec<ContentBlock> {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for event in events {
        match &event.kind {
            RunEventKind::TextDelta { text } if !text.is_empty() => match blocks.last_mut() {
                Some(ContentBlock::Text { text: accumulated }) => accumulated.push_str(text),
                _ => blocks.push(ContentBlock::text(text)),
            },
            RunEventKind::ThinkingDelta { text } if !text.is_empty() => {
                match blocks.last_mut() {
                    Some(ContentBlock::Thinking { text: accumulated }) => {
                        accumulated.push_str(text)
                    }
                    _ => blocks.push(ContentBlock::Thinking { text: text.clone() }),
                }
            }
            RunEventKind::ToolStarted { id, name, input } => {
                blocks.push(ContentBlock::Tool {
                    call: ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        output: None,
                        status: ToolStatus::Pending,
                    },
                });
            }
            RunEventKind::ToolCompleted { id, output, ok } => {
                let call = blocks.iter_mut().rev().find_map(|block| match block {
                    ContentBlock::Tool { call } if call.id == *id => Some(call),
                    _ => None,
                });
                if let Some(call) = call {
                    call.output = output.clone();
                    call.status = if *ok {
                        ToolStatus::Completed
                    } else {
                        ToolStatus::Failed
                    };
                } else {
                    blocks.push(ContentBlock::Tool {
                        call: ToolCall {
                            id: id.clone(),
                            name: "tool".into(),
                            input: None,
                            output: output.clone(),
                            status: if *ok {
                                ToolStatus::Completed
                            } else {
                                ToolStatus::Failed
                            },
                        },
                    });
                }
            }
            RunEventKind::FileWritten { path }
                if !blocks.iter().any(
                    |block| matches!(block, ContentBlock::FileWrite { path: known } if known == path),
                ) =>
            {
                blocks.push(ContentBlock::FileWrite { path: path.clone() });
            }
            RunEventKind::ChildSpawned {
                child_run_id,
                child_agent_id,
                task,
                ..
            } => blocks.push(ContentBlock::ChildActivity {
                run_id: child_run_id.clone(),
                agent_id: child_agent_id.clone(),
                task: task.clone(),
                status: None,
                blocks: Vec::new(),
            }),
            RunEventKind::ChildEvent {
                child_run_id,
                event,
            } => {
                if let Some(ContentBlock::ChildActivity {
                    blocks: child_blocks,
                    ..
                }) = blocks.iter_mut().rev().find(|block| {
                    matches!(block, ContentBlock::ChildActivity { run_id, .. } if run_id == child_run_id)
                }) {
                    append_child_block(child_blocks, event);
                }
            }
            RunEventKind::ChildCompleted {
                child_run_id,
                status,
            } => {
                if let Some(ContentBlock::ChildActivity {
                    status: known_status,
                    ..
                }) = blocks.iter_mut().rev().find(|block| {
                    matches!(block, ContentBlock::ChildActivity { run_id, .. } if run_id == child_run_id)
                }) {
                    *known_status = Some(*status);
                }
            }
            _ => {}
        }
    }
    blocks
        .into_iter()
        .filter(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Thinking { text } => {
                !text.trim().is_empty()
            }
            _ => true,
        })
        .collect()
}

/// Appends one explicitly emitted native-child event to its own content list.
fn append_child_block(blocks: &mut Vec<ContentBlock>, event: &RunEventKind) {
    match event {
        RunEventKind::TextDelta { text } if !text.is_empty() => match blocks.last_mut() {
            Some(ContentBlock::Text { text: accumulated }) => accumulated.push_str(text),
            _ => blocks.push(ContentBlock::text(text)),
        },
        RunEventKind::ThinkingDelta { text } if !text.is_empty() => match blocks.last_mut() {
            Some(ContentBlock::Thinking { text: accumulated }) => accumulated.push_str(text),
            _ => blocks.push(ContentBlock::Thinking { text: text.clone() }),
        },
        RunEventKind::ToolStarted { id, name, input } => blocks.push(ContentBlock::Tool {
            call: ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                output: None,
                status: ToolStatus::Pending,
            },
        }),
        RunEventKind::ToolCompleted { id, output, ok } => {
            if let Some(call) = blocks.iter_mut().rev().find_map(|block| match block {
                ContentBlock::Tool { call } if call.id == *id => Some(call),
                _ => None,
            }) {
                call.output = output.clone();
                call.status = if *ok {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Failed
                };
            }
        }
        RunEventKind::FileWritten { path }
            if !blocks.iter().any(
                |block| matches!(block, ContentBlock::FileWrite { path: known } if known == path),
            ) =>
        {
            blocks.push(ContentBlock::FileWrite { path: path.clone() });
        }
        _ => {}
    }
}

/// Serves until shutdown is requested.
pub async fn serve(daemon: Arc<Daemon>, lock: InstanceLock) -> Result<()> {
    let socket_path = daemon.paths.socket();

    // A leftover socket file from a crashed daemon would make bind() fail. The
    // instance lock already proved no live daemon owns it, so removing it is safe.
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .map_err(|e| ArgoError::Io(format!("remove stale socket: {e}")))?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| ArgoError::Io(format!("bind {}: {e}", socket_path.display())))?;

    // The socket is the entire access-control boundary for an unauthenticated
    // local API, so it must not be reachable by other users on the machine.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| ArgoError::Io(format!("secure socket: {e}")))?;
    }

    tracing::info!(socket = %socket_path.display(), "argo daemon listening");

    let mut shutdown_rx = daemon.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let daemon = Arc::clone(&daemon);
                        tokio::spawn(async move {
                            if let Err(error) = handle_client(daemon, stream).await {
                                tracing::debug!(%error, "client disconnected");
                            }
                        });
                    }
                    Err(error) => tracing::warn!(%error, "accept failed"),
                }
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    drop(lock);
    tracing::info!("argo daemon stopped");
    Ok(())
}

/// Handles one client connection.
async fn handle_client(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let mut handshaken = false;

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| ArgoError::Io(format!("read request: {e}")))?
    {
        if line.trim().is_empty() {
            continue;
        }

        let request = match Request::decode(&line) {
            Ok(request) => request,
            Err(error) => {
                write_half
                    .write_all(Response::from_error(&error).encode().as_bytes())
                    .await
                    .ok();
                continue;
            }
        };

        // Version negotiation precedes everything: a mismatched client must not be
        // allowed to issue requests it may misinterpret.
        if !handshaken {
            match &request {
                Request::Hello { protocol, client } => {
                    if *protocol != IPC_PROTOCOL_VERSION {
                        let error = ArgoError::Invalid(format!(
                            "client protocol v{protocol} does not match daemon v{IPC_PROTOCOL_VERSION}; restart both from the same build"
                        ));
                        write_half
                            .write_all(Response::from_error(&error).encode().as_bytes())
                            .await
                            .ok();
                        return Ok(());
                    }
                    handshaken = true;
                    tracing::debug!(client = %client, "client connected");
                    let welcome = Response::Welcome {
                        protocol: IPC_PROTOCOL_VERSION,
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        database: daemon.paths.database().to_string_lossy().to_string(),
                    };
                    write_half.write_all(welcome.encode().as_bytes()).await.ok();
                    continue;
                }
                _ => {
                    let error = ArgoError::Invalid("expected a hello handshake first".to_string());
                    write_half
                        .write_all(Response::from_error(&error).encode().as_bytes())
                        .await
                        .ok();
                    return Ok(());
                }
            }
        }

        // Subscriptions stream many responses, so they own the write half while
        // active rather than returning a single reply.
        if let Request::Subscribe { run_id, after_seq } = &request {
            stream_run(&daemon, &mut write_half, run_id, *after_seq).await?;
            continue;
        }

        let response = dispatch(&daemon, request).await;
        write_half
            .write_all(response.encode().as_bytes())
            .await
            .map_err(|e| ArgoError::Io(format!("write response: {e}")))?;
    }

    Ok(())
}

/// Streams a run's events from a cursor until it terminates.
async fn stream_run(
    daemon: &Arc<Daemon>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    run_id: &RunId,
    after_seq: i64,
) -> Result<()> {
    // Subscribe before replaying, so events emitted during the replay are not lost
    // in the gap between the two.
    let mut live = daemon.events.subscribe();

    let (backlog, already_finished) = {
        let store = daemon.store()?;
        let backlog = store.list_events_after(run_id, after_seq)?;
        let finished = store
            .get_run(run_id)
            .map(|run| run.status.is_terminal())
            .unwrap_or(false);
        (backlog, finished)
    };

    let mut last_seq = after_seq;
    for event in backlog {
        last_seq = event.seq;
        let terminal = event.is_terminal();
        write_half
            .write_all(Response::Event { event }.encode().as_bytes())
            .await
            .map_err(|e| ArgoError::Io(format!("write event: {e}")))?;
        if terminal {
            write_half
                .write_all(
                    Response::StreamEnd {
                        run_id: run_id.clone(),
                    }
                    .encode()
                    .as_bytes(),
                )
                .await
                .ok();
            return Ok(());
        }
    }

    if already_finished {
        // Terminal but no terminal event was recorded (an interrupted run), so
        // close the stream instead of waiting forever.
        write_half
            .write_all(
                Response::StreamEnd {
                    run_id: run_id.clone(),
                }
                .encode()
                .as_bytes(),
            )
            .await
            .ok();
        return Ok(());
    }

    loop {
        match live.recv().await {
            Ok(event) => {
                if &event.run_id != run_id || event.seq <= last_seq {
                    continue;
                }
                last_seq = event.seq;
                let terminal = event.is_terminal();
                write_half
                    .write_all(Response::Event { event }.encode().as_bytes())
                    .await
                    .map_err(|e| ArgoError::Io(format!("write event: {e}")))?;
                if terminal {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // The client fell behind. Refill from the store rather than
                // silently dropping the gap.
                tracing::debug!(skipped, "event subscriber lagged; refilling from store");
                let missed = {
                    let store = daemon.store()?;
                    store.list_events_after(run_id, last_seq)?
                };
                for event in missed {
                    last_seq = event.seq;
                    let terminal = event.is_terminal();
                    write_half
                        .write_all(Response::Event { event }.encode().as_bytes())
                        .await
                        .ok();
                    if terminal {
                        break;
                    }
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    write_half
        .write_all(
            Response::StreamEnd {
                run_id: run_id.clone(),
            }
            .encode()
            .as_bytes(),
        )
        .await
        .ok();
    Ok(())
}

/// Dispatches one request.
async fn dispatch(daemon: &Arc<Daemon>, request: Request) -> Response {
    match handle(daemon, request).await {
        Ok(response) => response,
        Err(error) => Response::from_error(&error),
    }
}

async fn handle(daemon: &Arc<Daemon>, request: Request) -> Result<Response> {
    match request {
        Request::Hello { .. } => Ok(Response::Welcome {
            protocol: IPC_PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            database: daemon.paths.database().to_string_lossy().to_string(),
        }),

        Request::Ping => Ok(Response::Ok),

        Request::ListAgents { refresh } => Ok(Response::Agents {
            agents: daemon.agent_inventory(refresh).await,
        }),

        Request::OpenWorkspace { root } => {
            let (workspace, conversations) = {
                let store = daemon.store()?;
                let workspace = store.ensure_workspace(&root)?;
                let conversations = store.list_conversations(&workspace)?;
                (workspace, conversations)
            };
            let mut summaries = Vec::new();
            for conversation in &conversations {
                summaries.push(daemon.summarize(conversation).await?);
            }
            let canonical = daemon.store()?.workspace_root(&workspace)?;
            Ok(Response::Workspace {
                root: canonical,
                conversations: summaries,
            })
        }

        Request::ListConversations { root } => {
            let conversations = {
                let store = daemon.store()?;
                let workspace = store.ensure_workspace(&root)?;
                store.list_conversations(&workspace)?
            };
            let mut summaries = Vec::new();
            for conversation in &conversations {
                summaries.push(daemon.summarize(conversation).await?);
            }
            Ok(Response::Conversations {
                conversations: summaries,
            })
        }

        Request::NewConversation { root, title } => {
            let conversation = {
                let store = daemon.store()?;
                let workspace = store.ensure_workspace(&root)?;
                let id = store.create_conversation(&workspace, title.as_deref())?;
                store.get_conversation(&id)?
            };
            Ok(Response::Conversation {
                summary: daemon.summarize(&conversation).await?,
                messages: vec![],
            })
        }

        Request::GetConversation { conversation_id } => {
            let (conversation, messages) = {
                let store = daemon.store()?;
                let conversation = store.get_conversation(&conversation_id)?;
                let stored = store.list_messages(&conversation_id)?;
                let mut messages = Vec::with_capacity(stored.len());
                for message in &stored {
                    let events = match &message.run_id {
                        Some(run_id) => store.list_events_after(run_id, 0)?,
                        None => Vec::new(),
                    };
                    messages.push(message_view(message, &events));
                }
                (conversation, messages)
            };
            Ok(Response::Conversation {
                summary: daemon.summarize(&conversation).await?,
                messages,
            })
        }

        Request::Select {
            conversation_id,
            change,
        } => {
            let existing = daemon.store()?.get_conversation(&conversation_id)?;
            validate_selection(daemon, &existing, &change).await?;
            let conversation = {
                let store = daemon.store()?;
                store.update_selection(&conversation_id, &change)?;
                store.get_conversation(&conversation_id)?
            };
            Ok(Response::Conversation {
                summary: daemon.summarize(&conversation).await?,
                messages: vec![],
            })
        }

        Request::SetMode {
            conversation_id,
            mode,
        } => {
            // Reject a mode the selected adapter cannot enforce, rather than
            // displaying a restriction that is not actually in force.
            if let Some(requested) = &mode {
                let parsed = argo_core::mode::AgentMode::parse(requested).ok_or_else(|| {
                    ArgoError::Invalid(format!(
                        "unknown mode '{requested}'. Available: full, plan, accept-edits, read-only"
                    ))
                })?;
                let conversation = daemon.store()?.get_conversation(&conversation_id)?;
                let (agent, _, _) = daemon.resolve_selection(&conversation).await?;
                let def = argo_runtime::require(&agent.id)?;
                if !def.capabilities.modes.supports(parsed) {
                    let available: Vec<&str> = def
                        .capabilities
                        .modes
                        .available()
                        .iter()
                        .map(|m| m.id())
                        .collect();
                    return Err(ArgoError::Invalid(format!(
                        "{} cannot enforce '{}' mode. Available: {}",
                        def.name,
                        parsed.id(),
                        available.join(", ")
                    )));
                }
            }
            let conversation = {
                let store = daemon.store()?;
                store.set_mode(&conversation_id, mode.as_deref())?;
                store.get_conversation(&conversation_id)?
            };
            Ok(Response::Conversation {
                summary: daemon.summarize(&conversation).await?,
                messages: vec![],
            })
        }

        Request::PreviewContext {
            conversation_id,
            prompt,
        } => preview_context(daemon, conversation_id, prompt).await,

        Request::SendMessage {
            conversation_id,
            prompt,
        } => send_message(daemon, conversation_id, prompt).await,

        Request::Cancel { run_id } => {
            let running = daemon.running.lock().await;
            match running.get(&run_id) {
                Some(token) => {
                    token.cancel();
                    Ok(Response::Ok)
                }
                None => Err(ArgoError::not_found("active run", run_id.as_str())),
            }
        }

        Request::ListChildren { conversation_id } => {
            let children = {
                let store = daemon.store()?;
                store.list_child_conversations(&conversation_id)?
            };
            let mut summaries = Vec::new();
            for child in &children {
                summaries.push(daemon.summarize(child).await?);
            }
            Ok(Response::Children {
                children: summaries,
            })
        }

        Request::Delegate {
            parent_conversation_id,
            parent_run_id,
            agent_id,
            model,
            task,
            timeout_ms,
        } => {
            delegate(
                daemon,
                parent_conversation_id,
                parent_run_id,
                agent_id,
                model,
                task,
                timeout_ms,
            )
            .await
        }

        Request::Shutdown => {
            // Stop in-flight turns so children are signalled rather than orphaned.
            for token in daemon.running.lock().await.values() {
                token.cancel();
            }
            let _ = daemon.shutdown.send(());
            Ok(Response::Ok)
        }

        Request::Subscribe { .. } => Err(ArgoError::Invalid(
            "subscribe is handled by the stream path".into(),
        )),
    }
}

/// Rejects a selection the registry cannot honor, before it is persisted.
///
/// Catching an invalid value here produces a clear message naming the valid
/// options, instead of an opaque CLI failure at spawn time.
async fn validate_selection(
    daemon: &Arc<Daemon>,
    conversation: &Conversation,
    change: &SelectionChange,
) -> Result<()> {
    let agents = daemon.agent_inventory(false).await;

    // The agent this change resolves to: the new one, or the one already selected.
    let agent_id = change
        .agent_id
        .as_ref()
        .map(|a| a.to_string())
        .or_else(|| conversation.selected_agent_id.clone());

    let Some(agent_id) = agent_id else {
        // Nothing selected yet and none named: the turn will pick a default.
        return Ok(());
    };
    let def = argo_runtime::require(&agent_id)?;
    let info = agents.iter().find(|a| a.id == def.id);

    if let Some(model) = &change.model {
        let known = info
            .map(|a| a.models.iter().any(|m| &m.id == model))
            .unwrap_or(false);
        if !known {
            let available: Vec<&str> = info
                .map(|a| a.models.iter().map(|m| m.id.as_str()).take(12).collect())
                .unwrap_or_default();
            return Err(ArgoError::Invalid(format!(
                "{} does not offer model '{model}'. Available: {}",
                def.name,
                available.join(", ")
            )));
        }
    }

    if let Some(reasoning) = &change.reasoning {
        // Levels are per model, so validate against the model in effect after this
        // change rather than the adapter-wide list.
        let model = change
            .model
            .clone()
            .or_else(|| conversation.selected_model.clone());
        let levels = info
            .map(|a| a.reasoning_for(model.as_deref()))
            .unwrap_or_default();
        if levels.is_empty() {
            return Err(ArgoError::Invalid(format!(
                "{} does not expose reasoning levels",
                def.name
            )));
        }
        if !levels.iter().any(|l| &l.id == reasoning) {
            let available: Vec<&str> = levels.iter().map(|l| l.id.as_str()).collect();
            return Err(ArgoError::Invalid(format!(
                "'{reasoning}' is not a reasoning level for {}. Available: {}",
                model.unwrap_or_else(|| def.name.to_string()),
                available.join(", ")
            )));
        }
    }
    Ok(())
}

/// Composes the body the next turn would send, without running anything.
async fn preview_context(
    daemon: &Arc<Daemon>,
    conversation_id: ConversationId,
    prompt: String,
) -> Result<Response> {
    let conversation = daemon.store()?.get_conversation(&conversation_id)?;
    let (agent, model, reasoning) = daemon.resolve_selection(&conversation).await?;
    let def = argo_runtime::require(&agent.id)?;

    let (cwd, stored, cursor) = {
        let store = daemon.store()?;
        let cwd = store.workspace_root(&conversation.workspace_id)?;
        let stored = store.get_agent_session(&conversation_id, &AgentId::new(&agent.id))?;
        let cursor = store.latest_completed_assistant_message_id(
            &conversation_id,
            None,
            stored.as_ref().and_then(|s| s.last_message_id.as_ref()),
        )?;
        (cwd, stored, cursor)
    };
    let plan = evaluate_resume(ResumeInputs {
        stored: stored.as_ref(),
        supports_resume: def.capabilities.native_resume,
        current_model: model.as_deref(),
        current_cwd: Some(&cwd),
        latest_completed_assistant: cursor.as_ref(),
    });

    let (skill_names, mcp_plan, descriptors) = resolve_resources(
        &daemon.paths,
        &cwd,
        def.capabilities.mcp_injection,
        "preview",
    );

    let turn = TurnRequest {
        conversation_id: conversation_id.clone(),
        parent_run_id: None,
        delegation_allowed: true,
        prompt: prompt.clone(),
        agent_id: AgentId::new(&agent.id),
        model,
        reasoning,
        bin: agent.path.clone().unwrap_or_else(|| def.bin.to_string()),
        help_flags: argo_runtime::observed_flags(&agent.id),
        active_skills: skill_names,
        active_mcp_servers: mcp_plan.names.clone(),
        project_instructions: resolve_instructions(&cwd),
        mcp_descriptors: descriptors,
        mcp_config: mcp_plan
            .config_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        timeout_ms: turn_timeout_ms(),
        mode: conversation
            .selected_mode
            .as_deref()
            .and_then(argo_core::mode::AgentMode::parse)
            .unwrap_or_default(),
    };

    let body = if plan.skip_transcript() {
        argo_context::compose_turn(&plan, &argo_context::ContextPackage::default(), &prompt)
    } else {
        let package =
            build_context_package(&daemon.store, &turn, &MessageId::new("preview"), &cwd)?;
        argo_context::compose_turn(&plan, &package, &prompt)
    };

    Ok(Response::ContextPreview {
        resuming: plan.skip_transcript(),
        reason: plan.invalidation.map(|r| r.detail().to_string()),
        body,
    })
}

/// Maximum nesting depth for delegated agents.
///
/// Without a cap, a subagent that delegates to a subagent can recurse until the
/// machine runs out of processes.
const MAX_DELEGATION_DEPTH: usize = 3;

/// Runs a task on another CLI as a subagent and returns its result.
///
/// The child gets its own conversation and its own upstream session, seeded with a
/// bounded capsule of the parent's context. It never inherits the parent's session
/// handle: two CLIs cannot share one session, and pretending otherwise is how
/// context gets corrupted.
async fn delegate(
    daemon: &Arc<Daemon>,
    parent_conversation_id: ConversationId,
    parent_run_id: Option<RunId>,
    agent_id: AgentId,
    model: Option<String>,
    task: String,
    timeout_ms: Option<u64>,
) -> Result<Response> {
    if task.trim().is_empty() {
        return Err(ArgoError::Invalid("the delegated task is empty".into()));
    }

    let def = argo_runtime::require(agent_id.as_str())?;
    let agents = daemon.agent_inventory(false).await;
    let info =
        agents
            .iter()
            .find(|a| a.id == def.id)
            .ok_or_else(|| ArgoError::AgentUnavailable {
                agent: def.id.to_string(),
                reason: "not present in the adapter registry".into(),
            })?;
    if !info.available {
        return Err(ArgoError::AgentUnavailable {
            agent: def.id.to_string(),
            reason: info
                .diagnostics
                .first()
                .cloned()
                .unwrap_or_else(|| format!("install it from {}", info.install_url)),
        });
    }

    // Refuse to nest without bound.
    let (parent, depth) = {
        let store = daemon.store()?;
        let parent = store.get_conversation(&parent_conversation_id)?;
        let mut depth = 0usize;
        let mut cursor = parent.parent_conversation_id.clone();
        while let Some(id) = cursor {
            depth += 1;
            if depth > MAX_DELEGATION_DEPTH {
                break;
            }
            cursor = store.get_conversation(&id)?.parent_conversation_id;
        }
        (parent, depth)
    };
    if depth >= MAX_DELEGATION_DEPTH {
        return Err(ArgoError::Invalid(format!(
            "delegation is nested {depth} levels deep; the limit is {MAX_DELEGATION_DEPTH}"
        )));
    }

    let parent_run_id = match parent_run_id {
        Some(run_id) => Some(run_id),
        None => daemon
            .store()?
            .running_run_for_conversation(&parent_conversation_id)?
            .map(|run| run.id),
    };

    if let Some(parent_run_id) = &parent_run_id {
        let run = daemon.store()?.get_run(parent_run_id)?;
        if run.conversation_id != parent_conversation_id {
            return Err(ArgoError::Invalid(format!(
                "parent run {parent_run_id} does not belong to conversation {parent_conversation_id}"
            )));
        }
        if run.status != RunStatus::Running {
            return Err(ArgoError::Invalid(format!(
                "parent run {parent_run_id} is not running"
            )));
        }
    }

    // A bounded capsule of the parent's conversation, so the child understands the
    // work without being handed the entire history.
    let capsule = {
        let store = daemon.store()?;
        let recent = store.list_recent_messages(&parent_conversation_id, 12)?;
        argo_context::flatten_transcript(&recent)
    };

    let workspace_root = daemon.store()?.workspace_root(&parent.workspace_id)?;
    let model = model.filter(|m| info.models.iter().any(|option| &option.id == m));

    // The child is a real conversation, so its transcript is inspectable through
    // `/children` rather than vanishing into the parent's turn.
    let child_conversation = {
        let store = daemon.store()?;
        let title = format!("{} · {}", def.id, first_line(&task));
        store.create_child_conversation(
            &parent.workspace_id,
            &parent_conversation_id,
            parent_run_id.as_ref(),
            Some(&title),
        )?
    };
    daemon.store()?.update_selection(
        &child_conversation,
        &SelectionChange {
            agent_id: Some(agent_id.clone()),
            model: model.clone(),
            reasoning: None,
        },
    )?;

    // A subagent gets the same resources as any other turn: the user's MCP servers
    // reach it too, otherwise delegating a task that needs one would silently fail.
    //
    // Whether it may delegate *further* is decided by depth alone — the check above
    // is authoritative. At the last permitted level the delegation tool is withheld,
    // so the child keeps its own MCP servers but cannot recurse past the cap.
    let child_may_delegate = depth + 1 < MAX_DELEGATION_DEPTH;
    let (skill_names, mcp_plan, descriptors) = resolve_resources_with(
        &daemon.paths,
        &workspace_root,
        def.capabilities.mcp_injection,
        child_conversation.as_str(),
        child_may_delegate,
    );
    let prompt = if capsule.is_empty() {
        task.clone()
    } else {
        format!(
            "You are acting as a subagent inside Argo. Another agent delegated this task to you.\n\n             ## Conversation so far\n\n{capsule}\n\n## Your task\n{task}\n\n             Report what you did and what you found. You are not continuing the other agent's turn."
        )
    };

    let turn = TurnRequest {
        conversation_id: child_conversation.clone(),
        parent_run_id: parent_run_id.clone(),
        delegation_allowed: child_may_delegate,
        prompt,
        agent_id: agent_id.clone(),
        model: model.clone(),
        reasoning: None,
        bin: info.path.clone().unwrap_or_else(|| def.bin.to_string()),
        help_flags: argo_runtime::observed_flags(&info.id),
        active_skills: skill_names,
        active_mcp_servers: mcp_plan.names.clone(),
        project_instructions: resolve_instructions(&workspace_root),
        mcp_descriptors: descriptors,
        mcp_config: mcp_plan
            .config_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        timeout_ms: timeout_ms.or_else(turn_timeout_ms),
        // A subagent inherits full authority: it was asked to do work, not plan.
        mode: argo_core::mode::AgentMode::Full,
    };

    let cancel = CancelToken::new();
    let events = daemon.events.clone();
    let store = Arc::clone(&daemon.store);
    let lifecycle_parent = parent_run_id.clone();
    let lifecycle_agent = agent_id.clone();
    let lifecycle_task = task.clone();
    let spawned = std::sync::atomic::AtomicBool::new(false);
    let listener = move |event: argo_core::event::RunEvent| {
        // The first persisted child event discloses its real generated run id.
        // Record spawn on the parent before publishing child output so a TUI can
        // subscribe and replay from sequence zero without losing anything.
        if let Some(parent_run_id) = &lifecycle_parent {
            if !spawned.swap(true, std::sync::atomic::Ordering::SeqCst) {
                if let Ok(store) = store.lock() {
                    if let Ok(parent_event) = store.append_event(
                        parent_run_id,
                        RunEventKind::ChildSpawned {
                            child_run_id: event.run_id.clone(),
                            child_agent_id: lifecycle_agent.clone(),
                            task: lifecycle_task.clone(),
                            native: false,
                        },
                    ) {
                        let _ = events.send(parent_event);
                    }
                }
            }
        }

        let terminal = match &event.kind {
            RunEventKind::RunFinished { status, .. } => Some(*status),
            _ => None,
        };
        let child_run_id = event.run_id.clone();
        let _ = events.send(event);

        // Child RunFinished is its own commit barrier. Publish completion to the
        // parent only after that barrier, and never reinterpret it as the parent
        // turn's terminal event.
        if let (Some(parent_run_id), Some(status)) = (&lifecycle_parent, terminal) {
            if let Ok(store) = store.lock() {
                if let Ok(parent_event) = store.append_event(
                    parent_run_id,
                    RunEventKind::ChildCompleted {
                        child_run_id,
                        status,
                    },
                ) {
                    let _ = events.send(parent_event);
                }
            }
        }
    };

    let outcome = run_turn(&daemon.store, &daemon.paths, turn, &cancel, Some(&listener)).await?;

    // The child's reply is what the parent agent receives as its tool result.
    let output = {
        let store = daemon.store()?;
        store
            .list_messages(&child_conversation)?
            .into_iter()
            .rfind(|m| m.role == argo_core::message::Role::Assistant)
            .map(|m| m.transferable_text())
            .unwrap_or_default()
    };

    let ok = outcome.status == RunStatus::Succeeded;
    Ok(Response::DelegateResult {
        conversation_id: child_conversation,
        run_id: outcome.run_id,
        agent_id: def.id.to_string(),
        ok,
        output: if output.trim().is_empty() {
            if ok {
                "the subagent produced no output".to_string()
            } else {
                "the subagent did not complete".to_string()
            }
        } else {
            output
        },
    })
}

/// First line of `text`, bounded, for a child conversation title.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= 48 {
        return line.to_string();
    }
    line.chars().take(45).collect::<String>() + "..."
}

/// Accepts a turn and runs it in the background, streaming events.
async fn send_message(
    daemon: &Arc<Daemon>,
    conversation_id: ConversationId,
    prompt: String,
) -> Result<Response> {
    if prompt.trim().is_empty() {
        return Err(ArgoError::Invalid("message is empty".into()));
    }

    let conversation = daemon.store()?.get_conversation(&conversation_id)?;
    let (agent, model, reasoning) = daemon.resolve_selection(&conversation).await?;
    let def = argo_runtime::require(&agent.id)?;

    // Decide resume vs fresh up front so the acceptance reply can tell the TUI
    // whether this turn is carrying the full context.
    let resume_plan = {
        let store = daemon.store()?;
        let cwd = store.workspace_root(&conversation.workspace_id)?;
        let stored = store.get_agent_session(&conversation_id, &AgentId::new(&agent.id))?;
        let cursor = store.latest_completed_assistant_message_id(
            &conversation_id,
            None,
            stored.as_ref().and_then(|s| s.last_message_id.as_ref()),
        )?;
        evaluate_resume(ResumeInputs {
            stored: stored.as_ref(),
            supports_resume: def.capabilities.native_resume,
            current_model: model.as_deref(),
            current_cwd: Some(&cwd),
            latest_completed_assistant: cursor.as_ref(),
        })
    };
    let resumed = resume_plan.skip_transcript();
    let context_transfer_reason = resume_plan
        .invalidation
        .map(|reason| reason.detail().to_string());

    let workspace_root = daemon.store()?.workspace_root(&conversation.workspace_id)?;
    let (skill_names, mcp_plan, descriptors) = resolve_resources(
        &daemon.paths,
        &workspace_root,
        def.capabilities.mcp_injection,
        conversation_id.as_str(),
    );

    let conversation_id_for_summary = conversation_id.clone();
    let turn = TurnRequest {
        conversation_id,
        parent_run_id: None,
        delegation_allowed: true,
        prompt,
        agent_id: AgentId::new(&agent.id),
        model: model.clone(),
        reasoning,
        bin: agent.path.clone().unwrap_or_else(|| def.bin.to_string()),
        help_flags: argo_runtime::observed_flags(&agent.id),
        active_skills: skill_names,
        active_mcp_servers: mcp_plan.names.clone(),
        project_instructions: resolve_instructions(&workspace_root),
        mcp_descriptors: descriptors,
        mcp_config: mcp_plan
            .config_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        timeout_ms: turn_timeout_ms(),
        mode: conversation
            .selected_mode
            .as_deref()
            .and_then(argo_core::mode::AgentMode::parse)
            .unwrap_or_default(),
    };

    // The run row must exist before this call returns so the client can subscribe
    // without racing the spawn.
    let cancel = CancelToken::new();
    let daemon_for_turn = Arc::clone(daemon);
    let cancel_for_turn = cancel.clone();
    let agent_id = agent.id.clone();

    let (tx, rx) = tokio::sync::oneshot::channel::<RunId>();

    tokio::spawn(async move {
        let events = daemon_for_turn.events.clone();
        // The first event carries the run id, which both announces the run to the
        // waiting caller and fans out to any subscriber.
        let announce = std::sync::Mutex::new(Some(tx));
        let listener = move |event: argo_core::event::RunEvent| {
            if let Ok(mut guard) = announce.lock() {
                if let Some(sender) = guard.take() {
                    let _ = sender.send(event.run_id.clone());
                }
            }
            // A send failure only means no client is currently subscribed.
            let _ = events.send(event);
        };

        let paths = daemon_for_turn.paths.clone();
        let result = run_turn(
            &daemon_for_turn.store,
            &paths,
            turn,
            &cancel_for_turn,
            Some(&listener),
        )
        .await;

        match &result {
            Ok(outcome) => {
                daemon_for_turn.running.lock().await.remove(&outcome.run_id);
            }
            Err(error) => tracing::warn!(%error, agent = %agent_id, "turn failed"),
        }
    });

    let run_id = rx
        .await
        .map_err(|_| ArgoError::Process("turn did not start".into()))?;
    daemon.running.lock().await.insert(run_id.clone(), cancel);

    // The engine persists the title and both message rows before announcing the
    // run, so this summary is authoritative and can update every client view
    // immediately instead of waiting for RunFinished.
    let conversation = daemon
        .store()?
        .get_conversation(&conversation_id_for_summary)?;
    let conversation = daemon.summarize(&conversation).await?;

    Ok(Response::RunStarted {
        run_id,
        agent_id: agent.id,
        model,
        resumed,
        context_transfer_reason,
        conversation: Some(conversation),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn daemon() -> (Arc<Daemon>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ArgoPaths::with_root(dir.path().join("data"));
        let daemon = Daemon::bootstrap(paths).await.expect("bootstrap");

        // Unit tests exercise daemon policy, not host CLI discovery. Seed every
        // registered adapter as available so results do not depend on which coding
        // agents happen to be installed on a developer machine or CI runner.
        let test_bin = std::env::current_exe()
            .expect("test executable")
            .to_string_lossy()
            .into_owned();
        let agents = argo_runtime::ADAPTERS
            .iter()
            .map(|def| {
                let mut info = AgentInfo::unavailable(def, "test fixture");
                info.available = true;
                info.path = Some(test_bin.clone());
                info.version = Some("test".into());
                info.diagnostics.clear();
                info
            })
            .collect();
        *daemon.agents.lock().await = Some(agents);

        (Arc::new(daemon), dir)
    }

    #[test]
    fn turn_timeout_defaults_and_can_be_overridden() {
        // A missing or malformed value must still produce a bounded deadline; only
        // an explicit 0 means "wait indefinitely".
        std::env::remove_var("ARGO_TURN_TIMEOUT_MS");
        assert_eq!(turn_timeout_ms(), Some(DEFAULT_TURN_TIMEOUT_MS));
        std::env::set_var("ARGO_TURN_TIMEOUT_MS", "5000");
        assert_eq!(turn_timeout_ms(), Some(5_000));
        std::env::set_var("ARGO_TURN_TIMEOUT_MS", "0");
        assert_eq!(turn_timeout_ms(), None);
        std::env::set_var("ARGO_TURN_TIMEOUT_MS", "not-a-number");
        assert_eq!(turn_timeout_ms(), Some(DEFAULT_TURN_TIMEOUT_MS));
        std::env::remove_var("ARGO_TURN_TIMEOUT_MS");
    }

    #[tokio::test]
    async fn ping_and_agent_listing_work() {
        let (daemon, _dir) = daemon().await;
        assert_eq!(
            handle(&daemon, Request::Ping).await.expect("ping"),
            Response::Ok
        );
        let response = handle(&daemon, Request::ListAgents { refresh: false })
            .await
            .expect("agents");
        match response {
            Response::Agents { agents } => {
                assert_eq!(agents.len(), 7);
                assert!(agents.iter().any(|a| a.id == "claude"));
                assert!(agents.iter().any(|a| a.id == "grok"));
                assert!(agents.iter().any(|a| a.id == "opencode"));
                assert!(agents.iter().any(|a| a.id == "cmd"));
                assert!(agents.iter().any(|a| a.id == "antigravity"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn opening_a_workspace_creates_it_and_lists_conversations() {
        let (daemon, dir) = daemon().await;
        let root = dir.path().to_string_lossy().to_string();
        let response = handle(&daemon, Request::OpenWorkspace { root: root.clone() })
            .await
            .expect("open");
        match response {
            Response::Workspace { conversations, .. } => assert!(conversations.is_empty()),
            other => panic!("unexpected: {other:?}"),
        }

        handle(
            &daemon,
            Request::NewConversation {
                root: root.clone(),
                title: Some("first".into()),
            },
        )
        .await
        .expect("new");

        let response = handle(&daemon, Request::ListConversations { root })
            .await
            .expect("list");
        match response {
            Response::Conversations { conversations } => {
                assert_eq!(conversations.len(), 1);
                assert_eq!(conversations[0].title.as_deref(), Some("first"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn selecting_an_unknown_agent_is_rejected() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        let error = handle(
            &daemon,
            Request::Select {
                conversation_id: conversation,
                change: SelectionChange {
                    agent_id: Some(AgentId::new("not-real")),
                    ..Default::default()
                },
            },
        )
        .await
        .expect_err("must reject");
        assert_eq!(error.code(), "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn selecting_a_model_the_agent_does_not_offer_is_rejected() {
        // Catching this at selection time gives a clear message instead of an
        // opaque CLI failure at spawn time.
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        let error = handle(
            &daemon,
            Request::Select {
                conversation_id: conversation,
                change: SelectionChange {
                    agent_id: Some(AgentId::new("claude")),
                    model: Some("gpt-5.6".into()),
                    reasoning: None,
                },
            },
        )
        .await
        .expect_err("must reject");
        assert!(error.to_string().contains("does not offer model"));
    }

    #[tokio::test]
    async fn a_valid_selection_is_persisted_and_clears_a_stale_model() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;

        handle(
            &daemon,
            Request::Select {
                conversation_id: conversation.clone(),
                change: SelectionChange {
                    agent_id: Some(AgentId::new("claude")),
                    model: Some("sonnet".into()),
                    reasoning: None,
                },
            },
        )
        .await
        .expect("select claude");

        // Switching agent must not carry `sonnet` into Codex.
        let response = handle(
            &daemon,
            Request::Select {
                conversation_id: conversation,
                change: SelectionChange {
                    agent_id: Some(AgentId::new("codex")),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("select codex");
        match response {
            Response::Conversation { summary, .. } => {
                assert_eq!(summary.selected_agent_id.as_deref(), Some("codex"));
                assert_eq!(summary.selected_model, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_invalid_reasoning_level_is_rejected_with_the_valid_set() {
        // Previously any string was accepted and only failed at spawn time.
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        handle(
            &daemon,
            Request::Select {
                conversation_id: conversation.clone(),
                change: SelectionChange {
                    agent_id: Some(AgentId::new("codex")),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("select codex");

        let error = handle(
            &daemon,
            Request::Select {
                conversation_id: conversation,
                change: SelectionChange {
                    reasoning: Some("bogus".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .expect_err("must reject");
        assert!(error.to_string().contains("not a reasoning level"));
    }

    #[tokio::test]
    async fn reasoning_is_rejected_for_an_agent_without_levels() {
        // Kiro exposes no reasoning levels. Claude does (`--effort`), so it is not
        // the right example here.
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        handle(
            &daemon,
            Request::Select {
                conversation_id: conversation.clone(),
                change: SelectionChange {
                    agent_id: Some(AgentId::new("kiro")),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("select kiro");

        let error = handle(
            &daemon,
            Request::Select {
                conversation_id: conversation,
                change: SelectionChange {
                    reasoning: Some("high".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .expect_err("must reject");
        assert!(error.to_string().contains("does not expose reasoning"));
    }

    #[tokio::test]
    async fn an_empty_delegated_task_is_rejected() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        let error = handle(
            &daemon,
            Request::Delegate {
                parent_conversation_id: conversation,
                parent_run_id: None,
                agent_id: AgentId::new("codex"),
                model: None,
                task: "   ".into(),
                timeout_ms: None,
            },
        )
        .await
        .expect_err("must reject");
        assert_eq!(error.code(), "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn delegating_to_an_unknown_agent_is_rejected() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        let error = handle(
            &daemon,
            Request::Delegate {
                parent_conversation_id: conversation,
                parent_run_id: None,
                agent_id: AgentId::new("not-real"),
                model: None,
                task: "do something".into(),
                timeout_ms: None,
            },
        )
        .await
        .expect_err("must reject");
        assert_eq!(error.code(), "INVALID_REQUEST");
    }

    #[test]
    fn a_subagent_keeps_the_users_mcp_servers() {
        // Previously a child got McpInjection::None, which also stripped the user's
        // own servers — delegating a task that needed one would silently fail.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ArgoPaths::with_root(dir.path().to_path_buf());
        std::fs::create_dir_all(paths.root()).expect("root");
        let mut registry = argo_resources::McpRegistry::default();
        registry
            .upsert(argo_resources::McpServer {
                name: "volrix".into(),
                transport: argo_resources::McpTransport::Remote {
                    url: "https://mcp.volrix.ai/mcp".into(),
                    headers: vec![],
                },
                enabled: true,
            })
            .expect("upsert");
        registry
            .save(&paths.root().join("mcp.json"))
            .expect("save registry");

        let workspace = dir.path().to_string_lossy().to_string();
        for offer_delegation in [true, false] {
            let (_, plan, _) = resolve_resources_with(
                &paths,
                &workspace,
                argo_core::runtime::McpInjection::ClaudeMcpJson,
                "conv-1",
                offer_delegation,
            );
            assert!(
                plan.names.iter().any(|n| n == "volrix"),
                "user server must reach a subagent (offer_delegation={offer_delegation})"
            );
            // Only the delegation tool itself is withheld at the cap.
            assert_eq!(
                plan.names.iter().any(|n| n == "argo"),
                offer_delegation,
                "delegation tool presence should follow offer_delegation"
            );
        }
    }

    #[tokio::test]
    async fn delegation_depth_is_bounded() {
        // Without a cap a subagent could delegate to a subagent indefinitely.
        let (daemon, dir) = daemon().await;
        // Scoped so the guard cannot be held across the await below.
        let current = {
            let store = daemon.store().expect("store");
            let workspace = store.ensure_workspace(dir.path()).expect("workspace");
            let mut current = store
                .create_conversation(&workspace, Some("root"))
                .expect("root");
            for depth in 0..MAX_DELEGATION_DEPTH {
                current = store
                    .create_child_conversation(
                        &workspace,
                        &current,
                        Some(&RunId::new("r")),
                        Some(&format!("depth {depth}")),
                    )
                    .expect("child");
            }
            current
        };

        let error = handle(
            &daemon,
            Request::Delegate {
                parent_conversation_id: current,
                parent_run_id: None,
                agent_id: AgentId::new("codex"),
                model: None,
                task: "go deeper".into(),
                timeout_ms: None,
            },
        )
        .await
        .expect_err("must refuse to nest further");
        assert!(error.to_string().contains("limit is"));
    }

    #[test]
    fn historical_messages_recover_structured_blocks_from_run_events() {
        let run_id = RunId::new("r1");
        let events = vec![
            RunEvent::new(
                run_id.clone(),
                1,
                RunEventKind::TextDelta {
                    text: "Submitting. ".into(),
                },
            ),
            RunEvent::new(
                run_id.clone(),
                2,
                RunEventKind::ToolStarted {
                    id: "t1".into(),
                    name: "run_backtest".into(),
                    input: Some("SENSEX".into()),
                },
            ),
            RunEvent::new(
                run_id.clone(),
                3,
                RunEventKind::ToolCompleted {
                    id: "t1".into(),
                    output: Some("runID=backtest-123".into()),
                    ok: true,
                },
            ),
            RunEvent::new(
                run_id.clone(),
                4,
                RunEventKind::TextDelta {
                    text: "Done.".into(),
                },
            ),
        ];
        let message = argo_core::message::Message {
            id: MessageId::new("m1"),
            role: argo_core::message::Role::Assistant,
            // Historical rows had only flattened prose despite richer run events.
            blocks: vec![ContentBlock::text("Submitting. Done.")],
            agent_id: Some(AgentId::new("antigravity")),
            model: Some("sonnet".into()),
            run_id: Some(run_id),
            seq: 1,
            created_at: 0,
        };

        let view = message_view(&message, &events);
        assert_eq!(view.blocks.len(), 3);
        assert!(matches!(
            &view.blocks[0],
            ContentBlock::Text { text } if text == "Submitting. "
        ));
        let ContentBlock::Tool { call } = &view.blocks[1] else {
            panic!("tool block was not recovered");
        };
        assert_eq!(call.name, "run_backtest");
        assert_eq!(call.output.as_deref(), Some("runID=backtest-123"));
        assert!(matches!(
            &view.blocks[2],
            ContentBlock::Text { text } if text == "Done."
        ));
    }

    #[test]
    fn native_child_activity_is_recovered_without_merging_into_parent_blocks() {
        let parent = RunId::new("parent");
        let child = RunId::new("claude-native-t1");
        let events = vec![
            RunEvent::new(
                parent.clone(),
                1,
                RunEventKind::ChildSpawned {
                    child_run_id: child.clone(),
                    child_agent_id: AgentId::new("claude/explore"),
                    task: "inspect parser".into(),
                    native: true,
                },
            ),
            RunEvent::new(
                parent.clone(),
                2,
                RunEventKind::ChildEvent {
                    child_run_id: child.clone(),
                    event: Box::new(RunEventKind::ThinkingDelta {
                        text: "checking".into(),
                    }),
                },
            ),
            RunEvent::new(
                parent.clone(),
                3,
                RunEventKind::ChildEvent {
                    child_run_id: child.clone(),
                    event: Box::new(RunEventKind::TextDelta {
                        text: "found it".into(),
                    }),
                },
            ),
            RunEvent::new(
                parent,
                4,
                RunEventKind::ChildCompleted {
                    child_run_id: child,
                    status: RunStatus::Succeeded,
                },
            ),
        ];

        let blocks = blocks_from_events(&events);
        assert_eq!(blocks.len(), 1);
        let ContentBlock::ChildActivity {
            agent_id,
            status,
            blocks,
            ..
        } = &blocks[0]
        else {
            panic!("expected child activity block");
        };
        assert_eq!(agent_id.as_str(), "claude/explore");
        assert_eq!(*status, Some(RunStatus::Succeeded));
        assert!(matches!(
            &blocks[0],
            ContentBlock::Thinking { text } if text == "checking"
        ));
        assert!(matches!(
            &blocks[1],
            ContentBlock::Text { text } if text == "found it"
        ));
    }

    #[test]
    fn child_titles_are_bounded() {
        assert_eq!(first_line("review the diff"), "review the diff");
        let long = "x".repeat(200);
        let title = first_line(&long);
        assert_eq!(title.chars().count(), 48);
        assert!(title.ends_with("..."));
        assert_eq!(first_line(""), "");
    }

    #[tokio::test]
    async fn empty_messages_are_rejected() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        let error = handle(
            &daemon,
            Request::SendMessage {
                conversation_id: conversation,
                prompt: "   ".into(),
            },
        )
        .await
        .expect_err("must reject");
        assert_eq!(error.code(), "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn cancelling_an_unknown_run_is_not_found() {
        let (daemon, _dir) = daemon().await;
        let error = handle(
            &daemon,
            Request::Cancel {
                run_id: RunId::new("ghost"),
            },
        )
        .await
        .expect_err("must fail");
        assert_eq!(error.code(), "NOT_FOUND");
    }

    #[tokio::test]
    async fn context_preview_explains_why_a_fresh_session_is_needed() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        handle(
            &daemon,
            Request::Select {
                conversation_id: conversation.clone(),
                change: SelectionChange {
                    agent_id: Some(AgentId::new("claude")),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("select");

        let response = handle(
            &daemon,
            Request::PreviewContext {
                conversation_id: conversation,
                prompt: "do the thing".into(),
            },
        )
        .await
        .expect("preview");

        match response {
            Response::ContextPreview { resuming, body, .. } => {
                // No prior session exists, so this is a fresh seed.
                assert!(!resuming);
                assert!(body.contains("do the thing"));
                assert!(body.contains(crate::STABLE_INSTRUCTIONS));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bootstrap_reconciles_runs_left_in_flight_by_a_crash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ArgoPaths::with_root(dir.path().join("data"));
        paths.ensure_dirs().expect("dirs");

        let run_id = {
            let store = Store::open(paths.database()).expect("store");
            let ws = store.ensure_workspace(dir.path()).expect("ws");
            let conv = store.create_conversation(&ws, None).expect("conv");
            let run = store
                .create_run(argo_store::NewRun {
                    conversation_id: conv,
                    workspace_id: ws,
                    agent_id: AgentId::new("claude"),
                    model: None,
                    resumed: false,
                    invalidation_reason: None,
                    parent_run_id: None,
                })
                .expect("run");
            store.mark_run_running(&run).expect("running");
            run
        };

        // Restart: the run has no live process behind it any more.
        let daemon = Daemon::bootstrap(paths.clone()).await.expect("bootstrap");
        let store = daemon.store().expect("store lock");
        let run = store.get_run(&run_id).expect("run");
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error_code.as_deref(), Some("RUN_INTERRUPTED"));
        assert!(store.list_unfinished_runs().expect("list").is_empty());
    }

    async fn new_conversation(daemon: &Arc<Daemon>, root: &std::path::Path) -> ConversationId {
        let response = handle(
            daemon,
            Request::NewConversation {
                root: root.to_string_lossy().to_string(),
                title: None,
            },
        )
        .await
        .expect("new conversation");
        match response {
            Response::Conversation { summary, .. } => summary.id,
            other => panic!("unexpected: {other:?}"),
        }
    }
}
