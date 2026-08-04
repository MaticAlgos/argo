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

    if let Err(error) = argo_resources::cleanup_legacy_workspace_cache(workspace) {
        tracing::warn!(%error, "could not remove legacy project-local skill cache");
    }

    let skills = match argo_resources::discover(workspace, &paths.user_skills(), home.as_deref()) {
        Ok(skills) => skills,
        Err(error) => {
            tracing::warn!(%error, "skill discovery failed; continuing without skills");
            Vec::new()
        }
    };

    // Skill copies are user-level cache data, not project state. Absolute paths
    // let every adapter read the same protected copy without creating `.argo` in
    // the workspace.
    let staged = match argo_resources::stage(&paths.staging().join("skills"), &skills) {
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
                "{} — {} (instructions: {})",
                entry.name,
                description,
                entry.instructions_path().display()
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

/// Resolves the optional per-turn deadline.
fn turn_timeout_ms() -> Option<u64> {
    std::env::var("ARGO_TURN_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
}

/// One accepted turn, reserved by conversation before execution starts.
struct ActiveRun {
    cancel: CancelToken,
    run_id: StdMutex<Option<RunId>>,
}

impl ActiveRun {
    fn new(cancel: CancelToken) -> Self {
        Self {
            cancel,
            run_id: StdMutex::new(None),
        }
    }

    fn announce(&self, run_id: &RunId) {
        if let Ok(mut current) = self.run_id.lock() {
            *current = Some(run_id.clone());
        }
    }

    fn matches(&self, run_id: &RunId) -> bool {
        self.run_id
            .lock()
            .map(|current| current.as_ref() == Some(run_id))
            .unwrap_or(false)
    }
}

/// Removes an active-turn reservation on every exit path, including errors.
struct ActiveRunRegistration {
    daemon: Arc<Daemon>,
    conversation_id: ConversationId,
    active: Arc<ActiveRun>,
}

impl ActiveRunRegistration {
    fn active(&self) -> Arc<ActiveRun> {
        Arc::clone(&self.active)
    }
}

impl Drop for ActiveRunRegistration {
    fn drop(&mut self) {
        self.daemon
            .unregister_active(&self.conversation_id, &self.active);
    }
}

/// Shared daemon state.
pub struct Daemon {
    store: SharedStore,
    paths: ArgoPaths,
    agents: Mutex<Option<Vec<AgentInfo>>>,
    /// Serializes deep probes so concurrent requests cannot launch duplicates.
    probe_lock: Mutex<()>,
    running: StdMutex<HashMap<ConversationId, Arc<ActiveRun>>>,
    events: broadcast::Sender<argo_core::event::RunEvent>,
    shutdown: broadcast::Sender<()>,
    /// Prepared Telegram authorization window, shared by its wait and cancel requests.
    pub(crate) telegram_link: Mutex<Option<crate::telegram::LinkAttempt>>,
}

impl Daemon {
    /// Builds daemon state over an existing store.
    pub fn new(store: Store, paths: ArgoPaths) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (shutdown, _) = broadcast::channel(1);
        // Initialize with lightweight filesystem-only discovery — no subprocesses.
        let agents = argo_runtime::discover_all_lightweight();
        Self {
            store: Arc::new(StdMutex::new(store)),
            paths,
            agents: Mutex::new(Some(agents)),
            probe_lock: Mutex::new(()),
            running: StdMutex::new(HashMap::new()),
            events,
            shutdown,
            telegram_link: Mutex::new(None),
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

    /// Reserves a conversation before any turn preparation or execution.
    fn register_active(
        self: &Arc<Self>,
        conversation_id: &ConversationId,
    ) -> Result<ActiveRunRegistration> {
        let mut running = self
            .running
            .lock()
            .map_err(|_| ArgoError::Store("active-run lock poisoned by a previous panic".into()))?;
        if running.contains_key(conversation_id) {
            return Err(ArgoError::Invalid(
                "a turn is already running for this conversation; cancel it or wait for it to finish"
                    .into(),
            ));
        }

        let active = Arc::new(ActiveRun::new(CancelToken::new()));
        running.insert(conversation_id.clone(), Arc::clone(&active));
        Ok(ActiveRunRegistration {
            daemon: Arc::clone(self),
            conversation_id: conversation_id.clone(),
            active,
        })
    }

    /// Releases a reservation if it still names this exact accepted turn.
    fn unregister_active(&self, conversation_id: &ConversationId, active: &Arc<ActiveRun>) {
        match self.running.lock() {
            Ok(mut running)
                if running
                    .get(conversation_id)
                    .is_some_and(|current| Arc::ptr_eq(current, active)) =>
            {
                running.remove(conversation_id);
            }
            Ok(_) => {}
            Err(_) => tracing::error!(
                conversation = %conversation_id,
                "active-run lock poisoned while cleaning up"
            ),
        }
    }

    /// Locks active-turn state for one brief operation.
    fn active_runs(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<ConversationId, Arc<ActiveRun>>>> {
        self.running
            .lock()
            .map_err(|_| ArgoError::Store("active-run lock poisoned by a previous panic".into()))
    }

    /// Cancels the active turn with this run id without re-entering dispatch.
    pub(crate) fn cancel_active_run(&self, run_id: &RunId) -> Result<bool> {
        let active = self
            .active_runs()?
            .values()
            .find(|active| active.matches(run_id))
            .cloned();
        if let Some(active) = active {
            active.cancel.cancel();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Locks the store for one brief operation.
    fn store(&self) -> Result<std::sync::MutexGuard<'_, Store>> {
        self.store
            .lock()
            .map_err(|_| ArgoError::Store("store lock poisoned by a previous panic".into()))
    }

    /// Follows every run event the daemon publishes.
    ///
    /// In-process subscribers get the same stream socket clients do, which is
    /// what lets the Telegram bridge mirror a turn the TUI is also watching.
    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<argo_core::event::RunEvent> {
        self.events.subscribe()
    }

    /// Notified when the daemon is shutting down.
    pub(crate) fn subscribe_shutdown(&self) -> broadcast::Receiver<()> {
        self.shutdown.subscribe()
    }

    /// Resolved data-directory layout.
    pub(crate) fn paths(&self) -> &ArgoPaths {
        &self.paths
    }

    /// Detected adapters. Refresh re-runs lightweight PATH-only discovery (never
    /// deep-probes all). Use `probe_agent` for deep-probing a single adapter.
    async fn agent_inventory(&self, refresh: bool) -> Vec<AgentInfo> {
        if refresh {
            let _probe = self.probe_lock.lock().await;
            let discovered = argo_runtime::discover_all_lightweight();
            *self.agents.lock().await = Some(discovered.clone());
            return discovered;
        }
        if let Some(cached) = self.agents.lock().await.clone() {
            return cached;
        }
        let discovered = argo_runtime::discover_all_lightweight();
        *self.agents.lock().await = Some(discovered.clone());
        discovered
    }

    /// Deep-probes a single adapter and updates that one cache entry.
    ///
    /// Prevents duplicate concurrent probes for the same agent. Returns the
    /// freshly probed `AgentInfo`.
    async fn probe_agent(&self, agent_id: &str, refresh: bool) -> Result<AgentInfo> {
        if !refresh {
            if let Some(info) = self
                .agents
                .lock()
                .await
                .as_ref()
                .and_then(|agents| agents.iter().find(|agent| agent.id == agent_id))
                .filter(|agent| agent.probed)
                .cloned()
            {
                return Ok(info);
            }
        }

        // Await the probe barrier, then re-check: another waiter may have filled
        // this exact entry while this task was suspended.
        let _probe = self.probe_lock.lock().await;
        if !refresh {
            if let Some(info) = self
                .agents
                .lock()
                .await
                .as_ref()
                .and_then(|agents| agents.iter().find(|agent| agent.id == agent_id))
                .filter(|agent| agent.probed)
                .cloned()
            {
                return Ok(info);
            }
        }

        let def = argo_runtime::require(agent_id)?;
        let probed = argo_runtime::detect_one(def).await;
        let mut inventory = self.agents.lock().await;
        let agents = inventory.get_or_insert_with(argo_runtime::discover_all_lightweight);
        if let Some(existing) = agents.iter_mut().find(|agent| agent.id == agent_id) {
            *existing = probed.clone();
        } else {
            agents.push(probed.clone());
        }
        Ok(probed)
    }

    /// Resolves which agent and model a conversation's next turn should use.
    ///
    /// Chooses from lightweight inventory, then deep-probes only the chosen
    /// adapter before returning its full info.
    async fn resolve_selection(
        &self,
        conversation: &Conversation,
    ) -> Result<(AgentInfo, Option<String>, Option<String>)> {
        let agents = self.agent_inventory(false).await;

        let chosen_id = match &conversation.selected_agent_id {
            Some(id) => {
                // Verify it exists in the registry.
                agents
                    .iter()
                    .find(|a| &a.id == id)
                    .ok_or_else(|| ArgoError::AgentUnavailable {
                        agent: id.clone(),
                        reason: "not present in the adapter registry".into(),
                    })?;
                id.clone()
            }
            None => agents
                .iter()
                .find(|a| a.available)
                .ok_or_else(|| ArgoError::AgentUnavailable {
                    agent: "any".into(),
                    reason: "no supported coding CLI was detected on PATH".into(),
                })?
                .id
                .clone(),
        };

        // Deep-probe only the chosen adapter to populate models/help/version.
        let chosen = self.probe_agent(&chosen_id, false).await?;

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
        let reasoning = conversation.selected_reasoning.clone().filter(|r| {
            chosen
                .reasoning_for(model.as_deref())
                .iter()
                .any(|option| &option.id == r)
        });

        Ok((chosen.clone(), model, reasoning))
    }

    /// Builds a summary for one conversation.
    async fn summarize(&self, conversation: &Conversation) -> Result<ConversationSummary> {
        let store = self.store()?;
        let messages = store.list_messages(&conversation.id)?;
        let user_prompts = messages
            .iter()
            .filter(|message| message.role == argo_core::message::Role::User)
            .map(argo_core::message::Message::transferable_text)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>();
        let description = argo_core::conversation_description(&user_prompts);
        let sessions = store.list_agent_sessions(&conversation.id)?;
        let workspace = Some(store.workspace_root(&conversation.workspace_id)?);
        Ok(ConversationSummary {
            id: conversation.id.clone(),
            title: conversation.title.clone(),
            description: (!description.is_empty()).then_some(description),
            selected_agent_id: conversation.selected_agent_id.clone(),
            selected_model: conversation.selected_model.clone(),
            selected_reasoning: conversation.selected_reasoning.clone(),
            selected_mode: conversation.selected_mode.clone(),
            selected_backup_agent_id: conversation.selected_backup_agent_id.clone(),
            selected_backup_model: conversation.selected_backup_model.clone(),
            selected_backup_reasoning: conversation.selected_backup_reasoning.clone(),
            message_count: messages.len(),
            agents_with_sessions: sessions.iter().map(|s| s.agent_id.to_string()).collect(),
            parent_conversation_id: conversation.parent_conversation_id.clone(),
            workspace,
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

    // Usage is set only for assistant messages whose run succeeded.
    let usage = if message.role == argo_core::message::Role::Assistant {
        events.iter().rev().find_map(|e| match &e.kind {
            RunEventKind::RunFinished {
                status: RunStatus::Succeeded,
                usage,
            } => Some(*usage),
            _ => None,
        })
    } else {
        None
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
        usage,
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

    // Started here rather than in bootstrap so it belongs to a real daemon
    // process; a no-op when Telegram was never set up.
    crate::telegram::spawn(Arc::clone(&daemon));

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
                        let response = Response::Error {
                            code: "PROTOCOL_MISMATCH".into(),
                            message: format!(
                                "client protocol v{protocol} does not match daemon v{IPC_PROTOCOL_VERSION}; restart both from the same build"
                            ),
                            retryable: true,
                        };
                        write_half
                            .write_all(response.encode().as_bytes())
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
pub(crate) async fn dispatch(daemon: &Arc<Daemon>, request: Request) -> Response {
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

        Request::ProbeAgent { agent_id, refresh } => {
            let agent = daemon.probe_agent(&agent_id, refresh).await?;
            Ok(Response::Agent { agent })
        }

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

        Request::ClearConversations { root } => {
            if !daemon.active_runs()?.is_empty() {
                return Err(ArgoError::Invalid(
                    "cannot clear history while an agent turn is running; cancel it first".into(),
                ));
            }
            let count = {
                let store = daemon.store()?;
                match root {
                    Some(root) => {
                        let workspace = store.ensure_workspace(&root)?;
                        store.clear_workspace_conversations(&workspace)?
                    }
                    None => store.clear_all_conversations()?,
                }
            };
            Ok(Response::Cleared { count })
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
            // Plan is owned by Argo and enforced through the composed turn. More
            // permissive/specialized modes still require verified adapter support.
            if let Some(requested) = &mode {
                let parsed = argo_core::mode::AgentMode::parse(requested).ok_or_else(|| {
                    ArgoError::Invalid(format!(
                        "unknown mode '{requested}'. Available: full, plan, accept-edits, read-only"
                    ))
                })?;
                let conversation = daemon.store()?.get_conversation(&conversation_id)?;
                let (agent, _, _) = daemon.resolve_selection(&conversation).await?;
                let def = argo_runtime::require(&agent.id)?;
                let support = def.capabilities.modes.with_argo_plan();
                if !support.supports(parsed) {
                    let available: Vec<&str> = def
                        .capabilities
                        .modes
                        .with_argo_plan()
                        .available()
                        .iter()
                        .map(|m| m.id())
                        .collect();
                    return Err(ArgoError::Invalid(format!(
                        "Argo cannot enforce '{}' with {}. Available: {}",
                        parsed.id(),
                        def.name,
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

        Request::TelegramStatus
        | Request::TelegramConnect { .. }
        | Request::TelegramPrepareLink { .. }
        | Request::TelegramLink { .. }
        | Request::TelegramCancelLink { .. }
        | Request::TelegramAllowWorkspace { .. }
        | Request::TelegramAllowUser { .. }
        | Request::TelegramStart
        | Request::TelegramRemove => crate::telegram::handle(daemon, request).await,

        Request::SetBackupAgent {
            conversation_id,
            agent_id,
            model,
            reasoning,
        } => {
            // Validated here rather than at failover time: discovering the standby
            // was never installed at the moment the primary runs dry is the worst
            // possible time to find out.
            if let Some(requested) = &agent_id {
                let conversation = daemon.store()?.get_conversation(&conversation_id)?;
                if conversation.selected_agent_id.as_deref() == Some(requested.as_str()) {
                    return Err(ArgoError::Invalid(format!(
                        "{requested} is already this conversation's agent; a backup must be a different CLI"
                    )));
                }
                validate_agent_route(daemon, requested, model.as_deref(), reasoning.as_deref())
                    .await?;
            } else if model.is_some() || reasoning.is_some() {
                return Err(ArgoError::Invalid(
                    "a backup model or reasoning level requires a backup agent".into(),
                ));
            }
            let conversation = {
                let store = daemon.store()?;
                store.set_backup_agent(
                    &conversation_id,
                    agent_id.as_deref(),
                    model.as_deref(),
                    reasoning.as_deref(),
                )?;
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
            if daemon.cancel_active_run(&run_id)? {
                Ok(Response::Ok)
            } else {
                Err(ArgoError::not_found("active run", run_id.as_str()))
            }
        }

        Request::ListChildren { conversation_id } => {
            // Breadth-first traversal exposes the complete orchestration graph,
            // including agents delegated by another delegated agent.
            let descendants = {
                let store = daemon.store()?;
                let mut pending = std::collections::VecDeque::from([conversation_id]);
                let mut seen = std::collections::HashSet::new();
                let mut descendants = Vec::new();
                while let Some(parent) = pending.pop_front() {
                    for child in store.list_child_conversations(&parent)? {
                        if seen.insert(child.id.clone()) {
                            pending.push_back(child.id.clone());
                            descendants.push(child);
                        }
                    }
                }
                descendants
            };
            let mut summaries = Vec::new();
            for child in &descendants {
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

        Request::Compact { conversation_id } => compact(daemon, conversation_id).await,

        Request::Shutdown => {
            // Stop in-flight turns so children are signalled rather than orphaned.
            for active in daemon.active_runs()?.values() {
                active.cancel.cancel();
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
async fn validate_agent_route(
    daemon: &Arc<Daemon>,
    agent_id: &str,
    model: Option<&str>,
    reasoning: Option<&str>,
) -> Result<()> {
    let def = argo_runtime::require(agent_id)?;

    // Explicit selection is consent to deep-probe this one adapter. Errors and
    // unavailable state remain actionable instead of becoming an empty model list.
    let info = daemon.probe_agent(agent_id, false).await?;
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

    if let Some(model) = model {
        if !info.models.iter().any(|candidate| candidate.id == model) {
            let available: Vec<&str> = info
                .models
                .iter()
                .map(|candidate| candidate.id.as_str())
                .take(12)
                .collect();
            return Err(ArgoError::Invalid(format!(
                "{} does not offer model '{model}'. Available: {}",
                def.name,
                available.join(", ")
            )));
        }
    }

    if let Some(reasoning) = reasoning {
        let levels = info.reasoning_for(model);
        if levels.is_empty() {
            return Err(ArgoError::Invalid(format!(
                "{} does not expose reasoning levels",
                def.name
            )));
        }
        if !levels.iter().any(|level| level.id == reasoning) {
            let available: Vec<&str> = levels.iter().map(|level| level.id.as_str()).collect();
            return Err(ArgoError::Invalid(format!(
                "'{reasoning}' is not a reasoning level for {}. Available: {}",
                model.unwrap_or(def.name),
                available.join(", ")
            )));
        }
    }
    Ok(())
}

async fn validate_selection(
    daemon: &Arc<Daemon>,
    conversation: &Conversation,
    change: &SelectionChange,
) -> Result<()> {
    // The agent this change resolves to: the new one, or the one already selected.
    let agent_id = change
        .agent_id
        .as_ref()
        .map(|agent| agent.to_string())
        .or_else(|| conversation.selected_agent_id.clone());

    let Some(agent_id) = agent_id else {
        // Nothing selected yet and none named: the turn will pick a default.
        return Ok(());
    };
    let agent_changed = change
        .agent_id
        .as_ref()
        .is_some_and(|agent| Some(agent.as_str()) != conversation.selected_agent_id.as_deref());
    let model = change.model.as_deref().or_else(|| {
        (!agent_changed)
            .then_some(conversation.selected_model.as_deref())
            .flatten()
    });
    validate_agent_route(daemon, &agent_id, model, change.reasoning.as_deref()).await
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
        help_flags: agent.help_flags.clone(),
        active_skills: skill_names,
        active_mcp_servers: mcp_plan.names.clone(),
        project_instructions: resolve_instructions(&cwd),
        mcp_descriptors: descriptors,
        mcp_config: mcp_plan
            .config_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        mcp_overrides: mcp_plan.config_overrides.clone(),
        mcp_environment: mcp_plan.environment.clone(),
        timeout_ms: turn_timeout_ms(),
        mode: conversation
            .selected_mode
            .as_deref()
            .and_then(argo_core::mode::AgentMode::parse)
            .unwrap_or_default(),
        append_user: true,
        // A preview never runs, so it needs no standby.
        failover: None,
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

/// Folds a conversation's history into a summary and forces a fresh context.
///
/// Three things happen together, and all three are required for `/compact` to
/// mean anything:
///
/// 1. An epoch records the boundary and the summary standing in for it, so future
///    projections replay the summary instead of the messages.
/// 2. Every native session handle is dropped, because a vendor CLI holding its
///    own copy of the history would keep answering from the uncompacted version
///    and the reduced projection would never be sent.
/// 3. The canonical rows are left untouched, so the transcript stays readable and
///    a later build can always reconstruct what was folded away.
///
/// The summary is mechanical rather than model-written: it states what was
/// omitted without inventing detail, which is the same guarantee automatic
/// budget compaction already gives.
async fn compact(daemon: &Arc<Daemon>, conversation_id: ConversationId) -> Result<Response> {
    // Hold the same gate used to reserve sends until the epoch is durable. A
    // send can therefore happen wholly before or wholly after compaction, never
    // against a partially compacted context.
    let running = daemon.active_runs()?;
    if running.contains_key(&conversation_id) {
        return Err(ArgoError::Invalid(
            "a turn is still running; cancel it or let it finish before compacting".into(),
        ));
    }

    let mut store = daemon.store()?;
    // Confirms the conversation exists before anything is written.
    store.get_conversation(&conversation_id)?;

    let previous = store.latest_context_epoch(&conversation_id)?;
    let already_compacted = previous.as_ref().map(|e| e.compacted_upto).unwrap_or(0);

    let Some(upto) = store.max_message_seq(&conversation_id)? else {
        return Err(ArgoError::Invalid(
            "this conversation has no messages to compact".into(),
        ));
    };
    if upto <= already_compacted {
        return Err(ArgoError::Invalid(
            "nothing new to compact since the last compaction".into(),
        ));
    }

    let folded: Vec<argo_core::message::Message> = store
        .list_messages(&conversation_id)?
        .into_iter()
        .filter(|m| m.seq > already_compacted && m.seq <= upto)
        .collect();

    // Carried forward rather than replaced: compacting twice must not discard the
    // outline of what the first compaction folded away.
    let summary = match previous.and_then(|e| e.summary) {
        Some(earlier) => format!(
            "{earlier}\n\n{}",
            argo_context::budget::fallback_summary(&folded)
        ),
        None => argo_context::budget::fallback_summary(&folded),
    };

    let (_, sessions_cleared) =
        store.compact_context(&conversation_id, Some(&summary), upto, "manual")?;
    drop(store);
    drop(running);

    tracing::info!(
        conversation = %conversation_id,
        compacted_upto = upto,
        messages = folded.len(),
        "conversation compacted on request"
    );

    Ok(Response::Compacted {
        compacted_upto: upto,
        messages_compacted: folded.len(),
        sessions_cleared,
        summary,
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
    // Deep-probe the delegate target so help_flags and models are available.
    let info = daemon.probe_agent(agent_id.as_str(), false).await?;
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
    let registration = daemon.register_active(&child_conversation)?;
    let active_run = registration.active();
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
        help_flags: info.help_flags.clone(),
        active_skills: skill_names,
        active_mcp_servers: mcp_plan.names.clone(),
        project_instructions: resolve_instructions(&workspace_root),
        mcp_descriptors: descriptors,
        mcp_config: mcp_plan
            .config_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        mcp_overrides: mcp_plan.config_overrides.clone(),
        mcp_environment: mcp_plan.environment.clone(),
        timeout_ms: timeout_ms.or_else(turn_timeout_ms),
        // A subagent inherits full authority: it was asked to do work, not plan.
        mode: argo_core::mode::AgentMode::Full,
        append_user: true,
        // Delegation names its agent explicitly; silently answering as a different
        // CLI would contradict what the delegating agent asked for.
        failover: None,
    };

    let cancel = active_run.cancel.clone();
    let events = daemon.events.clone();
    let store = Arc::clone(&daemon.store);
    let lifecycle_parent = parent_run_id.clone();
    let lifecycle_agent = agent_id.clone();
    let lifecycle_task = task.clone();
    let daemon_for_listener = Arc::clone(daemon);
    let active_conversation = child_conversation.clone();
    let active_for_cleanup = Arc::clone(&active_run);
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

        active_run.announce(&event.run_id);
        let terminal = match &event.kind {
            RunEventKind::RunFinished { status, .. } => Some(*status),
            _ => None,
        };
        let child_run_id = event.run_id.clone();
        if terminal.is_some() {
            // RunFinished is the queue commit barrier. Release before publishing
            // it so an immediate queued send is accepted.
            daemon_for_listener.unregister_active(&active_conversation, &active_for_cleanup);
        }
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

    let outcome = run_turn(&daemon.store, &daemon.paths, turn, &cancel, Some(&listener)).await;
    drop(registration);
    let outcome = outcome?;

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

/// Resolves the conversation's standby agent into a ready-to-run plan.
///
/// Returns `None` when no backup is configured, when it names the agent already
/// running the turn, or when it cannot be resolved — a failed lookup must
/// degrade to today's behaviour rather than break an otherwise valid turn.
async fn resolve_failover(
    daemon: &Arc<Daemon>,
    conversation: &Conversation,
    workspace_root: &str,
) -> Option<crate::engine::FailoverPlan> {
    let backup = conversation.selected_backup_agent_id.as_deref()?;
    if conversation.selected_agent_id.as_deref() == Some(backup) {
        return None;
    }

    // Resolved through a conversation whose primary selection *is* the backup,
    // carrying the standby's own recorded model and effort. Reusing
    // `resolve_selection` keeps one code path for validating a routing target.
    let probe = Conversation {
        selected_agent_id: Some(backup.to_string()),
        selected_model: conversation.selected_backup_model.clone(),
        selected_reasoning: conversation.selected_backup_reasoning.clone(),
        ..conversation.clone()
    };
    let (info, model, reasoning) = match daemon.resolve_selection(&probe).await {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(%error, backup, "backup agent is unavailable; failover disabled for this turn");
            return None;
        }
    };
    let def = match argo_runtime::require(&info.id) {
        Ok(def) => def,
        Err(error) => {
            tracing::warn!(%error, backup, "backup agent is not in the registry");
            return None;
        }
    };

    let (skills, mcp_plan, descriptors) = resolve_resources(
        &daemon.paths,
        workspace_root,
        def.capabilities.mcp_injection,
        conversation.id.as_str(),
    );

    Some(crate::engine::FailoverPlan {
        agent_id: AgentId::new(&info.id),
        model,
        reasoning,
        bin: info.path.clone().unwrap_or_else(|| def.bin.to_string()),
        help_flags: info.help_flags.clone(),
        active_skills: skills,
        active_mcp_servers: mcp_plan.names.clone(),
        mcp_descriptors: descriptors,
        mcp_config: mcp_plan
            .config_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        mcp_overrides: mcp_plan.config_overrides.clone(),
        mcp_environment: mcp_plan.environment.clone(),
    })
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

    // Reserve first: all preparation observes one coherent context boundary, and
    // every `?` below releases the reservation through `Drop`.
    let registration = daemon.register_active(&conversation_id)?;
    let active_run = registration.active();
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
    let captured = argo_resources::instructions::capture_user_directives(
        std::path::Path::new(&workspace_root),
        &prompt,
    )?;
    if !captured.is_empty() {
        tracing::info!(
            count = captured.len(),
            workspace = %workspace_root,
            "captured durable project instructions"
        );
    }
    let (skill_names, mcp_plan, descriptors) = resolve_resources(
        &daemon.paths,
        &workspace_root,
        def.capabilities.mcp_injection,
        conversation_id.as_str(),
    );

    // Resolved before the turn starts so a missing or broken standby surfaces as a
    // log line now rather than as a second failure at the worst moment. Costs
    // nothing for conversations that never configured one.
    let failover = resolve_failover(daemon, &conversation, &workspace_root).await;

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
        help_flags: agent.help_flags.clone(),
        active_skills: skill_names,
        active_mcp_servers: mcp_plan.names.clone(),
        project_instructions: resolve_instructions(&workspace_root),
        mcp_descriptors: descriptors,
        mcp_config: mcp_plan
            .config_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        mcp_overrides: mcp_plan.config_overrides.clone(),
        mcp_environment: mcp_plan.environment.clone(),
        timeout_ms: turn_timeout_ms(),
        mode: conversation
            .selected_mode
            .as_deref()
            .and_then(argo_core::mode::AgentMode::parse)
            .unwrap_or_default(),
        append_user: true,
        failover,
    };

    // The run row must exist before this call returns so the client can subscribe
    // without racing the spawn.
    let daemon_for_turn = Arc::clone(daemon);
    let active_conversation_for_turn = conversation_id_for_summary.clone();
    let cancel_for_turn = active_run.cancel.clone();
    let agent_id = agent.id.clone();

    let (tx, rx) = tokio::sync::oneshot::channel::<RunId>();

    tokio::spawn(async move {
        // Ownership moves to the task so every terminal or error path unregisters.
        let _registration = registration;
        let events = daemon_for_turn.events.clone();
        let daemon_for_listener = Arc::clone(&daemon_for_turn);
        let active_for_cleanup = Arc::clone(&active_run);
        // The first event carries the run id, which both announces the run to the
        // waiting caller and fans out to any subscriber.
        let announce = std::sync::Mutex::new(Some(tx));
        let listener = move |event: argo_core::event::RunEvent| {
            active_run.announce(&event.run_id);
            if let Ok(mut guard) = announce.lock() {
                if let Some(sender) = guard.take() {
                    let _ = sender.send(event.run_id.clone());
                }
            }
            if event.is_terminal() {
                // RunFinished is the queue commit barrier. Release before it is
                // visible so the next FIFO send cannot be spuriously rejected.
                daemon_for_listener
                    .unregister_active(&active_conversation_for_turn, &active_for_cleanup);
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

        if let Err(error) = &result {
            tracing::warn!(%error, agent = %agent_id, "turn failed");
        }
    });

    let run_id = rx
        .await
        .map_err(|_| ArgoError::Process("turn did not start".into()))?;

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
                info.probed = true;
                info.diagnostics.clear();
                info
            })
            .collect();
        *daemon.agents.lock().await = Some(agents);

        (Arc::new(daemon), dir)
    }

    #[test]
    fn turn_timeout_is_unlimited_by_default_and_can_be_set_explicitly() {
        // Missing or malformed values must not impose a default deadline. Only a
        // positive value opts into a bound; 0 remains an explicit no-deadline value.
        std::env::remove_var("ARGO_TURN_TIMEOUT_MS");
        assert_eq!(turn_timeout_ms(), None);
        std::env::set_var("ARGO_TURN_TIMEOUT_MS", "5000");
        assert_eq!(turn_timeout_ms(), Some(5_000));
        std::env::set_var("ARGO_TURN_TIMEOUT_MS", "0");
        assert_eq!(turn_timeout_ms(), None);
        std::env::set_var("ARGO_TURN_TIMEOUT_MS", "not-a-number");
        assert_eq!(turn_timeout_ms(), None);
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
    async fn list_agents_uses_only_lightweight_inventory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ArgoPaths::with_root(dir.path().join("data"));
        let daemon = Arc::new(Daemon::bootstrap(paths).await.expect("bootstrap"));
        let response = handle(&daemon, Request::ListAgents { refresh: true })
            .await
            .expect("list agents");
        match response {
            Response::Agents { agents } => {
                assert_eq!(agents.len(), argo_runtime::ADAPTERS.len());
                assert!(agents.iter().all(|agent| !agent.probed));
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
    async fn conversation_summary_describes_start_and_current_focus() {
        let (daemon, dir) = daemon().await;
        let id = new_conversation(&daemon, dir.path()).await;
        {
            let store = daemon.store().expect("store");
            store
                .append_message(&id, argo_store::NewMessage::user("build login"))
                .expect("first prompt");
            store
                .append_message(
                    &id,
                    argo_store::NewMessage::user("now fix keyboard shortcuts"),
                )
                .expect("latest prompt");
            store
                .set_title(
                    &id,
                    &argo_core::conversation_title("now fix keyboard shortcuts"),
                )
                .expect("title");
        }
        let conversation = daemon
            .store()
            .expect("store")
            .get_conversation(&id)
            .expect("conversation");
        let summary = daemon.summarize(&conversation).await.expect("summary");
        assert_eq!(summary.title.as_deref(), Some("now fix keyboard shortcuts"));
        let description = summary.description.expect("description");
        assert!(description.contains("Started with: build login"));
        assert!(description.contains("Current focus: now fix keyboard shortcuts"));
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
        // OpenCode exposes no independent effort control. Kiro and Claude do, so
        // neither is the right example here.
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        handle(
            &daemon,
            Request::Select {
                conversation_id: conversation.clone(),
                change: SelectionChange {
                    agent_id: Some(AgentId::new("opencode")),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("select opencode");

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
    async fn argo_plan_mode_is_available_without_a_native_cli_mode() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        handle(
            &daemon,
            Request::Select {
                conversation_id: conversation.clone(),
                change: SelectionChange {
                    agent_id: Some(AgentId::new("grok")),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("select grok");

        let response = handle(
            &daemon,
            Request::SetMode {
                conversation_id: conversation,
                mode: Some("plan".into()),
            },
        )
        .await
        .expect("Argo-owned plan mode");
        match response {
            Response::Conversation { summary, .. } => {
                assert_eq!(summary.selected_mode.as_deref(), Some("plan"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_backup_agent_round_trips_and_is_validated_up_front() {
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
        .expect("select claude");

        let response = handle(
            &daemon,
            Request::SetBackupAgent {
                conversation_id: conversation.clone(),
                agent_id: Some("codex".into()),
                model: None,
                reasoning: None,
            },
        )
        .await
        .expect("set backup");
        match response {
            Response::Conversation { summary, .. } => {
                assert_eq!(summary.selected_agent_id.as_deref(), Some("claude"));
                assert_eq!(summary.selected_backup_agent_id.as_deref(), Some("codex"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        // An unknown standby is refused now rather than at the moment the primary
        // runs dry, when it would be far more expensive to discover.
        let error = handle(
            &daemon,
            Request::SetBackupAgent {
                conversation_id: conversation.clone(),
                agent_id: Some("not-a-cli".into()),
                model: None,
                reasoning: None,
            },
        )
        .await
        .expect_err("must reject unknown");
        assert_eq!(error.code(), "INVALID_REQUEST");

        // Standing by for yourself is not failover.
        let error = handle(
            &daemon,
            Request::SetBackupAgent {
                conversation_id: conversation.clone(),
                agent_id: Some("claude".into()),
                model: None,
                reasoning: None,
            },
        )
        .await
        .expect_err("must reject self");
        assert!(error.to_string().contains("different CLI"));

        let error = handle(
            &daemon,
            Request::SetBackupAgent {
                conversation_id: conversation.clone(),
                agent_id: Some("codex".into()),
                model: Some("not-a-codex-model".into()),
                reasoning: None,
            },
        )
        .await
        .expect_err("must reject unknown backup model");
        assert!(error.to_string().contains("does not offer model"));

        let codex_model = daemon
            .probe_agent("codex", false)
            .await
            .expect("probe fixture")
            .models
            .first()
            .expect("fallback model")
            .id
            .clone();
        let error = handle(
            &daemon,
            Request::SetBackupAgent {
                conversation_id: conversation.clone(),
                agent_id: Some("codex".into()),
                model: Some(codex_model),
                reasoning: Some("not-a-level".into()),
            },
        )
        .await
        .expect_err("must reject invalid backup reasoning");
        assert!(error.to_string().contains("reasoning"));

        {
            let mut agents = daemon.agents.lock().await;
            let codex = agents
                .as_mut()
                .and_then(|agents| agents.iter_mut().find(|agent| agent.id == "codex"))
                .expect("codex fixture");
            codex.available = false;
            codex.diagnostics = vec!["not installed for test".into()];
        }
        let error = handle(
            &daemon,
            Request::SetBackupAgent {
                conversation_id: conversation.clone(),
                agent_id: Some("codex".into()),
                model: None,
                reasoning: None,
            },
        )
        .await
        .expect_err("must reject unavailable backup");
        assert_eq!(error.code(), "AGENT_UNAVAILABLE");

        handle(
            &daemon,
            Request::SetBackupAgent {
                conversation_id: conversation.clone(),
                agent_id: None,
                model: None,
                reasoning: None,
            },
        )
        .await
        .expect("clear backup");
        let conversation = daemon
            .store()
            .expect("store")
            .get_conversation(&conversation)
            .expect("load");
        assert!(conversation.selected_backup_agent_id.is_none());
    }

    #[tokio::test]
    async fn failover_resolution_skips_unusable_or_redundant_standbys() {
        let (daemon, dir) = daemon().await;
        let conversation_id = new_conversation(&daemon, dir.path()).await;
        let root = dir.path().to_string_lossy().to_string();
        let load = |daemon: &Arc<Daemon>| {
            daemon
                .store()
                .expect("store")
                .get_conversation(&conversation_id)
                .expect("load")
        };

        // No standby configured: the overwhelming common case must cost nothing
        // and change nothing.
        assert!(resolve_failover(&daemon, &load(&daemon), &root)
            .await
            .is_none());

        daemon
            .store()
            .expect("store")
            .set_backup_agent(&conversation_id, Some("codex"), None, None)
            .expect("set backup");
        let plan = resolve_failover(&daemon, &load(&daemon), &root)
            .await
            .expect("standby resolves");
        assert_eq!(plan.agent_id, AgentId::new("codex"));
        // The primary's model must not follow the turn across: model ids are not
        // portable between CLIs.
        assert!(plan.model.is_none() || plan.model.as_deref() != Some("sonnet"));

        // A standby configured with its own model must actually run on it,
        // otherwise the choice is recorded and silently ignored.
        daemon
            .store()
            .expect("store")
            .set_backup_agent(&conversation_id, Some("codex"), Some("gpt-5.6-sol"), None)
            .expect("set backup routing");
        let routed = resolve_failover(&daemon, &load(&daemon), &root)
            .await
            .expect("standby resolves");
        assert_eq!(routed.model.as_deref(), Some("gpt-5.6-sol"));

        // Once the conversation is already on the standby there is nothing to fail
        // over to, so resolution must decline rather than hand a CLI to itself.
        handle(
            &daemon,
            Request::Select {
                conversation_id: conversation_id.clone(),
                change: SelectionChange {
                    agent_id: Some(AgentId::new("codex")),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("select codex");
        assert!(resolve_failover(&daemon, &load(&daemon), &root)
            .await
            .is_none());
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

        // A failure after reservation but before run_turn starts must clean up.
        let missing = handle(
            &daemon,
            Request::SendMessage {
                conversation_id: ConversationId::new("missing"),
                prompt: "valid prompt".into(),
            },
        )
        .await
        .expect_err("missing conversation");
        assert_eq!(missing.code(), "NOT_FOUND");
        assert!(daemon.active_runs().expect("running").is_empty());
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
    async fn one_active_turn_per_conversation_blocks_sends_and_compaction() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        daemon
            .store()
            .expect("store")
            .append_message(
                &conversation,
                argo_store::NewMessage::user("existing history"),
            )
            .expect("append");

        let registration = daemon.register_active(&conversation).expect("reserve");
        assert_eq!(daemon.active_runs().expect("running").len(), 1);

        let duplicate = handle(
            &daemon,
            Request::SendMessage {
                conversation_id: conversation.clone(),
                prompt: "second client".into(),
            },
        )
        .await
        .expect_err("second send must be rejected");
        assert!(duplicate.to_string().contains("already running"));

        let compacting = handle(
            &daemon,
            Request::Compact {
                conversation_id: conversation.clone(),
            },
        )
        .await
        .expect_err("compaction must wait for the send");
        assert!(compacting.to_string().contains("still running"));

        // Registration is RAII-owned, so an early error path cannot leave the
        // conversation permanently busy.
        drop(registration);
        assert!(daemon.active_runs().expect("running").is_empty());

        // Terminal publication removes the reservation before the guard itself
        // drops, matching the point at which FIFO clients send their next turn.
        let registration = daemon
            .register_active(&conversation)
            .expect("reserve again");
        let active = registration.active();
        daemon.unregister_active(&conversation, &active);
        assert!(daemon.active_runs().expect("running").is_empty());
        drop(registration);

        handle(
            &daemon,
            Request::Compact {
                conversation_id: conversation,
            },
        )
        .await
        .expect("compaction after cleanup");
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

    #[tokio::test]
    async fn compact_folds_history_drops_sessions_and_keeps_the_transcript() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;

        // A conversation with real history and a live native handle, which is the
        // only situation where compaction has anything to do.
        {
            let store = daemon.store().expect("store");
            for i in 0..6 {
                store
                    .append_message(
                        &conversation,
                        argo_store::NewMessage::user(format!("request {i}")),
                    )
                    .expect("append");
            }
            store
                .upsert_agent_session(
                    &conversation,
                    &argo_core::session::AgentSessionRecord {
                        agent_id: AgentId::new("claude"),
                        session_id: argo_core::ids::SessionId::new("sess-1"),
                        model: None,
                        cwd: None,
                        stable_hash: None,
                        last_message_id: None,
                        updated_at: 0,
                    },
                )
                .expect("session");
        }

        let response = handle(
            &daemon,
            Request::Compact {
                conversation_id: conversation.clone(),
            },
        )
        .await
        .expect("compact");

        let Response::Compacted {
            compacted_upto,
            messages_compacted,
            sessions_cleared,
            summary,
        } = response
        else {
            panic!("unexpected: {response:?}");
        };
        assert_eq!(compacted_upto, 6);
        assert_eq!(messages_compacted, 6);
        // The vendor session must go, or the reduced projection is never sent.
        assert_eq!(sessions_cleared, 1);
        assert!(summary.contains("6 earlier message(s) omitted"));

        // Canonical history is untouched: /compact changes the projection only.
        {
            let store = daemon.store().expect("store");
            assert_eq!(store.list_messages(&conversation).expect("list").len(), 6);
            assert!(store
                .list_agent_sessions(&conversation)
                .expect("s")
                .is_empty());
        }

        // The next turn's projection replays the summary instead of the messages.
        let preview = handle(
            &daemon,
            Request::PreviewContext {
                conversation_id: conversation.clone(),
                prompt: "what next".into(),
            },
        )
        .await
        .expect("preview");
        let Response::ContextPreview { body, resuming, .. } = preview else {
            panic!("unexpected: {preview:?}");
        };
        assert!(!resuming, "a compacted conversation must reseed");
        assert!(body.contains("6 earlier message(s) omitted"));
        // None of the folded turns are replayed verbatim any more.
        for i in 0..6 {
            assert!(
                !body.contains(&format!("request {i}")),
                "request {i} should have been folded away:\n{body}"
            );
        }
    }

    #[tokio::test]
    async fn compact_folds_the_full_prefix_beyond_two_hundred_messages() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;
        const MESSAGE_COUNT: usize = 250;

        {
            let store = daemon.store().expect("store");
            for i in 0..MESSAGE_COUNT {
                store
                    .append_message(
                        &conversation,
                        argo_store::NewMessage::user(format!("request {i}")),
                    )
                    .expect("append");
            }
        }

        let response = handle(
            &daemon,
            Request::Compact {
                conversation_id: conversation.clone(),
            },
        )
        .await
        .expect("compact");
        let Response::Compacted {
            compacted_upto,
            messages_compacted,
            summary,
            ..
        } = response
        else {
            panic!("unexpected: {response:?}");
        };
        assert_eq!(compacted_upto, MESSAGE_COUNT as i64);
        assert_eq!(messages_compacted, MESSAGE_COUNT);
        assert!(summary.contains("250 earlier message(s) omitted"));
        assert_eq!(
            daemon
                .store()
                .expect("store")
                .list_messages(&conversation)
                .expect("messages")
                .len(),
            MESSAGE_COUNT,
            "manual compaction must preserve canonical messages"
        );
    }

    #[tokio::test]
    async fn compact_refuses_when_there_is_nothing_new_to_fold() {
        let (daemon, dir) = daemon().await;
        let conversation = new_conversation(&daemon, dir.path()).await;

        // Empty conversation: nothing to summarize.
        let empty = handle(
            &daemon,
            Request::Compact {
                conversation_id: conversation.clone(),
            },
        )
        .await;
        assert!(empty.is_err(), "expected a refusal, got {empty:?}");

        daemon
            .store()
            .expect("store")
            .append_message(&conversation, argo_store::NewMessage::user("only turn"))
            .expect("append");
        handle(
            &daemon,
            Request::Compact {
                conversation_id: conversation.clone(),
            },
        )
        .await
        .expect("first compaction");

        // Second attempt with no new messages is a refusal rather than an epoch
        // that summarizes nothing and needlessly drops sessions again.
        let repeat = handle(
            &daemon,
            Request::Compact {
                conversation_id: conversation.clone(),
            },
        )
        .await;
        assert!(repeat.is_err(), "expected a refusal, got {repeat:?}");
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
