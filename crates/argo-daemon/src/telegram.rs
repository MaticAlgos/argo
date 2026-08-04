//! The Telegram bridge.
//!
//! Runs inside the daemon and speaks the ordinary request protocol in-process,
//! so a phone is just another client: the TUI keeps mirroring the same
//! conversation live, and there is one place that owns conversation state.
//!
//! Everything reachable from a chat message is deliberately enumerated here.
//! Telegram is the one surface Argo exposes beyond the local socket, and the
//! agent behind it has full access to the machine, so the bridge issues a fixed
//! set of requests and never turns message text into a filesystem path.

use crate::protocol::{ConversationSummary, MessageView, Request, Response};
use crate::server::{dispatch, Daemon};
use argo_core::error::Result;
use argo_core::event::{RunEventKind, RunStatus};
use argo_core::ids::{ConversationId, RunId};
use argo_runtime::exec::CancelToken;
use argo_telegram::bot::{
    is_parse_entity_error, Bot, CallbackQuery, IncomingMessage, KeyboardRow, ParseMode, Update,
};
use argo_telegram::config::{self, TelegramConfig};
use argo_telegram::render::{self, Recap, ToolProgress};
use argo_telegram::split::{
    plain_text, safe_message_chunk, split_message_safe, utf16_len, MessageChunk, MAX_MESSAGE_CHARS,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Long-poll window. Long enough that the loop is mostly idle, short enough that
/// a shutdown is noticed promptly.
const POLL_SECS: u64 = 25;

/// One prepared authorization window shared across prepare, wait, and cancel requests.
#[derive(Debug, Clone)]
pub(crate) struct LinkAttempt {
    challenge: String,
    token: String,
    cancel: CancelToken,
}

impl LinkAttempt {
    fn matches(&self, challenge: &str) -> bool {
        self.challenge == challenge
    }
}

/// Minimum gap between edits of the same streaming bubble.
/// Telegram throttles aggressively per chat; editing on every delta would spend
/// the whole budget on a message nobody has finished reading.
const EDIT_INTERVAL: Duration = Duration::from_millis(2_000);

/// Backoff after a poll failure, so a network outage does not spin.
const POLL_BACKOFF: Duration = Duration::from_secs(5);

/// Command menu published to Telegram.
const COMMANDS: &[(&str, &str)] = &[
    ("ws", "switch working directory"),
    ("conv", "switch conversation"),
    ("new", "start a new conversation here"),
    ("history", "show recent messages"),
    ("status", "show the directory, CLI, model and mode"),
    ("agents", "list detected CLIs"),
    ("agent", "switch CLI, model and effort"),
    ("model", "switch model and effort"),
    ("mode", "set execution mode"),
    ("backup", "set the standby CLI for quota failover"),
    ("cancel", "stop the running turn"),
    ("help", "show available commands"),
];

/// Shared bridge state.
struct Bridge {
    daemon: Arc<Daemon>,
    bot: Bot,
    /// Exact token this poller was created for. A replacement invalidates it.
    token: String,
    /// Lifecycle generation captured at spawn, used for prompt cancellation.
    lifecycle_generation: u64,
    config: Mutex<TelegramConfig>,
    /// The run currently streaming, so `/cancel` has something to stop.
    active_run: Mutex<Option<RunId>>,
    /// Stops the detached event follower when this bot is reset or replaced.
    follower_cancel: Mutex<Option<CancelToken>>,
    /// Wakes lifecycle shutdown once the detached follower has released the run.
    follower_stopped: tokio::sync::Notify,
    /// Serializes network writes so revocation can drain and close the old bot.
    outbound: Mutex<()>,
    /// The keyboard currently awaiting a press, if any.
    ///
    /// Only one is live at a time. Buttons address their choice by index, so a
    /// press has to be matched against the menu that produced it; keeping the
    /// menu here is what makes a stale keyboard detectable rather than
    /// silently applying whatever now sits at that index.
    menu: Mutex<Option<Menu>>,
    /// Monotonic menu id, embedded in every callback payload.
    generation: std::sync::atomic::AtomicU64,
}

/// Which step of a selection wizard a menu is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Choosing the CLI, for the primary selection or the standby.
    Agent { backup: bool },
    /// Choosing a model for the CLI already picked.
    Model { backup: bool },
    /// Choosing a reasoning effort for the model already picked.
    Effort { backup: bool },
    /// Choosing the active workspace.
    Workspace,
    /// Choosing the active conversation.
    Conversation,
    /// Choosing the execution mode.
    Mode,
}

/// A keyboard awaiting a press.
struct Menu {
    /// Menu id, matched against the callback payload.
    generation: u64,
    /// Chat and message carrying the keyboard, so it can be edited in place.
    chat_id: i64,
    message_id: i64,
    /// What this step is choosing.
    step: Step,
    /// The value behind each button, in button order.
    values: Vec<String>,
    /// CLI chosen earlier in this wizard, carried into the model and effort steps.
    agent: Option<String>,
    /// Model chosen earlier in this wizard, carried into the effort step.
    model: Option<String>,
}

/// Payload for the button that skips the optional final step.
const SKIP: &str = "s";

/// Payload for the button that abandons a wizard.
const CANCEL: &str = "c";

/// Starts the bridge if it has been configured, returning immediately otherwise.
///
/// Safe to call at any time and idempotent: daemon startup calls it, and so does
/// the moment linking succeeds. Setting the bridge up must not require finding
/// and restarting a daemon that is already running — that reads as "connected
/// but nothing works".
pub fn spawn(daemon: Arc<Daemon>) {
    let paths = daemon.paths().clone();
    let config = match config::load(&paths) {
        Ok(Some(config)) if config.is_linked() => config,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(%error, "telegram config could not be read; bridge not started");
            return;
        }
    };
    let token = match config::load_token(&paths) {
        Ok(Some(token)) => token,
        Ok(None) => {
            tracing::warn!("telegram is configured but no bot token is stored; bridge not started");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "telegram token could not be read; bridge not started");
            return;
        }
    };

    // Claimed before spawning so two callers racing — bootstrap and a link
    // finishing at the same moment — cannot both start a poll loop and consume
    // each other's updates.
    if RUNNING
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        tracing::debug!("telegram bridge is already running");
        return;
    }

    let lifecycle_generation = BRIDGE_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    tokio::spawn(async move {
        let bridge = Arc::new(Bridge {
            bot: Bot::with_token(token.clone()),
            token,
            lifecycle_generation,
            daemon,
            config: Mutex::new(config),
            active_run: Mutex::new(None),
            follower_cancel: Mutex::new(None),
            follower_stopped: tokio::sync::Notify::new(),
            outbound: Mutex::new(()),
            menu: Mutex::new(None),
            generation: std::sync::atomic::AtomicU64::new(1),
        });
        if let Ok(mut active) = ACTIVE_BRIDGE.lock() {
            *active = Some(Arc::downgrade(&bridge));
        }
        bridge.prepare_bot().await;
        bridge.run().await;
        bridge.revoke().await;
        if let Ok(mut active) = ACTIVE_BRIDGE.lock() {
            let owns_slot = active
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .is_some_and(|current| Arc::ptr_eq(&current, &bridge));
            if owns_slot {
                *active = None;
            }
        }
        RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
        BRIDGE_STOPPED.notify_waiters();
    });
}

impl Bridge {
    /// True only while this exact bot binding is the daemon's current bridge.
    fn is_current(&self) -> bool {
        self.lifecycle_generation == BRIDGE_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Performs startup calls only while this exact token remains current.
    async fn prepare_bot(&self) {
        let _outbound = self.outbound.lock().await;
        if !self.is_current() {
            return;
        }
        if let Err(error) = self.bot.delete_webhook().await {
            tracing::warn!(%error, "could not clear telegram webhook before polling");
        }
        if let Err(error) = self.bot.set_my_commands(COMMANDS).await {
            // Cosmetic: the bridge works without a published menu.
            tracing::warn!(%error, "could not publish the telegram command menu");
        }
    }

    /// Revokes a detached Telegram turn and waits for its follower to release it.
    async fn revoke(&self) {
        // The generation changes before this method is called. Taking and
        // releasing the network lock drains a request that had already begun;
        // later callers acquire it, observe the stale generation, and stop.
        drop(self.outbound.lock().await);
        if let Some(cancel) = self.follower_cancel.lock().await.clone() {
            cancel.cancel();
        }
        if let Some(run_id) = self.active_run.lock().await.clone() {
            let _ = self.daemon.cancel_active_run(&run_id);
        }

        while self.active_run.lock().await.is_some() {
            let stopped = self.follower_stopped.notified();
            if self.active_run.lock().await.is_none() {
                break;
            }
            stopped.await;
        }
    }

    async fn run(self: &Arc<Self>) {
        let mut shutdown = self.daemon.subscribe_shutdown();
        tracing::info!("telegram bridge started");
        loop {
            if self.lifecycle_generation
                != BRIDGE_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
            {
                break;
            }

            // A reset or token replacement must stop this exact bot instance.
            let configured = matches!(
                (config::load(self.daemon.paths()), config::load_token(self.daemon.paths())),
                (Ok(Some(saved)), Ok(Some(token))) if saved.is_linked() && token == self.token
            );
            if !configured {
                tracing::info!("telegram was disconnected or rebound; stopping the bridge");
                break;
            }

            // Every getUpdates caller shares this ownership lock. Link holds it
            // for its whole authorization window; allow holds it through its
            // offset mutation, so no flow can consume another flow's updates.
            let ownership = tokio::select! {
                _ = shutdown.recv() => break,
                _ = BRIDGE_CHANGED.notified() => {
                    if self.lifecycle_generation
                        != BRIDGE_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        break;
                    }
                    continue;
                }
                guard = POLLING.lock() => guard,
            };
            if self.lifecycle_generation
                != BRIDGE_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
            {
                break;
            }

            let offset = self.config.lock().await.update_offset;
            let poll = self.bot.get_updates(offset, POLL_SECS);
            let batch = tokio::select! {
                _ = shutdown.recv() => break,
                _ = BRIDGE_CHANGED.notified() => {
                    if self.lifecycle_generation
                        != BRIDGE_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        break;
                    }
                    continue;
                }
                result = poll => match result {
                    Ok(batch) => batch,
                    Err(error) => {
                        drop(ownership);
                        tracing::warn!(%error, "telegram poll failed");
                        tokio::time::sleep(POLL_BACKOFF).await;
                        continue;
                    }
                },
            };

            // Acknowledge the raw envelope, not just updates this build knows
            // how to parse. Photos, edits, and future update kinds must not wedge
            // the offset and be redelivered forever.
            if let Some(high_water) = batch.high_water {
                self.advance_offset(high_water).await;
            }
            drop(ownership);

            for update in batch.updates {
                if !self.is_current() {
                    break;
                }
                if !update.is_private_chat() {
                    tracing::warn!(
                        user = update.from_id(),
                        "ignoring telegram update outside the sender's private chat"
                    );
                    continue;
                }
                if !self.config.lock().await.allows(update.from_id()) {
                    tracing::warn!(
                        user = update.from_id(),
                        "ignoring telegram update from an unauthorized user"
                    );
                    continue;
                }
                let (chat_id, outcome) = match &update {
                    Update::Message(message) => (message.chat_id, self.handle(message).await),
                    Update::Callback(query) => {
                        // Acknowledged first: Telegram spins on the button until
                        // this lands, so a slow action would look like a hang.
                        self.answer_callback(&query.id).await;
                        (query.chat_id, self.on_button(query).await)
                    }
                };
                if let Err(error) = outcome {
                    tracing::warn!(%error, "telegram update handling failed");
                    let _ = self
                        .say(chat_id, &render::notice(&format!("failed: {error}")))
                        .await;
                }
            }
        }
        tracing::info!("telegram bridge stopped");
    }

    /// Records that `update_id` has been consumed.
    async fn advance_offset(&self, update_id: i64) {
        let mut config = self.config.lock().await;
        if let Err(error) = Self::advance_offset_config(self.daemon.paths(), &mut config, update_id)
        {
            tracing::warn!(%error, "could not persist the telegram poll offset");
        }
    }

    /// Merges the latest persisted settings before recording a polling offset.
    ///
    /// The TUI can allow a workspace while the bridge is already polling. Reloading
    /// here makes that external change visible to the next command and prevents the
    /// bridge's old in-memory snapshot from overwriting it when the next update is
    /// consumed.
    fn advance_offset_config(
        paths: &argo_core::paths::ArgoPaths,
        cached: &mut TelegramConfig,
        update_id: i64,
    ) -> Result<()> {
        let updated = config::mutate(paths, |saved| {
            saved.update_offset = saved.update_offset.max(update_id.saturating_add(1));
            saved.clone()
        })?;
        *cached = updated;
        Ok(())
    }

    /// Applies one serialized disk mutation and refreshes the bridge cache.
    async fn mutate_config<T>(&self, change: impl FnOnce(&mut TelegramConfig) -> T) -> Result<T> {
        let mut cached = self.config.lock().await;
        let (updated, output) = config::mutate(self.daemon.paths(), |saved| {
            let output = change(saved);
            (saved.clone(), output)
        })?;
        *cached = updated;
        Ok(output)
    }

    /// Sends one message, splitting it if it exceeds Telegram's ceiling.
    async fn say(&self, chat_id: i64, text: &str) -> Result<Option<i64>> {
        if !self.is_current() {
            return Err(argo_core::error::ArgoError::Cancelled);
        }
        let mut last = None;
        for chunk in split_message_safe(text, MAX_MESSAGE_CHARS) {
            last = Some(self.send_chunk(chat_id, &chunk).await?);
        }
        Ok(last)
    }

    /// Sends a pre-sized chunk using its validated parse mode.
    async fn send_chunk(&self, chat_id: i64, chunk: &MessageChunk) -> Result<i64> {
        let _outbound = self.outbound.lock().await;
        if !self.is_current() {
            return Err(argo_core::error::ArgoError::Cancelled);
        }
        let mode = if chunk.markdown {
            ParseMode::MarkdownV2
        } else {
            ParseMode::Plain
        };
        match self
            .bot
            .send_message(chat_id, &chunk.text, mode, false)
            .await
        {
            Ok(id) => Ok(id),
            Err(error) if chunk.markdown && is_parse_entity_error(&error) => {
                tracing::warn!(%error, "markdown_v2 entity parse failed; retrying as plain text");
                self.bot
                    .send_message(chat_id, &plain_text(&chunk.text), ParseMode::Plain, false)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    /// Edits a bubble in place, degrading to plain text only on a formatting refusal.
    async fn revise(&self, chat_id: i64, message_id: i64, text: &str) {
        let clipped = clip(text, MAX_MESSAGE_CHARS);
        self.revise_exact(chat_id, message_id, &clipped).await;
    }

    /// Edits one already-sized chunk without clipping it again.
    async fn revise_exact(&self, chat_id: i64, message_id: i64, text: &str) {
        let chunk = safe_message_chunk(text);
        self.revise_chunk(chat_id, message_id, &chunk).await;
    }

    async fn revise_chunk(&self, chat_id: i64, message_id: i64, chunk: &MessageChunk) {
        let _outbound = self.outbound.lock().await;
        if !self.is_current() {
            return;
        }
        let mode = if chunk.markdown {
            ParseMode::MarkdownV2
        } else {
            ParseMode::Plain
        };
        match self
            .bot
            .edit_message_text(chat_id, message_id, &chunk.text, mode)
            .await
        {
            Ok(()) => {}
            Err(error) if chunk.markdown && is_parse_entity_error(&error) => {
                let _ = self
                    .bot
                    .edit_message_text(
                        chat_id,
                        message_id,
                        &plain_text(&chunk.text),
                        ParseMode::Plain,
                    )
                    .await;
            }
            Err(error) => tracing::warn!(%error, "telegram message edit failed"),
        }
    }

    /// Publishes a complete final answer, reusing the streaming bubble for its
    /// first chunk and sending every remaining chunk as a continuation.
    async fn finish_reply(&self, chat_id: i64, bubble: Option<i64>, text: &str) -> Result<()> {
        let chunks = split_message_safe(text, MAX_MESSAGE_CHARS);
        let mut chunks = chunks.into_iter();
        if let Some(first) = chunks.next() {
            match bubble {
                Some(id) => self.revise_chunk(chat_id, id, &first).await,
                None => {
                    self.send_chunk(chat_id, &first).await?;
                }
            }
        }
        for chunk in chunks {
            self.send_chunk(chat_id, &chunk).await?;
        }
        Ok(())
    }

    async fn react(&self, message: &IncomingMessage, emoji: Option<&str>) {
        if !self.config.lock().await.reactions {
            return;
        }
        let _outbound = self.outbound.lock().await;
        if !self.is_current() {
            return;
        }
        // Purely decorative; a failure must never interrupt a turn.
        let _ = self
            .bot
            .set_message_reaction(message.chat_id, message.message_id, emoji)
            .await;
    }

    /// Acknowledges a button only while this bridge remains current.
    async fn answer_callback(&self, callback_id: &str) {
        let _outbound = self.outbound.lock().await;
        if self.is_current() {
            let _ = self.bot.answer_callback(callback_id, None).await;
        }
    }

    /// Routes one authorized message.
    async fn handle(self: &Arc<Self>, message: &IncomingMessage) -> Result<()> {
        if !message.is_private_chat() || message.text.is_empty() {
            return Ok(());
        }
        if let Some(rest) = message.text.strip_prefix('/') {
            // Group chats deliver `/cmd@botname`; the suffix is not part of the
            // command.
            let (name, argument) = match rest.split_once(char::is_whitespace) {
                Some((name, argument)) => (name, argument.trim()),
                None => (rest, ""),
            };
            let name = name.split('@').next().unwrap_or(name).to_ascii_lowercase();
            return self.command(message, &name, argument).await;
        }
        self.turn(message).await
    }
}

impl Bridge {
    /// Returns the conversation to send to, creating one when needed.
    async fn active_conversation(&self, chat_id: i64) -> Result<Option<ConversationId>> {
        let (existing, workspace) = {
            let config = self.config.lock().await;
            (
                config.active_conversation.clone(),
                config.active_workspace.clone(),
            )
        };
        if let Some(id) = existing {
            return Ok(Some(ConversationId::new(id)));
        }
        let Some(root) = workspace else {
            self.say(
                chat_id,
                &render::notice(
                    "no workspace is allowed yet — run /telegram allow in the Argo TUI from the directory you want to work in",
                ),
            )
            .await?;
            return Ok(None);
        };
        self.open_conversation(&root, None).await.map(Some)
    }

    /// Creates a conversation in `root` and makes it active.
    async fn open_conversation(&self, root: &str, title: Option<String>) -> Result<ConversationId> {
        let response = dispatch(
            &self.daemon,
            Request::NewConversation {
                root: root.to_string(),
                title,
            },
        )
        .await;
        let id = match response {
            Response::Conversation { summary, .. } => summary.id,
            other => return Err(unexpected(other)),
        };
        self.mutate_config(|config| {
            config.active_conversation = Some(id.to_string());
            config.active_workspace = Some(root.to_string());
        })
        .await?;
        Ok(id)
    }

    /// Loads a conversation summary and its messages.
    async fn load(&self, id: &ConversationId) -> Result<(ConversationSummary, Vec<MessageView>)> {
        match dispatch(
            &self.daemon,
            Request::GetConversation {
                conversation_id: id.clone(),
            },
        )
        .await
        {
            Response::Conversation { summary, messages } => Ok((summary, messages)),
            other => Err(unexpected(other)),
        }
    }

    /// Posts the recap card for the currently active conversation.
    async fn post_recap(&self, chat_id: i64, id: &ConversationId) -> Result<()> {
        let (summary, messages) = self.load(id).await?;
        // Only the tail is quoted; `GetConversation` returns the whole history
        // and forwarding it wholesale would flood the chat.
        let card = render::recap_card(&recap_from(&summary, &messages));
        self.say(chat_id, &card).await?;
        Ok(())
    }

    /// Submits a turn and mirrors it into the chat as it streams.
    async fn turn(self: &Arc<Self>, message: &IncomingMessage) -> Result<()> {
        if self.active_run.lock().await.is_some() {
            self.say(
                message.chat_id,
                &render::notice("a turn is already running — /cancel to stop it"),
            )
            .await?;
            return Ok(());
        }
        let Some(conversation_id) = self.active_conversation(message.chat_id).await? else {
            return Ok(());
        };

        self.react(message, Some("👀")).await;

        // Subscribing before submitting closes the gap where the first deltas
        // could land before the receiver exists.
        let mut events = self.daemon.subscribe_events();
        let run_id = match dispatch(
            &self.daemon,
            Request::SendMessage {
                conversation_id: conversation_id.clone(),
                prompt: message.text.clone(),
            },
        )
        .await
        {
            Response::RunStarted { run_id, .. } => run_id,
            Response::Error {
                message: detail, ..
            } => {
                self.react(message, Some("❌")).await;
                self.say(message.chat_id, &render::notice(&detail)).await?;
                return Ok(());
            }
            other => return Err(unexpected(other)),
        };
        *self.active_run.lock().await = Some(run_id.clone());
        let follower_cancel = CancelToken::new();
        *self.follower_cancel.lock().await = Some(follower_cancel.clone());

        // Following can last minutes. Detach it so the poll loop remains free to
        // receive `/cancel`, `/status`, and other commands while the turn runs.
        let bridge = Arc::clone(self);
        let incoming = message.clone();
        tokio::spawn(async move {
            let outcome = bridge
                .follow(
                    &incoming,
                    &conversation_id,
                    &run_id,
                    &mut events,
                    &follower_cancel,
                )
                .await;
            let mut active = bridge.active_run.lock().await;
            if active.as_ref() == Some(&run_id) {
                *active = None;
                *bridge.follower_cancel.lock().await = None;
            }
            drop(active);
            bridge.follower_stopped.notify_waiters();
            if let Err(error) = outcome {
                tracing::warn!(%error, "telegram run following failed");
                let _ = bridge
                    .say(
                        incoming.chat_id,
                        &render::notice(&format!("failed: {error}")),
                    )
                    .await;
            }
        });
        Ok(())
    }

    /// Streams one run into a live-edited bubble.
    async fn follow(
        &self,
        message: &IncomingMessage,
        conversation_id: &ConversationId,
        run_id: &RunId,
        events: &mut tokio::sync::broadcast::Receiver<argo_core::event::RunEvent>,
        cancel: &CancelToken,
    ) -> Result<()> {
        let stream_edits = self.config.lock().await.stream_edits;
        let mut view = TurnView::default();
        let mut bubble: Option<i64> = None;
        let mut tool_bubble: Option<i64> = None;
        let mut last_edit = Instant::now() - EDIT_INTERVAL;
        let mut status = RunStatus::Running;

        loop {
            let event = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                event = events.recv() => match event {
                Ok(event) => event,
                // Lagging only loses intermediate deltas; the store holds the
                // canonical transcript, and the final render below is rebuilt
                // from it, so the reader still sees a complete answer.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "telegram bridge lagged behind the event stream");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            };
            if &event.run_id != run_id {
                continue;
            }
            view.apply(&event.kind);

            if let RunEventKind::RunFinished {
                status: final_status,
                ..
            } = &event.kind
            {
                status = *final_status;
                break;
            }

            if !stream_edits {
                continue;
            }
            if last_edit.elapsed() < EDIT_INTERVAL {
                continue;
            }
            last_edit = Instant::now();

            let text = view.bubble(false);
            match bubble {
                Some(id) => self.revise(message.chat_id, id, &text).await,
                None => bubble = self.say(message.chat_id, &text).await?,
            }
            let tools = render::tool_bubble(&view.tools);
            if !tools.is_empty() {
                match tool_bubble {
                    Some(id) => self.revise(message.chat_id, id, &tools).await,
                    None => tool_bubble = self.say(message.chat_id, &tools).await?,
                }
            }
        }

        // RunFinished is a commit barrier: rebuild from SQLite rather than
        // trusting a broadcast stream that may have lagged or dropped deltas.
        let canonical = self
            .load(conversation_id)
            .await?
            .1
            .into_iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| message.text)
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| view.text.clone());
        let text = view.bubble_with_text(&canonical, true);
        self.finish_reply(message.chat_id, bubble, &text).await?;
        if let (Some(id), false) = (tool_bubble, view.tools.is_empty()) {
            self.revise(message.chat_id, id, &render::tool_bubble(&view.tools))
                .await;
        }

        let reaction = match status {
            RunStatus::Succeeded => "✅",
            RunStatus::Cancelled => "🤝",
            _ => "❌",
        };
        self.react(message, Some(reaction)).await;
        Ok(())
    }
}

impl Bridge {
    /// Handles one slash command.
    async fn command(
        self: &Arc<Self>,
        message: &IncomingMessage,
        name: &str,
        argument: &str,
    ) -> Result<()> {
        let chat = message.chat_id;
        match name {
            "help" | "start" => {
                let (active, count) = {
                    let config = self.config.lock().await;
                    (config.active_workspace.clone(), config.workspaces.len())
                };
                let mut lines = vec!["*Argo*".to_string(), String::new()];
                // The working directory leads: a chat gives no other clue which
                // one it is pointed at, and guessing wrong is expensive.
                lines.push(format!(
                    "📂 {}",
                    argo_telegram::markdown_v2::escape(&match &active {
                        Some(root) =>
                            format!("{} — /ws to switch ({count} allowed)", workspace_name(root)),
                        None => "no directory yet".into(),
                    })
                ));
                lines.push(String::new());
                for (command, detail) in COMMANDS {
                    lines.push(format!(
                        "/{} — {}",
                        argo_telegram::markdown_v2::escape(command),
                        argo_telegram::markdown_v2::escape(detail)
                    ));
                }
                lines.push(String::new());
                lines.push(argo_telegram::markdown_v2::escape(
                    "Anything else you send is a message to the agent.",
                ));
                lines.push(argo_telegram::markdown_v2::escape(
                    "/new opens a conversation in the current directory; it does not change directory — /ws does.",
                ));
                lines.push(argo_telegram::markdown_v2::escape(
                    "To add a directory, run: argo telegram allow <your-id> in it.",
                ));
                self.say(chat, &lines.join("\n")).await?;
            }

            "ws" => self.workspace_command(chat, argument).await?,
            "conv" => self.conversation_command(chat, argument).await?,

            "new" => {
                let root = self.config.lock().await.active_workspace.clone();
                match root {
                    Some(root) => {
                        let title = (!argument.is_empty()).then(|| argument.to_string());
                        let id = self.open_conversation(&root, title).await?;
                        self.post_recap(chat, &id).await?;
                    }
                    None => {
                        self.say(chat, &render::notice("no workspace is allowed yet"))
                            .await?;
                    }
                }
            }

            "history" => {
                let count = argument.parse::<usize>().unwrap_or(5).clamp(1, 20);
                self.history_command(chat, count).await?;
            }

            "status" => {
                let id = self.config.lock().await.active_conversation.clone();
                match id {
                    Some(id) => self.post_recap(chat, &ConversationId::new(id)).await?,
                    None => {
                        self.say(chat, &render::notice("no conversation is open yet"))
                            .await?;
                    }
                }
            }

            "agents" => {
                let response = dispatch(&self.daemon, Request::ListAgents { refresh: false }).await;
                let Response::Agents { agents } = response else {
                    return Err(unexpected(response));
                };
                let lines: Vec<String> = agents
                    .iter()
                    .map(|agent| {
                        argo_telegram::markdown_v2::escape(&format!(
                            "{} {}",
                            if agent.available { "✓" } else { "·" },
                            agent.id
                        ))
                    })
                    .collect();
                self.say(chat, &lines.join("\n")).await?;
            }

            "agent" | "model" | "mode" | "backup" => {
                self.selection_command(chat, name, argument).await?
            }

            "cancel" => {
                let run = self.active_run.lock().await.clone();
                match run {
                    Some(run_id) => {
                        dispatch(&self.daemon, Request::Cancel { run_id }).await;
                        self.say(chat, &render::notice("cancelling")).await?;
                    }
                    None => {
                        self.say(chat, &render::notice("nothing is running"))
                            .await?;
                    }
                }
            }

            other => {
                self.say(
                    chat,
                    &render::notice(&format!("unknown command /{other} — try /help")),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Lists or switches the active workspace.
    ///
    /// Switching is by index into the allowlist, never by path: a chat message
    /// must not be able to name a directory that was never opted in.
    async fn workspace_command(self: &Arc<Self>, chat: i64, argument: &str) -> Result<()> {
        let (workspaces, active) = {
            let config = self.config.lock().await;
            (config.workspaces.clone(), config.active_workspace.clone())
        };
        if workspaces.is_empty() {
            self.say(
                chat,
                &render::notice(
                    "no workspaces allowed yet — run /telegram allow in the Argo TUI from the directory you want",
                ),
            )
            .await?;
            return Ok(());
        }

        if argument.is_empty() {
            let choices = workspaces
                .iter()
                .map(|root| {
                    let marker = if Some(root) == active.as_ref() {
                        "▸ "
                    } else {
                        ""
                    };
                    (format!("{marker}{}", workspace_name(root)), root.clone())
                })
                .collect();
            return self
                .open_menu(
                    chat,
                    Draft {
                        prompt: render::notice(&format!(
                            "workspace — currently {}",
                            active
                                .as_deref()
                                .map(workspace_name)
                                .unwrap_or_else(|| "none".into())
                        )),
                        step: Step::Workspace,
                        choices,
                        agent: None,
                        model: None,
                        skippable: false,
                    },
                )
                .await;
        }

        let Some(root) = argument
            .parse::<usize>()
            .ok()
            .filter(|index| *index >= 1 && *index <= workspaces.len())
            .map(|index| workspaces[index - 1].clone())
        else {
            self.say(chat, &render::notice("pick a number from /ws"))
                .await?;
            return Ok(());
        };
        self.switch_workspace(chat, &root).await
    }

    /// Makes `root` the active workspace and opens a conversation there.
    ///
    /// The conversation follows the workspace: a chat pointed at one directory
    /// while claiming another is the failure this whole indirection exists to
    /// avoid, so the newest conversation in the new root is adopted, or a fresh
    /// one is created when it has none.
    async fn switch_workspace(self: &Arc<Self>, chat: i64, root: &str) -> Result<()> {
        let newest = match dispatch(
            &self.daemon,
            Request::ListConversations {
                root: root.to_string(),
            },
        )
        .await
        {
            Response::Conversations { conversations } => {
                conversations.first().map(|c| c.id.clone())
            }
            other => return Err(unexpected(other)),
        };
        self.mutate_config(|config| {
            config.active_workspace = Some(root.to_string());
            config.active_conversation = newest.as_ref().map(|id| id.to_string());
        })
        .await?;
        match newest {
            Some(id) => self.post_recap(chat, &id).await?,
            None => {
                let id = self.open_conversation(root, None).await?;
                self.post_recap(chat, &id).await?;
            }
        }
        Ok(())
    }

    /// Lists or switches the active conversation within the current workspace.
    async fn conversation_command(&self, chat: i64, argument: &str) -> Result<()> {
        let Some(root) = self.config.lock().await.active_workspace.clone() else {
            self.say(chat, &render::notice("no workspace is allowed yet"))
                .await?;
            return Ok(());
        };
        // Summaries only: the full history is never fetched to build a list.
        let conversations = match dispatch(&self.daemon, Request::ListConversations { root }).await
        {
            Response::Conversations { conversations } => conversations,
            other => return Err(unexpected(other)),
        };
        if conversations.is_empty() {
            self.say(
                chat,
                &render::notice("no conversations yet — send a message to start one"),
            )
            .await?;
            return Ok(());
        }

        if argument.is_empty() {
            let active = self.config.lock().await.active_conversation.clone();
            let choices = conversations
                .iter()
                .take(12)
                .map(|summary| {
                    let marker = if active.as_deref() == Some(summary.id.as_str()) {
                        "▸ "
                    } else {
                        ""
                    };
                    (
                        format!(
                            "{marker}{} ({})",
                            clip(summary.title.as_deref().unwrap_or("untitled"), 34),
                            summary.message_count
                        ),
                        summary.id.to_string(),
                    )
                })
                .collect();
            return self
                .open_menu(
                    chat,
                    Draft {
                        prompt: render::notice("conversation"),
                        step: Step::Conversation,
                        choices,
                        agent: None,
                        model: None,
                        skippable: false,
                    },
                )
                .await;
        }

        let Some(summary) = argument
            .parse::<usize>()
            .ok()
            .filter(|index| *index >= 1 && *index <= conversations.len())
            .map(|index| conversations[index - 1].clone())
        else {
            self.say(chat, &render::notice("pick a number from /conv"))
                .await?;
            return Ok(());
        };
        self.switch_conversation(chat, &summary.id).await
    }

    /// Makes `id` the active conversation and posts its recap.
    async fn switch_conversation(&self, chat: i64, id: &ConversationId) -> Result<()> {
        self.mutate_config(|config| {
            config.active_conversation = Some(id.to_string());
        })
        .await?;
        self.post_recap(chat, id).await
    }

    /// Quotes the last `count` exchanges of the active conversation.
    async fn history_command(&self, chat: i64, count: usize) -> Result<()> {
        let Some(id) = self.config.lock().await.active_conversation.clone() else {
            self.say(chat, &render::notice("no conversation is open yet"))
                .await?;
            return Ok(());
        };
        let (_, messages) = self.load(&ConversationId::new(id)).await?;
        // Bounded slice of the tail: the daemon returns everything, and a long
        // conversation would otherwise be dumped into the chat.
        let tail: Vec<&MessageView> = messages
            .iter()
            .filter(|message| !message.text.trim().is_empty())
            .rev()
            .take(count * 2)
            .collect();
        if tail.is_empty() {
            self.say(chat, &render::notice("nothing to show yet"))
                .await?;
            return Ok(());
        }
        let mut lines = Vec::new();
        for message in tail.into_iter().rev() {
            let marker = if message.role == "user" {
                "🧑"
            } else {
                "🤖"
            };
            lines.push(format!(
                "{marker} {}",
                argo_telegram::markdown_v2::escape(&clip(message.text.trim(), 600))
            ));
        }
        self.say(chat, &lines.join("\n\n")).await?;
        Ok(())
    }

    /// Applies `/agent`, `/model`, `/mode`, or `/backup`.
    ///
    /// With no argument each opens a keyboard, and `/agent` and `/backup` then
    /// chain CLI → model → effort exactly as the TUI does. Typing the value
    /// still works, so the flow does not become the only way in.
    async fn selection_command(
        self: &Arc<Self>,
        chat: i64,
        name: &str,
        argument: &str,
    ) -> Result<()> {
        if self.config.lock().await.active_conversation.is_none() {
            self.say(chat, &render::notice("no conversation is open yet"))
                .await?;
            return Ok(());
        }

        if argument.is_empty() {
            return self.open_selection_menu(chat, name).await;
        }

        let selection = match name {
            "agent" => Selection::Primary(argument.to_string(), None, None),
            "model" => {
                return self
                    .apply_request(
                        chat,
                        Request::Select {
                            conversation_id: self.conversation().await?,
                            change: argo_core::session::SelectionChange {
                                model: Some(argument.to_string()),
                                ..Default::default()
                            },
                        },
                    )
                    .await
            }
            "mode" => Selection::Mode(argument.to_string()),
            _ => {
                // `/backup <agent> [model] [effort]` — the standby needs its own
                // model, because model ids do not transfer between CLIs.
                let mut parts = argument.split_whitespace();
                let agent = parts.next().unwrap_or_default();
                if matches!(agent, "none" | "off" | "clear") {
                    Selection::Backup(None, None, None)
                } else {
                    Selection::Backup(
                        Some(agent.to_string()),
                        parts.next().map(str::to_string),
                        parts.next().map(str::to_string),
                    )
                }
            }
        };
        self.apply_selection(chat, selection).await
    }

    /// Opens the first step of a keyboard-driven selection.
    async fn open_selection_menu(self: &Arc<Self>, chat: i64, name: &str) -> Result<()> {
        match name {
            "agent" | "backup" => {
                let backup = name == "backup";
                let response = dispatch(&self.daemon, Request::ListAgents { refresh: false }).await;
                let Response::Agents { agents } = response else {
                    return Err(unexpected(response));
                };
                // Only installed CLIs are offered: a button that cannot possibly
                // run is worse than no button.
                let mut choices: Vec<(String, String)> = agents
                    .iter()
                    .filter(|agent| agent.available)
                    .map(|agent| (agent.name.clone(), agent.id.clone()))
                    .collect();
                if choices.is_empty() {
                    self.say(chat, &render::notice("no CLI is installed and detected"))
                        .await?;
                    return Ok(());
                }
                if backup {
                    choices.push(("✕ no backup".into(), BACKUP_NONE.into()));
                }
                self.open_menu(
                    chat,
                    Draft {
                        prompt: render::notice(&format!("{}: choose a CLI", scope(backup))),
                        step: Step::Agent { backup },
                        choices,
                        agent: None,
                        model: None,
                        skippable: false,
                    },
                )
                .await
            }

            // `/model` alone keeps the current CLI and re-picks within it, so it
            // enters the chain one step in rather than restarting it.
            "model" => {
                let (summary, _) = self.load(&self.conversation().await?).await?;
                let Some(agent) = summary.selected_agent_id else {
                    self.say(chat, &render::notice("choose a CLI first — /agent"))
                        .await?;
                    return Ok(());
                };
                let menu = Menu {
                    generation: 0,
                    chat_id: chat,
                    message_id: self
                        .say(chat, &render::notice("…"))
                        .await?
                        .unwrap_or_default(),
                    step: Step::Agent { backup: false },
                    values: Vec::new(),
                    agent: None,
                    model: None,
                };
                self.chose_agent(&menu, Some(agent.to_string()), false)
                    .await
            }

            _ => {
                let (summary, _) = self.load(&self.conversation().await?).await?;
                let support = summary
                    .selected_agent_id
                    .as_deref()
                    .and_then(argo_runtime::find)
                    .map(|def| def.capabilities.modes)
                    .unwrap_or(argo_core::mode::ModeSupport::NONE)
                    .with_argo_plan();
                let choices: Vec<(String, String)> = support
                    .available()
                    .iter()
                    .map(|mode| (mode.label().to_string(), mode.id().to_string()))
                    .collect();
                if choices.is_empty() {
                    self.say(
                        chat,
                        &render::notice("the selected CLI always runs with full access"),
                    )
                    .await?;
                    return Ok(());
                }
                self.open_menu(
                    chat,
                    Draft {
                        prompt: render::notice("execution mode"),
                        step: Step::Mode,
                        choices,
                        agent: None,
                        model: None,
                        skippable: false,
                    },
                )
                .await
            }
        }
    }

    /// The active conversation, or an error when none is open.
    async fn conversation(&self) -> Result<ConversationId> {
        self.config
            .lock()
            .await
            .active_conversation
            .clone()
            .map(ConversationId::new)
            .ok_or_else(|| {
                argo_core::error::ArgoError::Invalid("no conversation is open yet".into())
            })
    }

    /// Turns a completed selection into the request that records it.
    async fn apply_selection(self: &Arc<Self>, chat: i64, selection: Selection) -> Result<()> {
        let conversation_id = self.conversation().await?;
        let request = match selection {
            Selection::Primary(agent, model, reasoning) => Request::Select {
                conversation_id,
                change: argo_core::session::SelectionChange {
                    agent_id: Some(argo_core::ids::AgentId::new(agent)),
                    model,
                    reasoning,
                },
            },
            Selection::Backup(agent_id, model, reasoning) => Request::SetBackupAgent {
                conversation_id,
                agent_id,
                model,
                reasoning,
            },
            Selection::Mode(mode) => Request::SetMode {
                conversation_id,
                mode: Some(mode),
            },
        };
        self.apply_request(chat, request).await
    }

    /// Dispatches a selection request and reports the resulting state.
    async fn apply_request(&self, chat: i64, request: Request) -> Result<()> {
        match dispatch(&self.daemon, request).await {
            Response::Conversation { summary, .. } => {
                let card = render::recap_card(&recap_from(&summary, &[]));
                self.say(chat, &card).await?;
            }
            Response::Error { message, .. } => {
                self.say(chat, &render::notice(&message)).await?;
            }
            other => return Err(unexpected(other)),
        }
        Ok(())
    }
}

/// Names which selection a wizard step belongs to, for its prompt.
fn scope(backup: bool) -> &'static str {
    if backup {
        "backup"
    } else {
        "agent"
    }
}

/// Longest button label Telegram lays out comfortably on one row of two.
const NARROW_LABEL: usize = 14;

/// A menu about to be posted or advanced to.
struct Draft {
    /// Text above the keyboard.
    prompt: String,
    /// What this step chooses.
    step: Step,
    /// Button label paired with the value it selects.
    choices: Vec<(String, String)>,
    /// CLI already chosen in this wizard.
    agent: Option<String>,
    /// Model already chosen in this wizard.
    model: Option<String>,
    /// True when the step is optional and should offer a skip button.
    skippable: bool,
}

impl Draft {
    /// Arranges the choices into keyboard rows.
    ///
    /// Long labels get a row each: Telegram shrinks text to fit rather than
    /// wrapping, so two model ids side by side become unreadable on a phone.
    fn rows(&self, generation: u64) -> Vec<KeyboardRow> {
        let widest = self
            .choices
            .iter()
            .map(|(label, _)| label.chars().count())
            .max()
            .unwrap_or(0);
        let per_row = if widest > NARROW_LABEL { 1 } else { 2 };
        // Indices address `values` positionally, so they are assigned across the
        // whole list rather than restarting inside each row.
        let mut rows: Vec<KeyboardRow> = self
            .choices
            .chunks(per_row)
            .enumerate()
            .map(|(row, chunk)| {
                chunk
                    .iter()
                    .enumerate()
                    .map(|(column, (label, _))| {
                        let index = row * per_row + column;
                        (label.clone(), format!("{generation}:{index}"))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut trailer: KeyboardRow = Vec::new();
        if self.skippable {
            trailer.push(("skip".into(), format!("{generation}:{SKIP}")));
        }
        trailer.push(("✕ cancel".into(), format!("{generation}:{CANCEL}")));
        rows.push(trailer);
        rows
    }
}

impl Bridge {
    /// Posts a keyboard and records it as the live menu.
    ///
    /// Replacing any previous menu is deliberate: only the newest keyboard stays
    /// answerable, so an old one left scrolled up in the chat cannot be tapped
    /// to apply a choice against a list that has since changed.
    async fn open_menu(&self, chat_id: i64, draft: Draft) -> Result<()> {
        let _outbound = self.outbound.lock().await;
        if !self.is_current() {
            return Err(argo_core::error::ArgoError::Cancelled);
        }
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let rows = draft.rows(generation);
        let message_id = match self
            .bot
            .send(chat_id, &draft.prompt, ParseMode::MarkdownV2, false, &rows)
            .await
        {
            Ok(id) => id,
            Err(error) if is_parse_entity_error(&error) => {
                tracing::warn!(%error, "markdown_v2 keyboard entity parse failed; retrying as plain text");
                self.bot
                    .send(
                        chat_id,
                        &plain_text(&draft.prompt),
                        ParseMode::Plain,
                        false,
                        &rows,
                    )
                    .await?
            }
            Err(error) => return Err(error),
        };
        *self.menu.lock().await = Some(Menu {
            generation,
            chat_id,
            message_id,
            step: draft.step,
            values: draft.choices.into_iter().map(|(_, value)| value).collect(),
            agent: draft.agent,
            model: draft.model,
        });
        Ok(())
    }

    /// Advances a wizard by rewriting the message the keyboard is attached to.
    ///
    /// Editing rather than posting keeps a three-step wizard to one bubble, so
    /// the chat is not left with two dead keyboards above the live one.
    async fn advance_menu(&self, chat_id: i64, message_id: i64, draft: Draft) -> Result<()> {
        let _outbound = self.outbound.lock().await;
        if !self.is_current() {
            return Err(argo_core::error::ArgoError::Cancelled);
        }
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let rows = draft.rows(generation);
        if let Err(error) = self
            .bot
            .edit_message(
                chat_id,
                message_id,
                &draft.prompt,
                ParseMode::MarkdownV2,
                &rows,
            )
            .await
        {
            if !is_parse_entity_error(&error) {
                return Err(error);
            }
            tracing::warn!(%error, "markdown_v2 keyboard entity parse failed; retrying as plain text");
            self.bot
                .edit_message(
                    chat_id,
                    message_id,
                    &plain_text(&draft.prompt),
                    ParseMode::Plain,
                    &rows,
                )
                .await?;
        }
        *self.menu.lock().await = Some(Menu {
            generation,
            chat_id,
            message_id,
            step: draft.step,
            values: draft.choices.into_iter().map(|(_, value)| value).collect(),
            agent: draft.agent,
            model: draft.model,
        });
        Ok(())
    }

    /// Rewrites a menu message as settled text, removing its keyboard.
    ///
    /// Removing it matters more than the text: leaving live buttons on a finished
    /// step lets one stray tap replay a choice that already applied.
    async fn close_menu(&self, chat_id: i64, message_id: i64, text: &str) {
        let _outbound = self.outbound.lock().await;
        if !self.is_current() {
            return;
        }
        if let Err(error) = self
            .bot
            .edit_message(chat_id, message_id, text, ParseMode::MarkdownV2, &[])
            .await
        {
            if is_parse_entity_error(&error) {
                let _ = self
                    .bot
                    .edit_message(
                        chat_id,
                        message_id,
                        &plain_text(text),
                        ParseMode::Plain,
                        &[],
                    )
                    .await;
            } else {
                tracing::warn!(%error, "telegram menu cleanup failed");
            }
        }
    }
}

/// What a callback payload asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Press {
    /// Take the choice at this index.
    Choose(usize),
    /// Leave the optional step unset.
    Skip,
    /// Abandon the wizard.
    Cancel,
}

/// Decodes a callback payload into the menu it belongs to and what it asks for.
///
/// Returns `None` for anything malformed. Payloads arrive from Telegram rather
/// than from Argo, so a keyboard left over from an older build — or a
/// hand-crafted press — has to be discarded rather than mapped onto whatever now
/// occupies that index.
fn parse_press(data: &str) -> Option<(u64, Press)> {
    let (generation, choice) = data.split_once(':')?;
    let generation = generation.parse().ok()?;
    let press = match choice {
        SKIP => Press::Skip,
        CANCEL => Press::Cancel,
        index => Press::Choose(index.parse().ok()?),
    };
    Some((generation, press))
}

impl Bridge {
    /// Routes one button press.
    async fn on_button(self: &Arc<Self>, query: &CallbackQuery) -> Result<()> {
        if !query.is_private_chat() {
            return Ok(());
        }
        let Some((generation, press)) = parse_press(&query.data) else {
            return Ok(());
        };

        // A press is only honoured against the menu that produced it. Checked
        // before the menu is consumed so a stale tap leaves the live menu alone.
        let menu = {
            let mut held = self.menu.lock().await;
            match held.as_ref() {
                Some(menu) if menu.generation == generation => held.take(),
                _ => None,
            }
        };
        let Some(menu) = menu else {
            self.close_menu(
                query.chat_id,
                query.message_id,
                &render::notice("that menu is no longer current — run the command again"),
            )
            .await;
            return Ok(());
        };

        let selected = match press {
            Press::Cancel => {
                self.close_menu(
                    menu.chat_id,
                    menu.message_id,
                    &render::notice("cancelled — nothing changed"),
                )
                .await;
                return Ok(());
            }
            Press::Skip => None,
            Press::Choose(index) => match menu.values.get(index) {
                Some(value) => Some(value.clone()),
                None => return Ok(()),
            },
        };

        match menu.step {
            Step::Agent { backup } => self.chose_agent(&menu, selected, backup).await,
            Step::Model { backup } => self.chose_model(&menu, selected, backup).await,
            Step::Effort { backup } => self.chose_effort(&menu, selected, backup).await,
            Step::Workspace => {
                let Some(root) = selected else { return Ok(()) };
                self.close_menu(
                    menu.chat_id,
                    menu.message_id,
                    &render::notice(&format!("workspace: {}", workspace_name(&root))),
                )
                .await;
                self.switch_workspace(menu.chat_id, &root).await
            }
            Step::Conversation => {
                let Some(id) = selected else { return Ok(()) };
                self.switch_conversation(menu.chat_id, &ConversationId::new(id))
                    .await
            }
            Step::Mode => {
                let Some(mode) = selected else { return Ok(()) };
                self.close_menu(
                    menu.chat_id,
                    menu.message_id,
                    &render::notice(&format!("mode: {mode}")),
                )
                .await;
                self.apply_selection(menu.chat_id, Selection::Mode(mode))
                    .await
            }
        }
    }

    /// Applies the chosen CLI, then offers its models.
    async fn chose_agent(
        self: &Arc<Self>,
        menu: &Menu,
        selected: Option<String>,
        backup: bool,
    ) -> Result<()> {
        let Some(agent) = selected else { return Ok(()) };

        // Clearing the standby is a terminal choice: there is no model to pick
        // for a CLI that will not be used.
        if backup && agent == BACKUP_NONE {
            self.close_menu(
                menu.chat_id,
                menu.message_id,
                &render::notice("backup cleared — failover is off"),
            )
            .await;
            return self
                .apply_selection(menu.chat_id, Selection::Backup(None, None, None))
                .await;
        }

        let info = self.probe(&agent).await?;
        // The placeholder id is excluded so a real model is chosen where the
        // adapter offers one; if it is all there is, it is offered as-is.
        let concrete: Vec<_> = info
            .models
            .iter()
            .filter(|model| model.id != argo_runtime::DEFAULT_MODEL_ID)
            .collect();
        let models: Vec<_> = if concrete.is_empty() {
            info.models.iter().collect()
        } else {
            concrete
        };
        if models.is_empty() {
            self.close_menu(
                menu.chat_id,
                menu.message_id,
                &render::notice(&format!("{agent} — no selectable models")),
            )
            .await;
            return self.finish(menu.chat_id, backup, agent, None, None).await;
        }

        let choices = models
            .iter()
            .map(|model| (clip(&model.label, 40), model.id.clone()))
            .collect();
        self.advance_menu(
            menu.chat_id,
            menu.message_id,
            Draft {
                prompt: render::notice(&format!("{}: choose a model", scope(backup))),
                step: Step::Model { backup },
                choices,
                agent: Some(agent),
                model: None,
                skippable: false,
            },
        )
        .await
    }

    /// Applies the chosen model, then offers effort levels when the model has any.
    async fn chose_model(
        self: &Arc<Self>,
        menu: &Menu,
        selected: Option<String>,
        backup: bool,
    ) -> Result<()> {
        let (Some(model), Some(agent)) = (selected, menu.agent.clone()) else {
            return Ok(());
        };
        let info = self.probe(&agent).await?;
        let levels = info.reasoning_for(Some(&model));
        if levels.is_empty() {
            self.close_menu(
                menu.chat_id,
                menu.message_id,
                &render::notice(&format!("{agent} / {model}")),
            )
            .await;
            return self
                .finish(menu.chat_id, backup, agent, Some(model), None)
                .await;
        }
        let choices = levels
            .iter()
            .map(|level| (clip(&level.label, 24), level.id.clone()))
            .collect();
        self.advance_menu(
            menu.chat_id,
            menu.message_id,
            Draft {
                prompt: render::notice(&format!("{agent} / {model}: choose effort")),
                step: Step::Effort { backup },
                choices,
                agent: Some(agent),
                model: Some(model),
                // Effort is genuinely optional, and skipping leaves the adapter
                // default rather than forcing a level the user did not want.
                skippable: true,
            },
        )
        .await
    }

    /// Applies the chosen effort and closes the wizard.
    async fn chose_effort(
        self: &Arc<Self>,
        menu: &Menu,
        selected: Option<String>,
        backup: bool,
    ) -> Result<()> {
        let Some(agent) = menu.agent.clone() else {
            return Ok(());
        };
        let summary = match (&menu.model, &selected) {
            (Some(model), Some(effort)) => format!("{agent} / {model} · {effort}"),
            (Some(model), None) => format!("{agent} / {model}"),
            _ => agent.clone(),
        };
        self.close_menu(menu.chat_id, menu.message_id, &render::notice(&summary))
            .await;
        self.finish(menu.chat_id, backup, agent, menu.model.clone(), selected)
            .await
    }

    /// Commits a completed wizard.
    async fn finish(
        self: &Arc<Self>,
        chat: i64,
        backup: bool,
        agent: String,
        model: Option<String>,
        reasoning: Option<String>,
    ) -> Result<()> {
        let selection = if backup {
            Selection::Backup(Some(agent), model, reasoning)
        } else {
            Selection::Primary(agent, model, reasoning)
        };
        self.apply_selection(chat, selection).await
    }

    /// Probes an adapter for its models and effort levels.
    async fn probe(&self, agent_id: &str) -> Result<argo_runtime::AgentInfo> {
        match dispatch(
            &self.daemon,
            Request::ProbeAgent {
                agent_id: agent_id.to_string(),
                refresh: false,
            },
        )
        .await
        {
            Response::Agent { agent } => Ok(*Box::new(agent)),
            other => Err(unexpected(other)),
        }
    }
}

/// What a completed picker applies.
enum Selection {
    /// CLI, model, and effort for the conversation itself.
    Primary(String, Option<String>, Option<String>),
    /// CLI, model, and effort for the standby, or all-none to disable failover.
    Backup(Option<String>, Option<String>, Option<String>),
    /// Execution mode.
    Mode(String),
}

/// Payload marking the "no backup" button, which no real agent id can collide with.
const BACKUP_NONE: &str = "\u{0}none";
fn validate_link_challenge(challenge: &str) -> Result<()> {
    if challenge.len() < 20
        || !challenge
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(argo_core::error::ArgoError::Invalid(
            "Telegram link challenge is malformed".into(),
        ));
    }
    Ok(())
}

/// Signals the current link window without waiting for its poll to unwind.
async fn signal_link_cancel(daemon: &Arc<Daemon>, challenge: Option<&str>) -> bool {
    let attempt = daemon.telegram_link.lock().await.clone();
    let Some(attempt) = attempt else {
        return false;
    };
    if challenge.is_some_and(|expected| !attempt.matches(expected)) {
        return false;
    }
    attempt.cancel.cancel();
    true
}

/// Removes a link window only when it is still the named attempt.
async fn clear_link_attempt(daemon: &Arc<Daemon>, challenge: &str) -> bool {
    let mut current = daemon.telegram_link.lock().await;
    if current
        .as_ref()
        .is_some_and(|attempt| attempt.matches(challenge))
    {
        *current = None;
        true
    } else {
        false
    }
}

/// Cancels one window, waits until it owns no poll, and restores the bridge.
async fn cancel_link_and_restore(daemon: &Arc<Daemon>, challenge: &str) -> bool {
    if !signal_link_cancel(daemon, Some(challenge)).await {
        return false;
    }
    let ownership = POLLING.lock().await;
    stop_bridge().await;
    let cleared = clear_link_attempt(daemon, challenge).await;
    drop(ownership);
    spawn(Arc::clone(daemon));
    cleared
}

/// Handles the `Telegram*` requests that back the `/telegram` wizard.
pub(crate) async fn handle(daemon: &Arc<Daemon>, request: Request) -> Result<Response> {
    let paths = daemon.paths().clone();
    match request {
        Request::TelegramStatus => Ok(status(daemon)),

        Request::TelegramConnect { token } => {
            let token = token.trim().to_string();
            if !config::looks_like_token(&token) {
                return Err(argo_core::error::ArgoError::Invalid(
                    "that does not look like a bot token — BotFather sends something like 8123456789:AA...".into(),
                ));
            }
            if let Ok(environment_token) = std::env::var(config::TOKEN_ENV) {
                let environment_token = environment_token.trim();
                if !environment_token.is_empty() && environment_token != token {
                    return Err(argo_core::error::ArgoError::Invalid(format!(
                        "{} overrides the stored Telegram token; update or unset it before connecting a different bot",
                        config::TOKEN_ENV
                    )));
                }
            }

            // Confirms the token works before stopping the current bridge or
            // writing anything, so a typo cannot disconnect a working bot.
            let identity = Bot::with_token(token.clone()).get_me().await?;

            // Token/config generation changes are serialized with getUpdates.
            // Otherwise a link opened on the old bot could authorize its sender
            // into the replacement bot's config and copy an unrelated offset.
            signal_link_cancel(daemon, None).await;
            let ownership = POLLING.lock().await;
            stop_bridge().await;
            *daemon.telegram_link.lock().await = None;

            let previous_token = config::load_token(&paths)?;
            let previous_config = config::load(&paths)?.unwrap_or_default();
            let token_changed = previous_token.as_deref() != Some(token.as_str());
            let has_bot_state = previous_config.bot_username.is_some()
                || !previous_config.allowed_user_ids.is_empty()
                || previous_config.update_offset != 0;
            let replacement = token_changed && (previous_token.is_some() || has_bot_state);

            let linked = config::mutate(&paths, |config| {
                config.bind_bot(identity.username.clone(), replacement);
                config.is_linked()
            })?;
            config::save_token(&paths, &token)?;
            drop(ownership);
            if linked {
                spawn(Arc::clone(daemon));
            }
            Ok(status(daemon))
        }

        Request::TelegramPrepareLink { challenge } => {
            validate_link_challenge(&challenge)?;
            signal_link_cancel(daemon, None).await;
            let ownership = POLLING.lock().await;
            // A superseded wait may have restored the bridge while this request
            // was waiting for ownership, so stop it only after the lock is ours.
            stop_bridge().await;
            *daemon.telegram_link.lock().await = None;

            let prepared: Result<LinkAttempt> = async {
                let Some(token) = config::load_token(&paths)? else {
                    return Err(argo_core::error::ArgoError::Invalid(
                        "connect a bot token first".into(),
                    ));
                };
                let bot = Bot::with_token(token.clone());
                bot.delete_webhook().await?;

                // Establish the baseline before the client displays the challenge.
                // Anything arriving afterwards belongs to this exact window.
                let mut offset = config::load(&paths)?.unwrap_or_default().update_offset;
                let stale = bot.get_updates(offset, 0).await?;
                if let Some(high_water) = stale.high_water {
                    offset = high_water.saturating_add(1);
                    config::mutate(&paths, |saved| {
                        saved.update_offset = saved.update_offset.max(offset);
                    })?;
                }
                Ok(LinkAttempt {
                    challenge: challenge.clone(),
                    token,
                    cancel: CancelToken::new(),
                })
            }
            .await;

            match prepared {
                Ok(attempt) => {
                    *daemon.telegram_link.lock().await = Some(attempt);
                    drop(ownership);
                    Ok(Response::Ok)
                }
                Err(error) => {
                    drop(ownership);
                    spawn(Arc::clone(daemon));
                    Err(error)
                }
            }
        }

        Request::TelegramLink {
            challenge,
            timeout_ms,
            root,
        } => {
            validate_link_challenge(&challenge)?;
            if timeout_ms == 0 {
                return Err(argo_core::error::ArgoError::Invalid(
                    "Telegram link timeout must be positive".into(),
                ));
            }
            let attempt = daemon
                .telegram_link
                .lock()
                .await
                .clone()
                .filter(|attempt| attempt.matches(&challenge))
                .ok_or_else(|| {
                    argo_core::error::ArgoError::Invalid(
                        "prepare this Telegram link challenge before waiting for it".into(),
                    )
                })?;

            let ownership = POLLING.lock().await;
            let still_current = daemon
                .telegram_link
                .lock()
                .await
                .as_ref()
                .is_some_and(|current| current.matches(&challenge));
            if !still_current || attempt.cancel.is_cancelled() {
                drop(ownership);
                return Err(argo_core::error::ArgoError::Cancelled);
            }
            if config::load_token(&paths)?.as_deref() != Some(attempt.token.as_str()) {
                clear_link_attempt(daemon, &challenge).await;
                drop(ownership);
                spawn(Arc::clone(daemon));
                return Err(argo_core::error::ArgoError::Invalid(
                    "the Telegram bot changed after this link window was prepared".into(),
                ));
            }

            let bot = Bot::with_token(attempt.token.clone());
            let link_result: Result<(TelegramConfig, IncomingMessage)> = async {
                let mut offset = config::load(&paths)?.unwrap_or_default().update_offset;
                let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                loop {
                    if attempt.cancel.is_cancelled() {
                        return Err(argo_core::error::ArgoError::Cancelled);
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(argo_core::error::ArgoError::Timeout(timeout_ms));
                    }
                    let poll_secs = remaining.as_secs().clamp(1, 20);
                    let poll = bot.get_updates(offset, poll_secs);
                    let batch = tokio::select! {
                        _ = attempt.cancel.cancelled() => {
                            return Err(argo_core::error::ArgoError::Cancelled);
                        }
                        result = tokio::time::timeout(remaining, poll) => {
                            match result {
                                Ok(result) => result?,
                                Err(_) => return Err(argo_core::error::ArgoError::Timeout(timeout_ms)),
                            }
                        }
                    };

                    let next_offset = batch
                        .high_water
                        .map(|high_water| high_water.saturating_add(1));
                    let linked = batch.updates.into_iter().find_map(|update| match update {
                        Update::Message(message) if is_link_message(&message, &challenge) => {
                            Some(message)
                        }
                        _ => None,
                    });

                    if let Some(message) = linked {
                        // Authorization and acknowledgement of its proof are one
                        // disk mutation: a crash cannot consume the command without
                        // also recording the user and workspace it authorized.
                        let linked_config = config::mutate(&paths, |saved| {
                            saved.allow_user(message.from_id);
                            saved.allow_workspace(root.clone());
                            if let Some(next) = next_offset {
                                saved.update_offset = saved.update_offset.max(next);
                            }
                            saved.clone()
                        })?;
                        return Ok((linked_config, message));
                    }

                    if let Some(next) = next_offset {
                        offset = offset.max(next);
                        config::mutate(&paths, |saved| {
                            saved.update_offset = saved.update_offset.max(offset);
                        })?;
                    }
                }
            }
            .await;

            clear_link_attempt(daemon, &challenge).await;
            let outcome = match link_result {
                Ok((linked_config, message)) => {
                    if let Err(error) = bot.set_my_commands(COMMANDS).await {
                        tracing::warn!(%error, "could not publish the telegram command menu");
                    }
                    let _ = bot
                        .send_message(
                            message.chat_id,
                            &welcome(
                                linked_config
                                    .active_workspace
                                    .as_deref()
                                    .map(workspace_name)
                                    .unwrap_or_else(|| "Argo".into()),
                            ),
                            ParseMode::MarkdownV2,
                            false,
                        )
                        .await;
                    Ok(status(daemon))
                }
                Err(error) => Err(error),
            };
            drop(ownership);
            spawn(Arc::clone(daemon));
            outcome
        }

        Request::TelegramCancelLink { challenge } => {
            validate_link_challenge(&challenge)?;
            // Report whether cancellation actually won. If authorization or
            // timeout completed first, return current status so a client cannot
            // claim remote access was cancelled when it remains linked.
            if cancel_link_and_restore(daemon, &challenge).await {
                Ok(Response::Ok)
            } else {
                Ok(status(daemon))
            }
        }

        Request::TelegramAllowWorkspace { root } => {
            let (outcome, updated) = config::mutate(&paths, |config| {
                let outcome = config.allow_workspace(root.clone());
                (outcome, config.clone())
            })?;
            announce_workspace(&paths, &updated, &root, outcome).await;
            Ok(status(daemon))
        }

        Request::TelegramAllowUser { user_id, root } => {
            signal_link_cancel(daemon, None).await;
            let ownership = POLLING.lock().await;
            stop_bridge().await;
            *daemon.telegram_link.lock().await = None;
            let current = config::load(&paths)?.unwrap_or_default();
            let mut high_water = None;
            // Old updates are skipped so authorizing does not immediately
            // replay whatever was said before this user was trusted.
            if let Some(token) = config::load_token(&paths)? {
                let bot = Bot::with_token(token);
                if let Err(error) = bot.delete_webhook().await {
                    tracing::warn!(%error, "could not clear telegram webhook before polling");
                }
                if let Ok(batch) = bot.get_updates(current.update_offset, 0).await {
                    high_water = batch.high_water;
                }
            }
            let (outcome, updated) = config::mutate(&paths, |config| {
                config.allow_user(user_id);
                let outcome = config.allow_workspace(root.clone());
                if let Some(latest) = high_water {
                    config.update_offset = config.update_offset.max(latest.saturating_add(1));
                }
                (outcome, config.clone())
            })?;
            drop(ownership);
            announce_workspace(&paths, &updated, &root, outcome).await;
            spawn(Arc::clone(daemon));
            Ok(status(daemon))
        }

        Request::TelegramStart => {
            if daemon.telegram_link.lock().await.is_some() {
                return Err(argo_core::error::ArgoError::Invalid(
                    "cancel or finish the active Telegram link window before starting the bridge"
                        .into(),
                ));
            }
            spawn(Arc::clone(daemon));
            // The spawned task claims the running flag before its first poll, so
            // a status read here could race it; a brief settle keeps the reply
            // honest.
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok(status(daemon))
        }

        Request::TelegramReset | Request::TelegramRemove => {
            signal_link_cancel(daemon, None).await;
            let ownership = POLLING.lock().await;
            stop_bridge().await;
            *daemon.telegram_link.lock().await = None;
            config::remove(&paths)?;
            drop(ownership);
            Ok(status(daemon))
        }

        other => Err(unexpected(Response::Error {
            code: "INVALID_REQUEST".into(),
            message: format!("{other:?} is not a telegram request"),
            retryable: false,
        })),
    }
}

/// Tells the chat that a workspace was allowlisted, and whether it is now live.
///
/// Silent when the root was already known, so re-running `allow` from the same
/// directory does not post a duplicate every time. Best effort throughout: this
/// is a courtesy message, and failing to deliver it must not fail the request
/// that allowed the workspace.
async fn announce_workspace(
    paths: &argo_core::paths::ArgoPaths,
    config: &TelegramConfig,
    root: &str,
    outcome: config::Allowed,
) {
    let text = match outcome {
        config::Allowed::AlreadyKnown => return,
        config::Allowed::Activated => format!(
            "📂 workspace *{}* is now active",
            escape(&workspace_name(root))
        ),
        // Spelled out rather than switched automatically: a turn may be running
        // against the current workspace, and hijacking it from a terminal would
        // be worse than one extra tap.
        config::Allowed::AddedInactive => format!(
            "📂 workspace *{}* added\n{}",
            escape(&workspace_name(root)),
            escape(&format!(
                "still working in {} — send /ws to switch",
                config
                    .active_workspace
                    .as_deref()
                    .map(workspace_name)
                    .unwrap_or_else(|| "none".into())
            ))
        ),
    };
    // A bot's private chat id is the user's own id, which is the only chat the
    // bridge is ever authorized to talk in.
    let Some(chat_id) = config.allowed_user_ids.first().copied() else {
        return;
    };
    let Ok(Some(token)) = config::load_token(paths) else {
        return;
    };
    if let Err(error) = Bot::with_token(token)
        .send_message(chat_id, &text, ParseMode::MarkdownV2, false)
        .await
    {
        tracing::debug!(%error, "could not announce the new telegram workspace");
    }
}

/// Accepts only the complete command generated for this linking window.
fn is_link_command(text: &str, challenge: &str) -> bool {
    text == format!("/link {challenge}")
}

/// Linking is valid only in the sender's private bot chat.
fn is_link_message(message: &IncomingMessage, challenge: &str) -> bool {
    message.is_private_chat() && is_link_command(&message.text, challenge)
}

/// The first message the bot ever sends.
///
/// Doubles as the command reference: a bot that says only "connected" leaves the
/// user staring at an empty chat with no idea what it accepts.
fn welcome(workspace: String) -> String {
    let mut lines = vec![
        format!("✅ *Argo connected* — {}", escape(&workspace)),
        String::new(),
        escape("Send any message to talk to the agent. Commands:"),
        String::new(),
    ];
    for (command, detail) in COMMANDS {
        lines.push(format!("/{} — {}", escape(command), escape(detail)));
    }
    lines.join("\n")
}

/// Shorthand for MarkdownV2 escaping inside this module.
fn escape(text: &str) -> String {
    argo_telegram::markdown_v2::escape(text)
}

/// Builds the status reply from what is currently on disk.
fn status(daemon: &Arc<Daemon>) -> Response {
    let config = config::load(daemon.paths())
        .ok()
        .flatten()
        .unwrap_or_default();
    let has_token = config::load_token(daemon.paths()).ok().flatten().is_some();
    Response::Telegram {
        linked: config.is_linked() && has_token,
        // The poll loop is started once at daemon startup, so a bridge that is
        // linked now but was not at boot needs a restart to come up.
        running: RUNNING.load(std::sync::atomic::Ordering::Relaxed),
        bot_username: config.bot_username.clone(),
        allowed_user_ids: config.allowed_user_ids.clone(),
        workspaces: config.workspaces.clone(),
        active_workspace: config.active_workspace.clone(),
    }
}

/// Whether a poll loop is live in this process.
static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Exactly one flow may consume `getUpdates` at a time.
static POLLING: Mutex<()> = Mutex::const_new(());

/// Incremented whenever the currently bound bridge must stop.
static BRIDGE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static BRIDGE_CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();
static BRIDGE_STOPPED: tokio::sync::Notify = tokio::sync::Notify::const_new();
static ACTIVE_BRIDGE: std::sync::Mutex<Option<std::sync::Weak<Bridge>>> =
    std::sync::Mutex::new(None);

/// Cancels in-flight polling and turns, then waits until the old token is unused.
async fn stop_bridge() {
    BRIDGE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    BRIDGE_CHANGED.notify_waiters();
    let active = ACTIVE_BRIDGE
        .lock()
        .ok()
        .and_then(|bridge| bridge.as_ref().and_then(std::sync::Weak::upgrade));
    if let Some(bridge) = active {
        bridge.revoke().await;
    }
    while RUNNING.load(std::sync::atomic::Ordering::SeqCst) {
        let stopped = BRIDGE_STOPPED.notified();
        if !RUNNING.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        stopped.await;
    }
}

/// A reply the bridge did not ask for, which means the protocol drifted.
fn unexpected(response: Response) -> argo_core::error::ArgoError {
    argo_core::error::ArgoError::Protocol(format!("unexpected daemon reply: {response:?}"))
}

/// Clips to Telegram's ceiling for an in-place edit, which cannot be split.
fn clip(text: &str, limit: usize) -> String {
    if utf16_len(text) <= limit {
        return text.to_string();
    }
    // Keep the tail: while a turn streams, the newest output is what the reader
    // is waiting on. Reserve one unit for the ellipsis.
    let budget = limit.saturating_sub(1);
    let mut units = 0;
    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        let next = units + character.len_utf16();
        if next > budget {
            break;
        }
        units = next;
        start = index;
    }
    format!("…{}", &text[start..])
}

/// Reduces a workspace root to its directory name for display.
fn workspace_name(root: &str) -> String {
    std::path::Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string())
}

/// Builds the recap shown when the active session changes.
fn recap_from(summary: &ConversationSummary, messages: &[MessageView]) -> Recap {
    let last = |role: &str| {
        messages
            .iter()
            .rev()
            .find(|message| message.role == role && !message.text.trim().is_empty())
            .map(|message| message.text.clone())
    };
    Recap {
        workspace: summary.workspace.as_deref().map(workspace_name),
        title: summary.title.clone(),
        agent: summary.selected_agent_id.clone(),
        model: summary.selected_model.clone(),
        mode: summary
            .selected_mode
            .as_deref()
            .and_then(argo_core::mode::AgentMode::parse)
            .map(|mode| mode.label().to_string()),
        backup: summary.selected_backup_agent_id.as_ref().map(|agent| {
            match &summary.selected_backup_model {
                Some(model) => format!("{agent}/{model}"),
                None => agent.clone(),
            }
        }),
        message_count: summary.message_count,
        description: summary.description.clone(),
        last_user: last("user"),
        last_assistant: last("assistant"),
    }
}

/// Accumulated state for one streaming turn.
#[derive(Default)]
struct TurnView {
    text: String,
    tools: Vec<ToolProgress>,
    notices: Vec<String>,
    failed: Option<String>,
}

impl TurnView {
    fn apply(&mut self, kind: &RunEventKind) {
        match kind {
            RunEventKind::TextDelta { text } => self.text.push_str(text),
            RunEventKind::ToolStarted { name, .. } => self.tools.push(ToolProgress {
                name: name.clone(),
                done: false,
                ok: true,
            }),
            RunEventKind::ToolCompleted { ok, .. } => {
                if let Some(tool) = self.tools.iter_mut().rev().find(|tool| !tool.done) {
                    tool.done = true;
                    tool.ok = *ok;
                }
            }
            RunEventKind::Error { message, .. } => self.failed = Some(message.clone()),
            RunEventKind::BackupFailover { detail, .. }
            | RunEventKind::Diagnostic { detail, .. }
            | RunEventKind::SessionReseeded { reason: detail } => self.notices.push(detail.clone()),
            _ => {}
        }
    }

    fn bubble(&self, finished: bool) -> String {
        self.bubble_with_text(&self.text, finished)
    }

    fn bubble_with_text(&self, text: &str, finished: bool) -> String {
        let mut out = render::reply_bubble(text, finished, self.failed.as_deref());
        for notice in &self.notices {
            out.push_str("\n\n");
            out.push_str(&render::notice(notice));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a draft with `count` choices of the given label width.
    fn draft(count: usize, width: usize, skippable: bool) -> Draft {
        Draft {
            prompt: "pick".into(),
            step: Step::Agent { backup: false },
            choices: (0..count)
                .map(|index| ("x".repeat(width), format!("value-{index}")))
                .collect(),
            agent: None,
            model: None,
            skippable,
        }
    }

    #[test]
    fn linking_requires_the_senders_private_chat() {
        let challenge = "abcdefghijklmnopqrst";
        let private = IncomingMessage {
            update_id: 1,
            chat_id: 42,
            message_id: 2,
            from_id: 42,
            text: format!("/link {challenge}"),
        };
        assert!(is_link_message(&private, challenge));

        let group = IncomingMessage {
            chat_id: -100,
            ..private.clone()
        };
        assert!(!is_link_message(&group, challenge));
        assert!(!is_link_message(&private, "different-challenge"));
    }

    #[test]
    fn button_payloads_address_choices_positionally_across_rows() {
        // Two-per-row layout must not restart the index inside each row: doing so
        // makes every button in the second row apply the first row's choice.
        let rows = draft(5, 4, false).rows(7);
        let payloads: Vec<&str> = rows
            .iter()
            .flatten()
            .map(|(_, data)| data.as_str())
            .collect();
        assert_eq!(
            payloads,
            vec!["7:0", "7:1", "7:2", "7:3", "7:4", &format!("7:{CANCEL}")]
        );
    }

    #[test]
    fn wide_labels_get_a_row_each_and_narrow_ones_share() {
        // Telegram shrinks button text to fit rather than wrapping, so two model
        // ids side by side are unreadable on a phone.
        let wide = draft(4, NARROW_LABEL + 1, false).rows(1);
        assert!(
            wide.iter().take(4).all(|row| row.len() == 1),
            "expected one wide button per row, got {wide:?}"
        );
        let narrow = draft(4, 3, false).rows(1);
        assert!(
            narrow.iter().take(2).all(|row| row.len() == 2),
            "expected paired narrow buttons, got {narrow:?}"
        );
    }

    #[test]
    fn every_payload_fits_the_callback_limit() {
        // Exceeding it makes sendMessage fail outright, which is why choices are
        // addressed by index rather than by name.
        let rows = draft(200, 60, true).rows(u64::MAX);
        for (_, data) in rows.iter().flatten() {
            assert!(
                data.len() <= argo_telegram::MAX_CALLBACK_DATA,
                "{data} exceeds the callback payload limit"
            );
        }
    }

    #[test]
    fn only_an_optional_step_offers_skip_and_every_step_offers_cancel() {
        let required = draft(2, 4, false).rows(1);
        let trailer = required.last().expect("trailer row");
        assert_eq!(trailer.len(), 1, "a required step must not offer skip");
        assert!(trailer[0].1.ends_with(CANCEL));

        let optional = draft(2, 4, true).rows(1);
        let trailer = optional.last().expect("trailer row");
        assert_eq!(trailer.len(), 2);
        assert!(trailer[0].1.ends_with(SKIP));
        assert!(trailer[1].1.ends_with(CANCEL));
    }

    #[test]
    fn the_no_backup_payload_cannot_collide_with_a_real_agent_id() {
        // It travels through the same value slot as an agent id, so it has to be
        // something no adapter could ever be called.
        assert!(argo_runtime::find(BACKUP_NONE).is_none());
        assert!(BACKUP_NONE.contains('\u{0}'));
    }

    #[test]
    fn allowing_a_second_workspace_does_not_redirect_an_active_chat() {
        // The exact confusion this reports on: allowing a directory from a
        // terminal must not silently repoint a phone that is working elsewhere.
        let mut config = TelegramConfig::default();
        assert_eq!(config.allow_workspace("/first"), config::Allowed::Activated);
        assert_eq!(config.active_workspace.as_deref(), Some("/first"));

        assert_eq!(
            config.allow_workspace("/second"),
            config::Allowed::AddedInactive
        );
        assert_eq!(
            config.active_workspace.as_deref(),
            Some("/first"),
            "allowing must not switch the active workspace"
        );
        assert_eq!(config.workspaces.len(), 2);

        // Re-allowing is silent, so repeating the command does not spam the chat.
        assert_eq!(
            config.allow_workspace("/second"),
            config::Allowed::AlreadyKnown
        );
        assert_eq!(config.workspaces.len(), 2);
    }

    #[test]
    fn advancing_offset_reloads_a_workspace_allowed_by_the_tui() {
        // The running bridge may have an older in-memory snapshot when the TUI
        // adds a workspace. Consuming the next update must not erase that root.
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = argo_core::ArgoPaths::with_root(directory.path().join("data"));
        let mut cached = TelegramConfig {
            bot_username: Some("argo_test_bot".into()),
            ..Default::default()
        };
        cached.allow_user(42);
        cached.allow_workspace("/first");
        config::save(&paths, &cached).expect("save initial config");

        let mut updated = cached.clone();
        updated.allow_workspace("/second");
        config::save(&paths, &updated).expect("save TUI update");

        Bridge::advance_offset_config(&paths, &mut cached, 41).expect("advance offset");

        assert_eq!(cached.update_offset, 42);
        assert_eq!(cached.workspaces, vec!["/first", "/second"]);
        assert_eq!(config::load(&paths).expect("reload config"), Some(cached));
    }

    #[test]
    fn the_plain_text_fallback_removes_escapes_rather_than_showing_them() {
        // A message that arrives looking wrong beats one that never arrives, but
        // it must not arrive full of backslashes.
        assert_eq!(plain_text("run\\.rs in argo\\-tui"), "run.rs in argo-tui");
        assert_eq!(plain_text("a\\\\b"), "a\\b");
        assert_eq!(plain_text("plain"), "plain");
        // A trailing lone backslash must not panic or duplicate.
        assert_eq!(plain_text("trailing\\"), "trailing");
    }

    #[test]
    fn an_edit_keeps_the_newest_output_when_it_overflows() {
        // Edits cannot be split across messages, so the tail is what survives:
        // during a stream the newest text is what the reader is waiting for.
        let text: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let clipped = clip(&text, 100);
        assert_eq!(clipped.chars().count(), 100);
        assert!(clipped.starts_with('…'));
        assert!(clipped.ends_with("line 199\n"), "{clipped}");
        assert_eq!(clip("short", 100), "short");
    }

    #[test]
    fn a_workspace_is_shown_by_its_directory_name() {
        assert_eq!(workspace_name("/Users/m/WORK/agentmux"), "agentmux");
        assert_eq!(workspace_name("relative"), "relative");
    }

    #[test]
    fn streamed_events_accumulate_into_one_bubble() {
        let mut view = TurnView::default();
        view.apply(&RunEventKind::TextDelta {
            text: "Hello ".into(),
        });
        view.apply(&RunEventKind::ToolStarted {
            id: "1".into(),
            name: "Read".into(),
            input: None,
        });
        view.apply(&RunEventKind::TextDelta {
            text: "world".into(),
        });
        view.apply(&RunEventKind::ToolCompleted {
            id: "1".into(),
            output: None,
            ok: true,
        });

        assert_eq!(view.text, "Hello world");
        assert_eq!(view.tools.len(), 1);
        assert!(view.tools[0].done && view.tools[0].ok);
        let bubble = view.bubble(true);
        assert!(bubble.contains("Hello world"), "{bubble}");
        assert!(!bubble.contains('▌'), "finished bubbles drop the cursor");
    }

    #[test]
    fn a_failover_notice_reaches_the_chat() {
        // The whole point of failover is that the user finds out it happened.
        let mut view = TurnView::default();
        view.apply(&RunEventKind::BackupFailover {
            from_agent_id: argo_core::ids::AgentId::new("claude"),
            from_model: Some("sonnet".into()),
            from_reasoning: None,
            to_agent_id: argo_core::ids::AgentId::new("codex"),
            to_model: Some("gpt-5.6-codex".into()),
            to_reasoning: None,
            detail: "claude reported its plan is exhausted — continuing on codex".into(),
        });
        view.apply(&RunEventKind::TextDelta {
            text: "carrying on".into(),
        });
        let bubble = view.bubble(true);
        assert!(bubble.contains("continuing on codex"), "{bubble}");
        assert!(bubble.contains("carrying on"), "{bubble}");
    }

    #[test]
    fn a_failed_turn_surfaces_its_error() {
        let mut view = TurnView::default();
        view.apply(&RunEventKind::Error {
            code: "AGENT_ERROR".into(),
            message: "usage limit reached".into(),
            retryable: false,
        });
        let bubble = view.bubble(true);
        assert!(bubble.contains("usage limit reached"), "{bubble}");
        assert!(bubble.contains("⚠️"), "{bubble}");
    }
}
