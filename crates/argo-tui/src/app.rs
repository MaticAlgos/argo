//! TUI state.
//!
//! The state is a plain struct mutated by explicit methods, so the interesting
//! behavior — composer editing, streaming assembly, picker navigation — is
//! testable without a terminal attached.

use argo_core::event::{RunEventKind, RunStatus};
use argo_core::ids::{ConversationId, RunId};
use argo_core::message::{ContentBlock, ToolStatus};
use argo_daemon::protocol::{ConversationSummary, MessageView};
use argo_runtime::AgentInfo;

/// A rendered transcript line.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// Who produced it.
    pub kind: LineKind,
    /// Text content.
    pub text: String,
}

/// Classification used for styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A user turn.
    User,
    /// Assistant prose.
    Assistant,
    /// Reasoning explicitly emitted by the CLI.
    ///
    /// Some providers never expose reasoning; Argo displays only what arrives on
    /// the wire and never fabricates or requests hidden chain-of-thought.
    Thinking,
    /// A header naming the agent and model.
    AgentHeader,
    /// Tool or file activity.
    Activity,
    /// Argo's own notice.
    Notice,
    /// An error.
    Error,
}

/// What pressing Enter should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterAction {
    /// Send the composer contents.
    Submit,
    /// Replace the composer with the highlighted suggestion.
    AcceptCompletion,
}

/// What the overlay pane is showing, if anything.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// Nothing; the transcript has the full pane.
    None,
    /// A selectable, filterable list.
    ///
    /// Filtering is not a nicety: OpenCode reports several hundred models, and an
    /// unfiltered list of that size cannot be navigated with arrow keys.
    Picker {
        /// Title shown above the list.
        title: String,
        /// All items, pre-rendered.
        items: Vec<String>,
        /// Values submitted when an item is chosen, parallel to `items`.
        values: Vec<String>,
        /// Highlighted index into the filtered view.
        selected: usize,
        /// Current filter text.
        filter: String,
        /// What choosing an item does.
        action: PickerAction,
    },
    /// Scrollable read-only text.
    Text {
        /// Title shown above the text.
        title: String,
        /// Body lines.
        lines: Vec<String>,
        /// First visible line.
        scroll: usize,
    },
}

/// What a picker selection applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerAction {
    /// Switch agent.
    Agent,
    /// Switch model.
    Model,
    /// Set reasoning effort.
    Effort,
    /// Open a conversation.
    Conversation,
    /// Set the execution mode.
    Mode,
    /// Submit an option offered by the latest assistant response.
    ResponseOption,
}

/// Whole-application state.
pub struct App {
    /// Canonical workspace root.
    pub workspace: String,
    /// Active conversation, when one is open.
    pub conversation: Option<ConversationSummary>,
    /// Conversations in this workspace.
    pub conversations: Vec<ConversationSummary>,
    /// Detected adapters.
    pub agents: Vec<AgentInfo>,
    /// Transcript lines.
    pub lines: Vec<Line>,
    /// Composer contents.
    pub input: String,
    /// Caret position, as a char index.
    pub cursor: usize,
    /// Run currently streaming, if any.
    pub active_run: Option<RunId>,
    /// Overlay pane.
    pub overlay: Overlay,
    /// Status line message.
    pub status: String,
    /// True once the user asked to quit.
    pub should_quit: bool,
    /// Transcript scrollback offset from the bottom.
    pub scroll_back: usize,
    /// Width-aware maximum rendered-row scroll, refreshed by the renderer.
    scroll_limit: std::cell::Cell<usize>,
    /// Recent inputs, newest last.
    history: Vec<String>,
    /// Position while browsing history.
    history_cursor: Option<usize>,
    /// Assistant text accumulated for the streaming turn.
    streaming: String,
    /// Reasoning text accumulated for the streaming turn.
    thinking_streaming: String,
    /// Tool ids to display names, retained until completion events arrive.
    active_tools: std::collections::HashMap<String, String>,
    /// Live command suggestions for the current composer contents.
    pub completions: Vec<&'static str>,
    /// Highlighted suggestion, so the list is navigable rather than fixed.
    pub completion_index: usize,
    /// True once the user moved through the suggestion list with the arrows.
    ///
    /// Distinguishes "I am picking from this list" from "the list happens to be
    /// showing while I finish typing", which decides what Enter does.
    completion_touched: bool,
    /// Tokens the last turn reported, when the CLI reported any.
    pub last_usage: Option<argo_core::event::TokenUsage>,
    /// Agent/model attribution for the last completed turn, including turns that
    /// omitted token data.
    pub last_usage_source: Option<String>,
    /// Running estimate of tokens the conversation would replay.
    pub context_tokens: usize,
    /// Argo build version, shown on the splash.
    pub version: String,
    /// Messages typed while a turn was running, sent in order once it ends.
    ///
    /// Dropping them was the alternative, and it lost work: a follow-up thought
    /// typed mid-turn is exactly the thing a user does not want to retype.
    pub queued: std::collections::VecDeque<String>,
    /// Prompt owned by the active run, retained only long enough to offer an
    /// explicit retry after a transient failure.
    active_prompt: Option<String>,
    /// Failed retryable prompt waiting ahead of queued follow-ups.
    retry_prompt: Option<String>,
    /// Whether the active run emitted a retryable terminal error.
    active_error_retryable: bool,
    /// Animation frame, advanced on a timer while a turn runs.
    pub tick: usize,
    /// What the agent is currently doing, for the activity indicator.
    pub activity: Activity,
    /// Event-derived detail for the live activity row, such as the active tool.
    ///
    /// This is never guessed reasoning: emitted thinking remains in transcript
    /// `Thinking` lines, while this field describes only observable stream state.
    activity_detail: Option<String>,
}

/// Message printed after the TUI closes, naming the session and how to return.
///
/// Returns `None` when nothing was said, since an empty conversation is not worth
/// resuming and printing an id for it is just noise.
pub fn farewell(app: &App) -> Option<String> {
    let summary = app.conversation.as_ref()?;
    let said_something = app
        .lines
        .iter()
        .any(|line| matches!(line.kind, LineKind::User | LineKind::Assistant));
    if !said_something {
        return None;
    }

    let id = summary.id.to_string();
    // The short form is what the picker shows and is unambiguous in practice; the
    // full id is given too so it can be scripted without a lookup.
    let short = id.split('-').next().unwrap_or(&id);
    Some(format!(
        "\nsession {short}\n  resume here:      argo tui   then  /resume {short}\n  or from a shell:  argo send --conversation-id {id} \"...\"\n  transcript:       argo show {id}"
    ))
}

/// What the running agent appears to be doing.
///
/// Derived from the event stream rather than guessed, so the indicator reflects
/// reality instead of just spinning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Activity {
    /// Nothing is running.
    #[default]
    Idle,
    /// Waiting for the first output.
    Starting,
    /// Model reasoning is streaming.
    Thinking,
    /// Assistant text is streaming.
    Responding,
    /// A tool is running.
    Working,
}

impl Activity {
    /// Label shown beside the spinner.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "",
            Self::Starting => "starting",
            Self::Thinking => "thinking",
            Self::Responding => "responding",
            Self::Working => "working",
        }
    }
}

impl App {
    /// Builds an empty app for `workspace`.
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            conversation: None,
            conversations: Vec::new(),
            agents: Vec::new(),
            lines: Vec::new(),
            input: String::new(),
            cursor: 0,
            active_run: None,
            overlay: Overlay::None,
            status: "Type a message, or /help for commands".to_string(),
            should_quit: false,
            scroll_back: 0,
            scroll_limit: std::cell::Cell::new(0),
            history: Vec::new(),
            history_cursor: None,
            streaming: String::new(),
            thinking_streaming: String::new(),
            active_tools: std::collections::HashMap::new(),
            completions: Vec::new(),
            completion_index: 0,
            completion_touched: false,
            last_usage: None,
            last_usage_source: None,
            context_tokens: 0,
            version: env!("CARGO_PKG_VERSION").to_string(),
            queued: std::collections::VecDeque::new(),
            active_prompt: None,
            retry_prompt: None,
            active_error_retryable: false,
            tick: 0,
            activity: Activity::Idle,
            activity_detail: None,
        }
    }

    /// Associates the accepted user prompt with the active run.
    pub fn track_active_prompt(&mut self, prompt: String) {
        self.active_prompt = Some(prompt);
        self.active_error_retryable = false;
    }

    /// Retryable failed prompt, which has priority over queued follow-ups.
    pub fn retry_prompt(&self) -> Option<&str> {
        self.retry_prompt.as_deref()
    }

    /// Commits a retry only after the daemon acknowledges `RunStarted`.
    pub fn commit_retry_prompt(&mut self) -> Option<String> {
        self.retry_prompt.take()
    }

    /// Discards a paused failed prompt.
    pub fn clear_retry_prompt(&mut self) -> bool {
        self.retry_prompt.take().is_some()
    }

    /// Queues a message to send when the current turn finishes.
    ///
    /// Returns the queue depth so the caller can tell the user where it landed.
    pub fn enqueue(&mut self, message: String) -> usize {
        self.queued.push_back(message);
        self.queued.len()
    }

    /// Returns the next queued message without removing it.
    ///
    /// Delivery is a two-phase operation: peek, ask the daemon to accept it, then
    /// pop only after `RunStarted`. This prevents a socket or validation error from
    /// silently losing queued work.
    pub fn queued_front(&self) -> Option<&str> {
        self.queued.front().map(String::as_str)
    }

    /// Commits delivery of the oldest queued message.
    pub fn commit_queued(&mut self) -> Option<String> {
        self.queued.pop_front()
    }

    /// Takes the next queued message, if any.
    pub fn dequeue(&mut self) -> Option<String> {
        self.queued.pop_front()
    }

    /// Number of messages waiting.
    pub fn queue_depth(&self) -> usize {
        self.queued.len()
    }

    /// Discards every queued message, returning how many were dropped.
    pub fn clear_queue(&mut self) -> usize {
        std::mem::take(&mut self.queued).len()
    }

    /// Advances the animation. Called on a timer while a turn is running.
    pub fn advance_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Current spinner glyph.
    pub fn spinner(&self) -> char {
        const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        FRAMES[self.tick % FRAMES.len()]
    }

    /// Animated status text, or `None` when nothing is running.
    ///
    /// Labels describe only observable stream state. Actual reasoning text is
    /// rendered separately from `ThinkingDelta` events when a CLI emits it.
    pub fn activity_indicator(&self) -> Option<String> {
        if self.activity == Activity::Idle {
            return None;
        }
        let detail = self
            .activity_detail
            .as_deref()
            .unwrap_or(match self.activity {
                Activity::Idle => "",
                Activity::Starting => "waiting for CLI output",
                Activity::Thinking => "receiving CLI-emitted reasoning",
                Activity::Responding => "streaming response",
                Activity::Working => "tool running",
            });
        let base = format!("{} {} · {detail}", self.spinner(), self.activity.label());
        match self.queue_depth() {
            0 => Some(base),
            1 => Some(format!("{base} · 1 queued")),
            n => Some(format!("{base} · {n} queued")),
        }
    }

    /// True while a turn is streaming.
    pub fn is_busy(&self) -> bool {
        self.active_run.is_some()
    }

    /// Appends a transcript line.
    pub fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        self.lines.push(Line {
            kind,
            text: text.into(),
        });
    }

    /// Sets the status line.
    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
    }

    /// Reports an error in both the transcript and the status line.
    pub fn report_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.push(LineKind::Error, text.clone());
        self.status = text;
    }

    // --- composer editing ---

    /// Inserts a character at the caret.
    pub fn insert(&mut self, ch: char) {
        let index = self.byte_index(self.cursor);
        self.input.insert(index, ch);
        self.cursor += 1;
        self.history_cursor = None;
        self.refresh_completions();
    }

    /// Recomputes command suggestions for the current input.
    ///
    /// Shown as you type rather than only on Tab, so the command surface is
    /// discoverable without knowing it exists.
    pub fn refresh_completions(&mut self) {
        self.completions = crate::commands::complete(&self.input);
        // Reset rather than clamp: the previous highlight refers to a list that no
        // longer exists.
        self.completion_index = 0;
        self.completion_touched = false;
    }

    /// True when a navigable suggestion list is showing.
    pub fn has_completions(&self) -> bool {
        !self.completions.is_empty()
    }

    /// Moves the suggestion highlight, wrapping at both ends.
    ///
    /// Wrapping matters for a short list: reaching the bottom and pressing Down
    /// again should return to the top rather than feel stuck.
    pub fn completion_move(&mut self, delta: i32) {
        if self.completions.is_empty() {
            return;
        }
        let count = self.completions.len() as i32;
        let next = (self.completion_index as i32 + delta).rem_euclid(count);
        self.completion_index = next as usize;
        self.completion_touched = true;
    }

    /// Accepts the highlighted suggestion, if any.
    pub fn accept_completion(&mut self) -> bool {
        let Some(chosen) = self.completions.get(self.completion_index).copied() else {
            return false;
        };
        self.input = format!("{chosen} ");
        self.move_end();
        self.refresh_completions();
        true
    }

    /// What Enter should do for the current composer contents.
    ///
    /// Typing a command in full leaves it showing as its own suggestion, so
    /// unconditionally completing on Enter made the key look broken: the command
    /// never ran. Enter therefore runs whatever is already valid, and only picks
    /// from the list when the user actually navigated it.
    pub fn enter_action(&self) -> EnterAction {
        if self.completion_touched && self.has_completions() {
            return EnterAction::AcceptCompletion;
        }
        let trimmed = self.input.trim();
        if crate::commands::is_command(trimmed) {
            // Already a runnable command, even if a longer name also matches.
            if crate::commands::parse(trimmed).is_ok() {
                return EnterAction::Submit;
            }
            // A partial name: completing it is the useful move.
            if self.has_completions() {
                return EnterAction::AcceptCompletion;
            }
        }
        EnterAction::Submit
    }

    /// Inserts a line break without submitting.
    ///
    /// Multi-line prompts are routine — pasting a stack trace, or writing a
    /// paragraph — so Enter alone must not be the only way to extend the input.
    pub fn insert_newline(&mut self) {
        self.insert('\n');
    }

    /// Caret position as (row, column) within the composer.
    ///
    /// Computed from newlines rather than by dividing the caret index, which is
    /// wrong the moment the input contains a line break.
    pub fn caret_row_column(&self) -> (usize, usize) {
        let mut row = 0usize;
        let mut column = 0usize;
        for (index, ch) in self.input.chars().enumerate() {
            if index >= self.cursor {
                break;
            }
            if ch == '\n' {
                row += 1;
                column = 0;
            } else {
                column += 1;
            }
        }
        (row, column)
    }

    /// Number of lines the composer currently holds.
    pub fn input_line_count(&self) -> usize {
        self.input.lines().count().max(1)
    }

    /// Deletes the character before the caret.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
        self.refresh_completions();
    }

    /// Deletes the character at the caret.
    pub fn delete(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.input.replace_range(start..end, "");
    }

    /// Moves the caret left.
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Moves the caret right.
    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    /// Moves the caret to the start.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Moves the caret to the end.
    pub fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
    }

    /// Clears the composer and returns what it held.
    pub fn take_input(&mut self) -> String {
        let taken = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_cursor = None;
        self.completions.clear();
        self.completion_index = 0;
        if !taken.trim().is_empty() {
            self.history.push(taken.clone());
        }
        taken
    }

    /// Byte offset for a char index.
    ///
    /// The composer is edited by character, but `String` is indexed by byte, so
    /// multi-byte input would panic without this conversion.
    fn byte_index(&self, char_index: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_index)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    /// Recalls the previous input.
    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(current) => current - 1,
        };
        self.history_cursor = Some(next);
        self.input = self.history[next].clone();
        self.cursor = self.input.chars().count();
    }

    /// Moves forward through history, ending at an empty composer.
    pub fn history_next(&mut self) {
        match self.history_cursor {
            Some(current) if current + 1 < self.history.len() => {
                self.history_cursor = Some(current + 1);
                self.input = self.history[current + 1].clone();
                self.cursor = self.input.chars().count();
            }
            Some(_) => {
                self.history_cursor = None;
                self.input.clear();
                self.cursor = 0;
            }
            None => {}
        }
    }

    // --- streaming ---

    /// Records that a turn started.
    pub fn begin_run(&mut self, run_id: RunId, agent: &str, model: Option<&str>, resumed: bool) {
        self.begin_run_with_reason(run_id, agent, model, resumed, None);
    }

    /// Records a turn start and visibly explains any canonical context transfer.
    pub fn begin_run_with_reason(
        &mut self,
        run_id: RunId,
        agent: &str,
        model: Option<&str>,
        resumed: bool,
        context_transfer_reason: Option<&str>,
    ) {
        self.active_run = Some(run_id);
        self.streaming.clear();
        self.thinking_streaming.clear();
        self.active_tools.clear();
        self.scroll_back = 0;
        let model = model.unwrap_or("default");
        let mode = if resumed {
            "resumed session"
        } else {
            "fresh session with transferred context"
        };
        self.push(LineKind::AgentHeader, format!("{agent} · {model} · {mode}"));
        if let Some(reason) = context_transfer_reason {
            self.push(
                LineKind::Notice,
                format!("↻ context transferred to a fresh session — {reason}"),
            );
        }
        self.activity = Activity::Starting;
        self.activity_detail = None;
        self.tick = 0;
        self.set_status(format!("{agent} · Esc to cancel"));
    }

    /// Folds one streamed event into the transcript.
    pub fn apply_event(&mut self, kind: RunEventKind) {
        match kind {
            RunEventKind::TextDelta { text } => {
                if self.activity != Activity::Responding {
                    self.streaming.clear();
                }
                self.thinking_streaming.clear();
                self.activity = Activity::Responding;
                self.activity_detail = None;
                self.streaming.push_str(&text);
                self.rewrite_streaming_line();
            }
            RunEventKind::ThinkingDelta { text } => {
                if self.activity != Activity::Thinking {
                    self.thinking_streaming.clear();
                }
                self.streaming.clear();
                self.activity = Activity::Thinking;
                self.activity_detail = None;
                self.thinking_streaming.push_str(&text);
                self.rewrite_thinking_line();
            }
            RunEventKind::ToolStarted { id, name, input } => {
                self.activity = Activity::Working;
                self.activity_detail = Some(format!("running {name}"));
                self.active_tools.insert(id, name.clone());
                let detail = input
                    .as_deref()
                    .map(compact_activity)
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" — {s}"))
                    .unwrap_or_default();
                self.push(LineKind::Activity, format!("↳ calling {name}{detail}"));
                // Output after a tool call belongs to a new block.
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::ToolCompleted { id, output, ok } => {
                let name = self
                    .active_tools
                    .remove(&id)
                    .unwrap_or_else(|| "tool".to_string());
                self.activity_detail = Some(
                    self.active_tools
                        .values()
                        .next()
                        .map(|name| format!("running {name}"))
                        .unwrap_or_else(|| "processing tool result".into()),
                );
                let mark = if ok { "✓" } else { "✗" };
                let detail = output
                    .as_deref()
                    .map(compact_activity)
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" — {s}"))
                    .unwrap_or_default();
                self.push(LineKind::Activity, format!("{mark} {name}{detail}"));
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::FileWritten { path } => {
                self.push(LineKind::Activity, format!("✎ wrote {path}"));
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::ChildSpawned {
                child_run_id,
                child_agent_id,
                task,
            } => {
                self.activity = Activity::Working;
                self.activity_detail = Some(format!("subagent {child_agent_id}"));
                self.push(
                    LineKind::Activity,
                    format!(
                        "↳ subagent {} ({}) — {}",
                        child_agent_id,
                        child_run_id,
                        compact_activity(&task)
                    ),
                );
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::ChildCompleted {
                child_run_id,
                status,
            } => {
                self.push(
                    LineKind::Activity,
                    format!("✓ subagent {child_run_id} — {status:?}"),
                );
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::PlanUpdated { steps } => {
                self.activity = Activity::Working;
                self.activity_detail = Some("updating plan".into());
                self.push(LineKind::Activity, "· plan updated");
                for step in steps {
                    self.push(LineKind::Activity, format!("  {step}"));
                }
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::SessionReseeded { reason } => {
                self.push(
                    LineKind::Notice,
                    format!("· {reason}; retrying with full context"),
                );
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::Diagnostic { code, detail } => {
                // Most diagnostics are noise for a chat view; surface only the ones
                // that explain something the user can act on.
                if code == "PERMISSION_AUTO_APPROVED"
                    || code == "RUN_INTERRUPTED"
                    || code == "TRANSIENT_RETRY"
                    || code == "ACP_METHOD_UNSUPPORTED"
                    || code == "ACP_UPDATE"
                    || code == "THINKING_UNAVAILABLE"
                {
                    self.push(LineKind::Notice, format!("· {detail}"));
                    self.streaming.clear();
                    self.thinking_streaming.clear();
                }
            }
            RunEventKind::Error {
                message, retryable, ..
            } => {
                self.active_error_retryable |= retryable;
                self.push(LineKind::Error, format!("! {message}"));
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::RunFinished { status, usage } => {
                let can_retry = status == RunStatus::Failed && self.active_error_retryable;
                if can_retry {
                    self.retry_prompt = self.active_prompt.take();
                } else {
                    self.active_prompt = None;
                }
                self.active_error_retryable = false;
                self.active_run = None;
                self.activity = Activity::Idle;
                self.activity_detail = None;
                self.streaming.clear();
                self.thinking_streaming.clear();
                self.active_tools.clear();
                self.last_usage_source = self
                    .lines
                    .iter()
                    .rev()
                    .find(|line| line.kind == LineKind::AgentHeader)
                    .map(|line| line.text.split(" · ").take(2).collect::<Vec<_>>().join("/"));
                self.last_usage = (!usage.is_empty()).then_some(usage);
                self.recompute_context();
                let mut note = match status {
                    RunStatus::Succeeded => "done".to_string(),
                    RunStatus::Cancelled => "cancelled".to_string(),
                    RunStatus::Failed if can_retry => {
                        "retryable failure · Enter retries · Esc discards".to_string()
                    }
                    _ => "the turn did not complete".to_string(),
                };
                if let (Some(input), Some(output)) = (usage.input, usage.output) {
                    note.push_str(&format!(" · {input} in / {output} out"));
                }
                self.set_status(note);
            }
            _ => {}
        }
    }

    /// Replaces the trailing reasoning line with the accumulated text.
    fn rewrite_thinking_line(&mut self) {
        let text = self.thinking_streaming.trim_end_matches('\n').to_string();
        match self.lines.last_mut() {
            Some(line) if line.kind == LineKind::Thinking => line.text = text,
            _ => self.push(LineKind::Thinking, text),
        }
    }

    /// Replaces the trailing assistant line with the accumulated text.
    ///
    /// Streaming arrives as fragments; rewriting one line keeps paragraphs intact
    /// instead of emitting a transcript line per token.
    fn rewrite_streaming_line(&mut self) {
        let text = self.streaming.trim_end_matches('\n').to_string();
        match self.lines.last_mut() {
            Some(line) if line.kind == LineKind::Assistant => line.text = text,
            _ => self.push(LineKind::Assistant, text),
        }
    }

    // --- overlays ---

    /// Opens a picker.
    pub fn open_picker(
        &mut self,
        title: impl Into<String>,
        items: Vec<String>,
        values: Vec<String>,
        action: PickerAction,
    ) {
        self.overlay = Overlay::Picker {
            title: title.into(),
            items,
            values,
            selected: 0,
            filter: String::new(),
            action,
        };
    }

    /// Opens a picker when the latest assistant response asks the user to choose.
    pub fn open_latest_response_options(&mut self) -> bool {
        let response = self
            .lines
            .iter()
            .rev()
            .take_while(|line| line.kind != LineKind::User && line.kind != LineKind::AgentHeader)
            .filter(|line| line.kind == LineKind::Assistant)
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let Some(values) = response_options(&response) else {
            return false;
        };
        self.open_picker(
            "choose a response",
            values.clone(),
            values,
            PickerAction::ResponseOption,
        );
        true
    }

    /// Indices of picker items matching the current filter.
    ///
    /// Matching is a case-insensitive substring test on the rendered label, which
    /// is what a user typing "sonnet" or "nova" expects.
    pub fn picker_matches(&self) -> Vec<usize> {
        match &self.overlay {
            Overlay::Picker { items, filter, .. } => {
                if filter.is_empty() {
                    return (0..items.len()).collect();
                }
                let needle = filter.to_ascii_lowercase();
                items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.to_ascii_lowercase().contains(&needle))
                    .map(|(index, _)| index)
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Appends to the picker filter and resets the highlight.
    pub fn picker_filter_push(&mut self, ch: char) {
        if let Overlay::Picker {
            filter, selected, ..
        } = &mut self.overlay
        {
            filter.push(ch);
            *selected = 0;
        }
    }

    /// Removes the last filter character.
    pub fn picker_filter_pop(&mut self) {
        if let Overlay::Picker {
            filter, selected, ..
        } = &mut self.overlay
        {
            filter.pop();
            *selected = 0;
        }
    }

    /// Opens a scrollable text pane.
    pub fn open_text(&mut self, title: impl Into<String>, lines: Vec<String>) {
        self.overlay = Overlay::Text {
            title: title.into(),
            lines,
            scroll: 0,
        };
    }

    /// Closes any overlay.
    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    /// True when an overlay is showing.
    pub fn has_overlay(&self) -> bool {
        !matches!(self.overlay, Overlay::None)
    }

    /// Moves the overlay selection or scroll.
    pub fn overlay_move(&mut self, delta: i32) {
        match &mut self.overlay {
            Overlay::Picker { .. } => {
                // Movement is over the filtered view, not the full item list.
                let count = self.picker_matches().len();
                if count == 0 {
                    return;
                }
                let last = count - 1;
                if let Overlay::Picker { selected, .. } = &mut self.overlay {
                    *selected = if delta < 0 {
                        selected.saturating_sub((-delta) as usize)
                    } else {
                        (*selected + delta as usize).min(last)
                    };
                }
            }
            Overlay::Text { lines, scroll, .. } => {
                let last = lines.len().saturating_sub(1);
                *scroll = if delta < 0 {
                    scroll.saturating_sub((-delta) as usize)
                } else {
                    (*scroll + delta as usize).min(last)
                };
            }
            Overlay::None => {}
        }
    }

    /// Returns the chosen value and action, closing the picker.
    pub fn overlay_choose(&mut self) -> Option<(PickerAction, String)> {
        let matches = self.picker_matches();
        let chosen = match &self.overlay {
            Overlay::Picker {
                values,
                selected,
                action,
                ..
            } => matches
                .get(*selected)
                .and_then(|index| values.get(*index))
                .map(|value| (*action, value.clone())),
            _ => None,
        };
        if chosen.is_some() {
            self.close_overlay();
        }
        chosen
    }

    // --- transcript scrolling ---

    /// Updates the viewport's width-aware rendered-row scroll limit.
    pub(crate) fn set_scroll_limit(&self, limit: usize) {
        self.scroll_limit.set(limit);
    }

    /// Scrolls the transcript back by `amount` rendered rows.
    pub fn scroll_up(&mut self, amount: usize) {
        let limit = self.scroll_limit.get();
        self.scroll_back = self
            .scroll_back
            .min(limit)
            .saturating_add(amount)
            .min(limit);
    }

    /// Scrolls the transcript forward by `amount` rendered rows.
    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll_back = self
            .scroll_back
            .min(self.scroll_limit.get())
            .saturating_sub(amount);
    }

    /// Re-estimates how much context this conversation would replay.
    ///
    /// A byte-based estimate rather than a real tokenizer: Argo cannot tokenize for
    /// five different vendors, and the number's job is to warn before a switch
    /// becomes expensive, not to bill anyone.
    pub fn recompute_context(&mut self) {
        const BYTES_PER_TOKEN: usize = 3;
        let bytes: usize = self.lines.iter().map(|line| line.text.len()).sum();
        self.context_tokens = bytes / BYTES_PER_TOKEN;
    }

    /// Human-readable context usage, with a share of the window when known.
    pub fn context_label(&self) -> String {
        let tokens = self.context_tokens;
        let rendered = if tokens >= 1_000 {
            format!("{:.1}k", tokens as f64 / 1_000.0)
        } else {
            tokens.to_string()
        };
        match self.model_window() {
            Some(window) if window > 0 => {
                let percent = ((tokens as f64 / window as f64) * 100.0).round() as usize;
                format!("ctx ~{rendered}/{}k · {percent}%", window / 1_000)
            }
            _ => format!("ctx ~{rendered}"),
        }
    }

    /// Context window for the selected model, when Argo knows it.
    ///
    /// Only reported for models whose window is documented; guessing would make the
    /// percentage misleading.
    fn model_window(&self) -> Option<usize> {
        let model = self
            .conversation
            .as_ref()
            .and_then(|c| c.selected_model.as_deref())?;
        let lower = model.to_ascii_lowercase();
        if lower.contains("gpt-5") {
            return Some(272_000);
        }
        if lower.contains("sonnet") || lower.contains("opus") || lower.contains("haiku") {
            return Some(200_000);
        }
        None
    }

    /// Execution mode in effect for the next turn.
    pub fn mode(&self) -> argo_core::mode::AgentMode {
        self.conversation
            .as_ref()
            .and_then(|c| c.selected_mode.as_deref())
            .and_then(argo_core::mode::AgentMode::parse)
            .unwrap_or_default()
    }

    /// Modes the selected adapter can actually enforce.
    pub fn mode_support(&self) -> argo_core::mode::ModeSupport {
        self.conversation
            .as_ref()
            .and_then(|c| c.selected_agent_id.as_deref())
            .and_then(argo_runtime::find)
            .map(|def| def.capabilities.modes)
            .unwrap_or(argo_core::mode::ModeSupport::NONE)
    }

    /// Next mode the selected adapter supports.
    pub fn next_mode(&self) -> argo_core::mode::AgentMode {
        self.mode_support().next_supported(self.mode())
    }

    /// Reasoning effort in effect, when one is selected.
    pub fn effort_label(&self) -> Option<String> {
        self.conversation
            .as_ref()
            .and_then(|c| c.selected_reasoning.clone())
    }

    /// Selection summary for the status bar.
    pub fn selection_label(&self) -> String {
        let Some(conversation) = &self.conversation else {
            return "no conversation".to_string();
        };
        let agent = conversation
            .selected_agent_id
            .clone()
            .unwrap_or_else(|| "auto".into());
        let model = conversation
            .selected_model
            .clone()
            .unwrap_or_else(|| "default".into());
        format!("{agent}/{model}")
    }

    /// Exact last-turn token accounting reported by the selected CLI stream.
    pub fn usage_report(&self) -> Vec<String> {
        let source = self
            .last_usage_source
            .as_deref()
            .unwrap_or("no completed turn");
        let mut lines = vec![format!("Last completed turn: {source}")];
        match self.last_usage {
            Some(usage) => {
                lines.push(format!("input:       {}", token_count(usage.input)));
                lines.push(format!("output:      {}", token_count(usage.output)));
                lines.push(format!("cached input: {}", token_count(usage.cached_input)));
                lines.push(format!("reasoning:   {}", token_count(usage.reasoning)));
                lines.push(String::new());
                lines.push("Exact values reported by that CLI's turn stream.".into());
                lines.push(
                    "An unavailable field was not reported; Argo does not estimate it.".into(),
                );
            }
            None => {
                lines.push("No exact token counts were reported for that turn.".into());
                lines.push(
                    "Plain-output adapters such as Command Code and Grok cannot expose token usage."
                        .into(),
                );
            }
        }
        lines.push(String::new());
        lines.push("Account quota, credits, and billing are unavailable: installed CLIs do not expose them non-interactively.".into());
        lines.push(
            "OpenCode users can also run `opencode stats` for its CLI-local aggregate.".into(),
        );
        lines
    }

    /// Current Argo session state; this deliberately avoids invented provider
    /// billing or quota information.
    pub fn status_report(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(conversation) = &self.conversation {
            let title = conversation
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or("(untitled)");
            lines.push(format!("Conversation: {title} ({})", conversation.id));
        } else {
            lines.push("Conversation: none".into());
        }
        lines.push(format!("Selection: {}", self.selection_label()));
        lines.push(format!("Mode: {}", self.mode().label()));
        lines.push(format!("Context: {} (estimated)", self.context_label()));
        let state = if self.is_busy() {
            self.activity.label()
        } else if self.retry_prompt.is_some() {
            "paused — Enter retries"
        } else {
            "idle"
        };
        lines.push(format!("Run state: {state}"));
        lines.push(format!("Queued follow-ups: {}", self.queue_depth()));
        if let Some(source) = &self.last_usage_source {
            lines.push(format!(
                "Last usage source: {source} ({})",
                if self.last_usage.is_some() {
                    "exact stream values available"
                } else {
                    "CLI reported no token counts"
                }
            ));
        }
        lines.push(String::new());
        lines.push(
            "Provider account quota/status is not exposed by the installed CLI interfaces.".into(),
        );
        lines
    }

    /// Rebuilds canonical history using the same visual vocabulary as live events.
    pub fn replace_transcript(&mut self, messages: Vec<MessageView>) {
        self.lines.clear();
        for message in messages {
            match message.role.as_str() {
                "user" => {
                    if message.blocks.is_empty() {
                        self.push(LineKind::User, message.text);
                    } else {
                        for block in message.blocks {
                            if let ContentBlock::Text { text } = block {
                                self.push(LineKind::User, text);
                            }
                        }
                    }
                }
                "assistant" => {
                    if let Some(agent) = &message.agent_id {
                        let model = message.model.as_deref().unwrap_or("default");
                        self.push(LineKind::AgentHeader, format!("{agent} · {model}"));
                    }
                    if message.blocks.is_empty() {
                        self.push(LineKind::Assistant, message.text);
                        continue;
                    }
                    for block in message.blocks {
                        match block {
                            ContentBlock::Text { text } => {
                                if !text.trim().is_empty() {
                                    self.push(LineKind::Assistant, text);
                                }
                            }
                            ContentBlock::Thinking { text } => {
                                if !text.trim().is_empty() {
                                    self.push(LineKind::Thinking, text);
                                }
                            }
                            ContentBlock::Tool { call } => {
                                let input = call
                                    .input
                                    .as_deref()
                                    .map(compact_activity)
                                    .filter(|value| !value.is_empty())
                                    .map(|value| format!(" — {value}"))
                                    .unwrap_or_default();
                                self.push(
                                    LineKind::Activity,
                                    format!("↳ calling {}{input}", call.name),
                                );
                                if call.status != ToolStatus::Pending {
                                    let mark = if call.status == ToolStatus::Completed {
                                        "✓"
                                    } else {
                                        "✗"
                                    };
                                    let output = call
                                        .output
                                        .as_deref()
                                        .map(compact_activity)
                                        .filter(|value| !value.is_empty())
                                        .map(|value| format!(" — {value}"))
                                        .unwrap_or_default();
                                    self.push(
                                        LineKind::Activity,
                                        format!("{mark} {}{output}", call.name),
                                    );
                                }
                            }
                            ContentBlock::FileWrite { path } => {
                                self.push(LineKind::Activity, format!("✎ wrote {path}"));
                            }
                        }
                    }
                }
                _ => {
                    if message.blocks.is_empty() {
                        self.push(LineKind::Notice, message.text);
                    } else {
                        for block in message.blocks {
                            if let ContentBlock::Text { text } = block {
                                self.push(LineKind::Notice, text);
                            }
                        }
                    }
                }
            }
        }
        self.scroll_back = 0;
        self.recompute_context();
    }

    /// Replaces active metadata and its cached history-list entry atomically.
    ///
    /// Every view must observe one summary: otherwise the header can show the new
    /// title while `/resume` still describes the same conversation as untitled.
    pub fn set_conversation_summary(&mut self, summary: ConversationSummary) {
        let id = summary.id.clone();
        self.conversation = Some(summary.clone());
        if let Some(existing) = self
            .conversations
            .iter_mut()
            .find(|existing| existing.id == id)
        {
            *existing = summary;
        } else {
            self.conversations.push(summary);
        }
        self.conversations
            .sort_by_key(|summary| std::cmp::Reverse(summary.updated_at));
    }

    /// Replaces the history cache and refreshes active metadata from the same list.
    pub fn set_conversation_summaries(&mut self, conversations: Vec<ConversationSummary>) {
        if let Some(active_id) = self.conversation.as_ref().map(|summary| summary.id.clone()) {
            if let Some(authoritative) = conversations
                .iter()
                .find(|summary| summary.id == active_id)
                .cloned()
            {
                self.conversation = Some(authoritative);
            }
        }
        self.conversations = conversations;
    }

    /// A one-line description of a conversation, for the history list.
    ///
    /// Argo has no titles until a turn happens, so the description falls back to
    /// which agents participated and how much was said — enough to recognize a
    /// session without opening it.
    pub fn describe(summary: &ConversationSummary) -> String {
        let title = summary
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "(untitled)".to_string());
        let agents = if summary.agents_with_sessions.is_empty() {
            "no agent yet".to_string()
        } else {
            summary.agents_with_sessions.join("+")
        };
        format!(
            "{title}  ·  {} msg  ·  {agents}  ·  {}",
            summary.message_count,
            relative_time(summary.updated_at)
        )
    }

    /// Conversation ids in list order, for `/open <n>`.
    pub fn conversation_at(&self, index: usize) -> Option<ConversationId> {
        self.conversations.get(index).map(|c| c.id.clone())
    }
}

fn token_count(value: Option<u64>) -> String {
    let Some(value) = value else {
        return "unavailable".into();
    };
    let digits = value.to_string();
    let mut rendered = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            rendered.push(',');
        }
        rendered.push(ch);
    }
    rendered
}

/// Makes tool arguments/results readable in one bounded transcript row.
///
/// Event parsers already cap payload size; this tighter UI cap avoids a large JSON
/// result pushing the actual assistant answer out of view.
fn compact_activity(text: &str) -> String {
    const LIMIT: usize = 220;
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= LIMIT {
        return normalized;
    }
    let head: String = normalized.chars().take(LIMIT).collect();
    format!("{head}…")
}

/// Renders a timestamp as a short relative age.
fn relative_time(at: i64) -> String {
    let now = argo_core::now_millis();
    let delta = (now - at).max(0) / 1_000;
    match delta {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", delta / 60),
        3_600..=86_399 => format!("{}h ago", delta / 3_600),
        _ => format!("{}d ago", delta / 86_400),
    }
}

/// Extracts a deliberate numbered choice request from an assistant response.
///
/// Numbered prose alone is not enough: a nearby choice phrase is required so
/// ordinary explanations and plans do not unexpectedly become interactive.
fn response_options(response: &str) -> Option<Vec<String>> {
    let lower = response.to_ascii_lowercase();
    let asks_for_choice = [
        "which do you want",
        "which option",
        "choose",
        "select",
        "pick one",
        "what would you like",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));
    if !asks_for_choice {
        return None;
    }

    let mut options = Vec::new();
    let mut expected: Option<usize> = None;
    for line in response.lines() {
        let trimmed = line.trim();
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            continue;
        }
        let number = trimmed[..digits].parse::<usize>().ok()?;
        let rest = trimmed[digits..].strip_prefix(['.', ')'])?.trim();
        if rest.is_empty() {
            continue;
        }
        match expected {
            None => expected = Some(number + 1),
            Some(next) if number == next => expected = Some(number + 1),
            Some(_) => {
                options.clear();
                expected = Some(number + 1);
            }
        }
        options.push(format!("{number}. {rest}"));
    }
    (options.len() >= 2).then_some(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    use argo_core::event::TokenUsage;

    fn new_app() -> App {
        App::new("/repo")
    }

    #[test]
    fn composer_edits_by_character_not_byte() {
        // Multi-byte input would panic on byte indexing.
        let mut app = new_app();
        for ch in "héllo".chars() {
            app.insert(ch);
        }
        assert_eq!(app.input, "héllo");
        app.move_left();
        app.backspace();
        assert_eq!(app.input, "hélo");
        app.move_home();
        app.delete();
        assert_eq!(app.input, "élo");
    }

    #[test]
    fn caret_movement_is_clamped() {
        let mut app = new_app();
        app.insert('a');
        app.move_right();
        app.move_right();
        assert_eq!(app.cursor, 1);
        app.move_left();
        app.move_left();
        assert_eq!(app.cursor, 0);
        app.backspace();
        assert_eq!(app.input, "a");
    }

    #[test]
    fn taking_input_clears_the_composer_and_records_history() {
        let mut app = new_app();
        for ch in "hello".chars() {
            app.insert(ch);
        }
        assert_eq!(app.take_input(), "hello");
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);

        app.history_previous();
        assert_eq!(app.input, "hello");
    }

    #[test]
    fn blank_input_is_not_recorded_in_history() {
        let mut app = new_app();
        app.insert(' ');
        app.take_input();
        app.history_previous();
        assert!(app.input.is_empty());
    }

    #[test]
    fn history_walks_back_and_forward_to_empty() {
        let mut app = new_app();
        for text in ["first", "second"] {
            for ch in text.chars() {
                app.insert(ch);
            }
            app.take_input();
        }
        app.history_previous();
        assert_eq!(app.input, "second");
        app.history_previous();
        assert_eq!(app.input, "first");
        app.history_previous();
        assert_eq!(app.input, "first", "must not run off the start");
        app.history_next();
        assert_eq!(app.input, "second");
        app.history_next();
        assert!(app.input.is_empty(), "forward past the end clears");
    }

    #[test]
    fn streaming_text_accumulates_into_one_line() {
        // Emitting a line per fragment would shred paragraphs.
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "claude", Some("haiku"), false);
        for fragment in ["Hello", ", ", "world"] {
            app.apply_event(RunEventKind::TextDelta {
                text: fragment.into(),
            });
        }
        let assistant: Vec<&Line> = app
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Assistant)
            .collect();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0].text, "Hello, world");
    }

    #[test]
    fn a_tool_call_starts_a_new_paragraph() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "codex", None, false);
        app.apply_event(RunEventKind::TextDelta {
            text: "before".into(),
        });
        app.apply_event(RunEventKind::ToolStarted {
            id: "t1".into(),
            name: "shell".into(),
            input: None,
        });
        app.apply_event(RunEventKind::TextDelta {
            text: "after".into(),
        });
        let assistant: Vec<&str> = app
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Assistant)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(assistant, vec!["before", "after"]);
        assert!(app.lines.iter().any(|l| l.text.contains("shell")));
    }

    #[test]
    fn emitted_thinking_is_visible_and_accumulates_without_fragmentation() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "claude", None, false);
        for fragment in ["checking ", "the files"] {
            app.apply_event(RunEventKind::ThinkingDelta {
                text: fragment.into(),
            });
        }
        let thinking: Vec<&Line> = app
            .lines
            .iter()
            .filter(|line| line.kind == LineKind::Thinking)
            .collect();
        assert_eq!(thinking.len(), 1);
        assert_eq!(thinking[0].text, "checking the files");
    }

    #[test]
    fn interleaved_thinking_tools_and_responses_keep_stream_order_without_duplication() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "antigravity", None, false);
        for event in [
            RunEventKind::ThinkingDelta {
                text: "first thought".into(),
            },
            RunEventKind::TextDelta {
                text: "first answer".into(),
            },
            RunEventKind::ThinkingDelta {
                text: "second thought".into(),
            },
            RunEventKind::ToolStarted {
                id: "t1".into(),
                name: "search".into(),
                input: None,
            },
            RunEventKind::ToolCompleted {
                id: "t1".into(),
                output: Some("found".into()),
                ok: true,
            },
            RunEventKind::ThinkingDelta {
                text: "third thought".into(),
            },
            RunEventKind::TextDelta {
                text: "final answer".into(),
            },
        ] {
            app.apply_event(event);
        }

        let flow = app
            .lines
            .iter()
            .filter(|line| {
                matches!(
                    line.kind,
                    LineKind::Thinking | LineKind::Assistant | LineKind::Activity
                )
            })
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            flow,
            vec![
                "first thought",
                "first answer",
                "second thought",
                "↳ calling search",
                "✓ search — found",
                "third thought",
                "final answer",
            ]
        );
    }

    #[test]
    fn tool_start_completion_and_file_write_are_all_visible() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "claude", None, false);
        app.apply_event(RunEventKind::ToolStarted {
            id: "tool-1".into(),
            name: "shell".into(),
            input: Some("{\"command\":\"cargo test\"}".into()),
        });
        app.apply_event(RunEventKind::ToolCompleted {
            id: "tool-1".into(),
            output: Some("all tests passed".into()),
            ok: true,
        });
        app.apply_event(RunEventKind::FileWritten {
            path: "src/main.rs".into(),
        });

        let activity = app
            .lines
            .iter()
            .filter(|line| line.kind == LineKind::Activity)
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(activity.contains("calling shell"), "{activity}");
        assert!(activity.contains("cargo test"), "{activity}");
        assert!(activity.contains("✓ shell"), "{activity}");
        assert!(activity.contains("all tests passed"), "{activity}");
        assert!(activity.contains("wrote src/main.rs"), "{activity}");
    }

    #[test]
    fn the_header_states_whether_context_was_transferred() {
        // This is the fact the user most needs when switching agents.
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "codex", Some("default"), false);
        assert!(app.lines[0]
            .text
            .contains("fresh session with transferred context"));

        let mut app = new_app();
        app.begin_run(RunId::new("r2"), "claude", Some("haiku"), true);
        assert!(app.lines[0].text.contains("resumed session"));
    }

    #[test]
    fn a_context_transfer_reason_is_a_visible_transcript_alert() {
        let mut app = new_app();
        app.begin_run_with_reason(
            RunId::new("r1"),
            "antigravity",
            Some("claude-sonnet-4-6"),
            false,
            Some("model changed since this session was created"),
        );
        let notice = app
            .lines
            .iter()
            .find(|line| line.kind == LineKind::Notice)
            .expect("context transfer notice");
        assert!(notice
            .text
            .contains("context transferred to a fresh session"));
        assert!(notice.text.contains("model changed"));
    }

    #[test]
    fn the_indicator_reflects_what_the_agent_is_doing() {
        // Derived from events rather than merely spinning, so it is informative.
        let mut app = new_app();
        assert!(app.activity_indicator().is_none());

        app.begin_run(RunId::new("r1"), "claude", None, false);
        assert_eq!(app.activity, Activity::Starting);
        assert!(app
            .activity_indicator()
            .expect("indicator")
            .contains("starting"));

        app.apply_event(RunEventKind::ThinkingDelta { text: "hmm".into() });
        assert_eq!(app.activity, Activity::Thinking);

        app.apply_event(RunEventKind::TextDelta { text: "hi".into() });
        assert_eq!(app.activity, Activity::Responding);

        app.apply_event(RunEventKind::ToolStarted {
            id: "t1".into(),
            name: "shell".into(),
            input: None,
        });
        assert_eq!(app.activity, Activity::Working);

        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Succeeded,
            usage: TokenUsage::default(),
        });
        assert_eq!(app.activity, Activity::Idle);
        assert!(app.activity_indicator().is_none());
    }

    #[test]
    fn the_farewell_names_the_session_and_how_to_return() {
        let mut app = new_app();
        let mut convo = summary(Some("work"), 2, &["claude"], None);
        convo.id = argo_core::ids::ConversationId::new("abcd1234-5678-90ab-cdef-1234567890ab");
        app.conversation = Some(convo);
        // Nothing was said yet, so there is nothing worth resuming.
        assert!(farewell(&app).is_none());

        app.push(LineKind::User, "hello".to_string());
        let message = farewell(&app).expect("farewell");
        assert!(message.contains("abcd1234"), "short id: {message}");
        assert!(
            message.contains("/resume abcd1234"),
            "resume hint: {message}"
        );
        assert!(
            message.contains("abcd1234-5678-90ab-cdef-1234567890ab"),
            "full id for scripting: {message}"
        );
        assert!(message.contains("argo show"), "transcript hint: {message}");
    }

    #[test]
    fn a_farewell_needs_a_conversation() {
        let app = new_app();
        assert!(farewell(&app).is_none());
    }

    #[test]
    fn messages_typed_during_a_turn_queue_in_order() {
        let mut app = new_app();
        assert_eq!(app.queue_depth(), 0);
        assert_eq!(app.enqueue("first".into()), 1);
        assert_eq!(app.enqueue("second".into()), 2);
        // FIFO: the order they were typed is the order they are sent.
        assert_eq!(app.queued_front(), Some("first"));
        assert_eq!(app.commit_queued().as_deref(), Some("first"));
        assert_eq!(app.queued_front(), Some("second"));
        assert_eq!(app.dequeue().as_deref(), Some("second"));
        assert_eq!(app.dequeue(), None);
    }

    #[test]
    fn the_queue_can_be_discarded_wholesale() {
        let mut app = new_app();
        app.enqueue("a".into());
        app.enqueue("b".into());
        assert_eq!(app.clear_queue(), 2);
        assert_eq!(app.queue_depth(), 0);
        assert_eq!(app.clear_queue(), 0);
    }

    #[test]
    fn the_indicator_reports_how_many_messages_are_waiting() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "claude", None, false);
        assert!(!app
            .activity_indicator()
            .expect("indicator")
            .contains("queued"));

        app.enqueue("next".into());
        let one = app.activity_indicator().expect("indicator");
        assert!(one.contains("1 queued"), "{one}");

        app.enqueue("another".into());
        let two = app.activity_indicator().expect("indicator");
        assert!(two.contains("2 queued"), "{two}");
    }

    #[test]
    fn the_spinner_cycles_and_never_panics_on_overflow() {
        let mut app = new_app();
        let first = app.spinner();
        app.advance_tick();
        assert_ne!(first, app.spinner());
        // Wrapping arithmetic, so a long-running turn cannot overflow.
        app.tick = usize::MAX;
        app.advance_tick();
        assert_eq!(app.tick, 0);
    }

    #[test]
    fn finishing_a_run_clears_busy_and_reports_usage() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "claude", None, false);
        assert!(app.is_busy());
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Succeeded,
            usage: TokenUsage {
                input: Some(10),
                output: Some(3),
                ..Default::default()
            },
        });
        assert!(!app.is_busy());
        assert!(app.status.contains("done"));
        assert!(app.status.contains("10 in / 3 out"));
    }

    #[test]
    fn a_reseed_is_surfaced_as_a_notice_not_an_error() {
        // The turn still succeeds, so it must not look like a failure.
        let mut app = new_app();
        app.apply_event(RunEventKind::SessionReseeded {
            reason: "the agent's saved session no longer exists".into(),
        });
        let line = app.lines.last().expect("line");
        assert_eq!(line.kind, LineKind::Notice);
        assert!(line.text.contains("retrying with full context"));
    }

    #[test]
    fn noisy_diagnostics_are_filtered_but_agent_updates_remain_visible() {
        let mut app = new_app();
        app.apply_event(RunEventKind::Diagnostic {
            code: "UNPARSEABLE_LINE".into(),
            detail: "terminal banner noise".into(),
        });
        assert!(app.lines.is_empty());

        app.apply_event(RunEventKind::Diagnostic {
            code: "ACP_UPDATE".into(),
            detail: "agent_progress_message: still checking".into(),
        });
        assert_eq!(app.lines.len(), 1);
        assert!(app.lines[0].text.contains("still checking"));

        app.apply_event(RunEventKind::Diagnostic {
            code: "PERMISSION_AUTO_APPROVED".into(),
            detail: "auto-approved a write".into(),
        });
        assert_eq!(app.lines.len(), 2);
    }

    #[test]
    fn picker_selection_is_clamped_and_returns_its_value() {
        let mut app = new_app();
        app.open_picker(
            "Agent",
            vec!["claude".into(), "codex".into()],
            vec!["claude".into(), "codex".into()],
            PickerAction::Agent,
        );
        assert!(app.has_overlay());
        app.overlay_move(-5);
        app.overlay_move(1);
        app.overlay_move(9);
        let (action, value) = app.overlay_choose().expect("choice");
        assert_eq!(action, PickerAction::Agent);
        assert_eq!(value, "codex");
        // Choosing dismisses the overlay.
        assert!(!app.has_overlay());
    }

    #[test]
    fn deliberate_numbered_options_open_a_response_picker() {
        let mut app = new_app();
        app.push(
            LineKind::AgentHeader,
            "cmd · MiniMaxAI/MiniMax-M3 · resumed session",
        );
        app.push(
            LineKind::Assistant,
            "A few options — which do you want?\n\n1. **Invoke it yourself**\n2. **Re-authenticate** the MCP server\n3. Use curl directly",
        );
        assert!(app.open_latest_response_options());
        let Overlay::Picker { items, action, .. } = &app.overlay else {
            panic!("expected picker");
        };
        assert_eq!(*action, PickerAction::ResponseOption);
        assert_eq!(items.len(), 3);
        assert_eq!(items[1], "2. **Re-authenticate** the MCP server");

        app.overlay_move(1);
        let (action, value) = app.overlay_choose().expect("selected option");
        assert_eq!(action, PickerAction::ResponseOption);
        assert_eq!(value, "2. **Re-authenticate** the MCP server");
    }

    #[test]
    fn ordinary_numbered_explanations_do_not_open_a_picker() {
        let mut app = new_app();
        app.push(
            LineKind::Assistant,
            "The implementation has three steps:\n1. Parse input\n2. Store it\n3. Render output",
        );
        assert!(!app.open_latest_response_options());
        assert!(!app.has_overlay());
    }

    #[test]
    fn suggestions_appear_as_you_type_a_command() {
        // The original build only offered completions on Tab, which made the
        // command surface invisible.
        let mut app = new_app();
        app.insert('/');
        assert!(!app.completions.is_empty());
        app.insert('m');
        assert_eq!(app.completions, vec!["/mcp", "/mode", "/model"]);
        app.insert('o');
        assert_eq!(app.completions, vec!["/mode", "/model"]);
        // Backspacing widens the suggestions again.
        app.backspace();
        assert_eq!(app.completions, vec!["/mcp", "/mode", "/model"]);
    }

    #[test]
    fn suggestions_are_navigable_and_wrap() {
        // The original build offered no way to move through the list at all.
        let mut app = new_app();
        app.insert('/');
        app.insert('a');
        assert_eq!(app.completions, vec!["/agent", "/agents"]);
        assert_eq!(app.completion_index, 0);

        app.completion_move(1);
        assert_eq!(app.completion_index, 1);
        // Past the end wraps to the top rather than sticking.
        app.completion_move(1);
        assert_eq!(app.completion_index, 0);
        app.completion_move(-1);
        assert_eq!(app.completion_index, 1);
    }

    #[test]
    fn enter_runs_a_command_that_is_already_complete() {
        // Typing `/model` leaves `/model` showing as its own suggestion. Enter must
        // run it, not silently re-complete it — that made Enter look broken.
        let mut app = new_app();
        for ch in "/model".chars() {
            app.insert(ch);
        }
        assert!(app.has_completions());
        assert_eq!(app.enter_action(), EnterAction::Submit);
    }

    #[test]
    fn enter_completes_a_partially_typed_command() {
        let mut app = new_app();
        for ch in "/mod".chars() {
            app.insert(ch);
        }
        assert_eq!(app.enter_action(), EnterAction::AcceptCompletion);
    }

    #[test]
    fn enter_takes_the_highlighted_entry_once_the_list_is_navigated() {
        // `/agent` is valid on its own, but after arrowing to `/agents` the user is
        // clearly choosing from the list.
        let mut app = new_app();
        for ch in "/agent".chars() {
            app.insert(ch);
        }
        assert_eq!(app.enter_action(), EnterAction::Submit);
        app.completion_move(1);
        assert_eq!(app.enter_action(), EnterAction::AcceptCompletion);
    }

    #[test]
    fn enter_submits_ordinary_messages() {
        let mut app = new_app();
        for ch in "explain this repo".chars() {
            app.insert(ch);
        }
        assert_eq!(app.enter_action(), EnterAction::Submit);
    }

    #[test]
    fn enter_submits_an_unknown_command_so_the_error_is_shown() {
        // Better to run it and report "unknown command" than to sit inert.
        let mut app = new_app();
        for ch in "/zzz".chars() {
            app.insert(ch);
        }
        assert!(!app.has_completions());
        assert_eq!(app.enter_action(), EnterAction::Submit);
    }

    #[test]
    fn editing_after_navigating_restores_submit_behaviour() {
        let mut app = new_app();
        for ch in "/agent".chars() {
            app.insert(ch);
        }
        app.completion_move(1);
        assert_eq!(app.enter_action(), EnterAction::AcceptCompletion);
        // Continuing to type means the user is no longer picking from the list.
        app.insert('s');
        assert_eq!(app.enter_action(), EnterAction::Submit);
    }

    #[test]
    fn accepting_takes_the_highlighted_suggestion_not_the_first() {
        let mut app = new_app();
        app.insert('/');
        app.insert('a');
        app.completion_move(1);
        assert!(app.accept_completion());
        assert_eq!(app.input, "/agents ");
    }

    #[test]
    fn editing_resets_the_highlight_to_a_valid_index() {
        let mut app = new_app();
        app.insert('/');
        app.insert('a');
        app.completion_move(1);
        app.insert('g');
        // The list changed, so the old highlight would be meaningless.
        assert_eq!(app.completion_index, 0);
        assert!(app.completion_index < app.completions.len());
    }

    #[test]
    fn the_composer_accepts_line_breaks() {
        let mut app = new_app();
        for ch in "first".chars() {
            app.insert(ch);
        }
        app.insert_newline();
        for ch in "second".chars() {
            app.insert(ch);
        }
        assert_eq!(app.input, "first\nsecond");
        assert_eq!(app.input_line_count(), 2);
    }

    #[test]
    fn the_caret_tracks_rows_and_columns_across_line_breaks() {
        // Dividing the caret index by the width is wrong once a newline exists.
        let mut app = new_app();
        for ch in "ab".chars() {
            app.insert(ch);
        }
        app.insert_newline();
        for ch in "cde".chars() {
            app.insert(ch);
        }
        assert_eq!(app.caret_row_column(), (1, 3));
        app.move_home();
        assert_eq!(app.caret_row_column(), (0, 0));
    }

    #[test]
    fn a_newline_does_not_trigger_a_command_lookup() {
        let mut app = new_app();
        app.insert('/');
        app.insert_newline();
        // The line is no longer a bare command name.
        assert!(app.completions.is_empty());
    }

    #[test]
    fn ordinary_text_produces_no_suggestions() {
        let mut app = new_app();
        for ch in "hello".chars() {
            app.insert(ch);
        }
        assert!(app.completions.is_empty());
    }

    #[test]
    fn accepting_a_suggestion_prefills_the_composer() {
        let mut app = new_app();
        app.insert('/');
        app.insert('d');
        app.insert('e');
        assert!(app.accept_completion());
        assert_eq!(app.input, "/delegate ");
        assert_eq!(app.cursor, app.input.chars().count());
    }

    #[test]
    fn accepting_with_no_suggestion_does_nothing() {
        let mut app = new_app();
        for ch in "plain".chars() {
            app.insert(ch);
        }
        assert!(!app.accept_completion());
        assert_eq!(app.input, "plain");
    }

    #[test]
    fn suggestions_clear_once_the_line_is_submitted() {
        let mut app = new_app();
        app.insert('/');
        assert!(!app.completions.is_empty());
        app.take_input();
        assert!(app.completions.is_empty());
    }

    #[test]
    fn a_long_picker_can_be_narrowed_by_typing() {
        // OpenCode reports hundreds of models; arrow keys alone are unusable.
        let mut app = new_app();
        let items: Vec<String> = vec![
            "anthropic/claude-sonnet-4-5".into(),
            "openai/gpt-5.6".into(),
            "amazon-bedrock/amazon.nova-pro-v1:0".into(),
            "opencode/big-pickle".into(),
        ];
        app.open_picker("model", items.clone(), items, PickerAction::Model);
        assert_eq!(app.picker_matches().len(), 4);

        for ch in "nova".chars() {
            app.picker_filter_push(ch);
        }
        let matches = app.picker_matches();
        assert_eq!(matches.len(), 1);

        // Choosing must return the filtered item, not the first unfiltered one.
        let (action, value) = app.overlay_choose().expect("choice");
        assert_eq!(action, PickerAction::Model);
        assert_eq!(value, "amazon-bedrock/amazon.nova-pro-v1:0");
    }

    #[test]
    fn picker_filtering_is_case_insensitive_and_reversible() {
        let mut app = new_app();
        let items: Vec<String> = vec!["anthropic/claude-SONNET".into(), "openai/gpt".into()];
        app.open_picker("model", items.clone(), items, PickerAction::Model);
        for ch in "sonnet".chars() {
            app.picker_filter_push(ch);
        }
        assert_eq!(app.picker_matches().len(), 1);
        for _ in 0..6 {
            app.picker_filter_pop();
        }
        assert_eq!(app.picker_matches().len(), 2);
    }

    #[test]
    fn a_filter_matching_nothing_cannot_be_chosen_from() {
        let mut app = new_app();
        let items: Vec<String> = vec!["a".into(), "b".into()];
        app.open_picker("model", items.clone(), items, PickerAction::Model);
        for ch in "zzzz".chars() {
            app.picker_filter_push(ch);
        }
        assert!(app.picker_matches().is_empty());
        app.overlay_move(1);
        assert!(app.overlay_choose().is_none());
    }

    #[test]
    fn an_empty_picker_cannot_be_chosen_from() {
        let mut app = new_app();
        app.open_picker("Empty", vec![], vec![], PickerAction::Model);
        app.overlay_move(1);
        assert!(app.overlay_choose().is_none());
    }

    #[test]
    fn a_read_only_pane_has_nothing_to_choose() {
        // The caller closes it instead, so Enter is not a dead key on /help.
        let mut app = new_app();
        app.open_text("commands", vec!["/help".into()]);
        assert!(app.overlay_choose().is_none());
        assert!(
            app.has_overlay(),
            "choose must not close a text pane itself"
        );
        app.close_overlay();
        assert!(!app.has_overlay());
    }

    #[test]
    fn text_overlay_scroll_is_bounded() {
        let mut app = new_app();
        app.open_text("Context", vec!["a".into(), "b".into(), "c".into()]);
        app.overlay_move(100);
        match &app.overlay {
            Overlay::Text { scroll, .. } => assert_eq!(*scroll, 2),
            other => panic!("unexpected overlay: {other:?}"),
        }
        app.overlay_move(-100);
        match &app.overlay {
            Overlay::Text { scroll, .. } => assert_eq!(*scroll, 0),
            other => panic!("unexpected overlay: {other:?}"),
        }
    }

    #[test]
    fn transcript_scrollback_tracks_rendered_rows_without_logical_line_clamping() {
        let mut app = new_app();
        // A single logical response may wrap to far more than five rows. App state
        // records requested row movement; the width-aware renderer clamps it.
        for i in 0..5 {
            app.push(LineKind::Assistant, format!("line {i}"));
        }
        app.set_scroll_limit(100);
        app.scroll_up(100);
        assert_eq!(app.scroll_back, 100);
        app.scroll_down(100);
        assert_eq!(app.scroll_back, 0);
    }

    fn summary(
        title: Option<&str>,
        messages: usize,
        agents: &[&str],
        model: Option<&str>,
    ) -> ConversationSummary {
        ConversationSummary {
            id: argo_core::ids::ConversationId::new("c1"),
            title: title.map(str::to_string),
            selected_agent_id: Some("codex".into()),
            selected_model: model.map(str::to_string),
            selected_reasoning: None,
            selected_mode: None,
            message_count: messages,
            agents_with_sessions: agents.iter().map(|a| a.to_string()).collect(),
            parent_conversation_id: None,
            updated_at: argo_core::now_millis(),
        }
    }

    #[test]
    fn authoritative_summary_updates_header_and_cached_description_together() {
        let mut app = new_app();
        app.set_conversation_summary(summary(None, 0, &[], None));

        let mut updated = summary(Some("fix immediate metadata"), 2, &["codex"], Some("gpt-5"));
        updated.updated_at += 1;
        app.set_conversation_summary(updated);

        assert_eq!(
            app.conversation
                .as_ref()
                .and_then(|item| item.title.as_deref()),
            Some("fix immediate metadata")
        );
        assert_eq!(app.conversations.len(), 1, "the cache must be upserted");
        let description = App::describe(&app.conversations[0]);
        assert!(
            description.contains("fix immediate metadata"),
            "{description}"
        );
        assert!(description.contains("2 msg"), "{description}");
        assert!(description.contains("codex"), "{description}");
    }

    #[test]
    fn resumed_transcript_restores_structured_activity_and_markdown() {
        let mut app = new_app();
        app.replace_transcript(vec![
            MessageView {
                id: "m1".into(),
                role: "user".into(),
                text: "run it".into(),
                blocks: vec![ContentBlock::text("run it")],
                agent_id: None,
                model: None,
                created_at: 1,
            },
            MessageView {
                id: "m2".into(),
                role: "assistant".into(),
                text: "fallback text".into(),
                blocks: vec![
                    ContentBlock::Thinking {
                        text: "checking data".into(),
                    },
                    ContentBlock::Tool {
                        call: argo_core::message::ToolCall {
                            id: "t1".into(),
                            name: "run_backtest".into(),
                            input: Some("{\"symbol\":\"SENSEX\"}".into()),
                            output: Some("{\"runID\":\"backtest-123\"}".into()),
                            status: ToolStatus::Completed,
                        },
                    },
                    ContentBlock::FileWrite {
                        path: "strategy.py".into(),
                    },
                    ContentBlock::text("## Result\n\n[Report](https://example.com/report)"),
                ],
                agent_id: Some("antigravity".into()),
                model: Some("sonnet".into()),
                created_at: 2,
            },
        ]);

        assert_eq!(app.lines[0].kind, LineKind::User);
        assert!(app
            .lines
            .iter()
            .any(|line| line.kind == LineKind::Thinking && line.text == "checking data"));
        let activity = app
            .lines
            .iter()
            .filter(|line| line.kind == LineKind::Activity)
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(activity.contains("calling run_backtest"), "{activity}");
        assert!(activity.contains("backtest-123"), "{activity}");
        assert!(activity.contains("wrote strategy.py"), "{activity}");
        let answer = app
            .lines
            .iter()
            .find(|line| line.kind == LineKind::Assistant)
            .expect("assistant markdown");
        assert!(answer.text.contains("[Report](https://example.com/report)"));
        assert!(
            !answer.text.contains("runID"),
            "tools must not flatten into prose"
        );
    }

    #[test]
    fn a_session_description_identifies_it_without_opening_it() {
        let described = App::describe(&summary(
            Some("add a health endpoint"),
            6,
            &["claude", "codex"],
            None,
        ));
        assert!(described.contains("add a health endpoint"));
        assert!(described.contains("6 msg"));
        assert!(described.contains("claude+codex"));
        assert!(described.contains("just now"));
    }

    #[test]
    fn an_untitled_session_still_describes_itself() {
        let described = App::describe(&summary(None, 0, &[], None));
        assert!(described.contains("(untitled)"));
        assert!(described.contains("no agent yet"));
    }

    #[test]
    fn blank_titles_are_treated_as_missing() {
        assert!(App::describe(&summary(Some("   "), 1, &[], None)).contains("(untitled)"));
    }

    #[test]
    fn relative_time_reads_naturally() {
        let now = argo_core::now_millis();
        assert_eq!(relative_time(now), "just now");
        assert_eq!(relative_time(now - 120_000), "2m ago");
        assert_eq!(relative_time(now - 7_200_000), "2h ago");
        assert_eq!(relative_time(now - 172_800_000), "2d ago");
        // A clock skew into the future must not underflow.
        assert_eq!(relative_time(now + 60_000), "just now");
    }

    #[test]
    fn context_usage_is_estimated_from_the_transcript() {
        let mut app = new_app();
        app.push(LineKind::User, "x".repeat(3_000));
        app.recompute_context();
        assert_eq!(app.context_tokens, 1_000);
        assert!(app.context_label().contains("ctx ~1.0k"));
    }

    #[test]
    fn context_usage_shows_a_share_of_a_known_window() {
        let mut app = new_app();
        app.conversation = Some(summary(None, 0, &[], Some("gpt-5.6-sol")));
        app.push(LineKind::User, "x".repeat(3_000));
        app.recompute_context();
        let label = app.context_label();
        assert!(
            label.contains("272k"),
            "expected the documented window: {label}"
        );
        assert!(label.contains('%'));
    }

    #[test]
    fn an_unknown_model_reports_tokens_without_a_misleading_percentage() {
        let mut app = new_app();
        app.conversation = Some(summary(None, 0, &[], Some("some/unknown-model")));
        app.push(LineKind::User, "x".repeat(300));
        app.recompute_context();
        let label = app.context_label();
        assert!(label.contains("ctx ~100"));
        assert!(!label.contains('%'));
    }

    #[test]
    fn retryable_failure_preserves_partial_response_and_prompt() {
        let mut app = new_app();
        app.track_active_prompt("continue the analysis".into());
        app.begin_run(RunId::new("r1"), "antigravity", None, true);
        app.apply_event(RunEventKind::TextDelta {
            text: "Partial answer before disconnect.".into(),
        });
        app.apply_event(RunEventKind::Error {
            code: "AGENT_ERROR".into(),
            message: "network connection reset".into(),
            retryable: true,
        });
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Failed,
            usage: TokenUsage::default(),
        });

        assert_eq!(app.retry_prompt(), Some("continue the analysis"));
        assert!(app.lines.iter().any(|line| {
            line.kind == LineKind::Assistant
                && line.text.contains("Partial answer before disconnect")
        }));
        assert!(app.lines.iter().any(|line| {
            line.kind == LineKind::Error && line.text.contains("connection reset")
        }));
        assert!(app.status.contains("Enter retries"), "{}", app.status);
    }

    #[test]
    fn non_retryable_failure_does_not_offer_the_prompt_again() {
        let mut app = new_app();
        app.track_active_prompt("use an invalid model".into());
        app.begin_run(RunId::new("r1"), "codex", None, false);
        app.apply_event(RunEventKind::Error {
            code: "AGENT_ERROR".into(),
            message: "invalid model".into(),
            retryable: false,
        });
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Failed,
            usage: TokenUsage::default(),
        });
        assert_eq!(app.retry_prompt(), None);
    }

    #[test]
    fn usage_from_a_finished_turn_is_retained() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "codex", Some("gpt-5"), false);
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Succeeded,
            usage: TokenUsage {
                input: Some(2_000),
                output: Some(50),
                cached_input: Some(1_500),
                reasoning: None,
            },
        });
        let usage = app.last_usage.expect("usage");
        assert_eq!(usage.input, Some(2_000));
        let report = app.usage_report().join("\n");
        assert!(report.contains("codex/gpt-5"), "{report}");
        assert!(report.contains("2,000"), "{report}");
        assert!(report.contains("reasoning:   unavailable"), "{report}");
        assert!(report.contains("do not expose them"), "{report}");
    }

    #[test]
    fn a_turn_without_usage_clears_stale_counts_and_says_so() {
        let mut app = new_app();
        app.last_usage = Some(TokenUsage {
            input: Some(999),
            ..Default::default()
        });
        app.begin_run(RunId::new("r1"), "grok", None, false);
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Succeeded,
            usage: TokenUsage::default(),
        });

        assert!(app.last_usage.is_none());
        let report = app.usage_report().join("\n");
        assert!(report.contains("grok/default"), "{report}");
        assert!(report.contains("No exact token counts"), "{report}");
    }

    #[test]
    fn status_report_uses_current_argo_state_without_claiming_provider_quota() {
        let mut app = new_app();
        app.conversation = Some(summary(Some("network retry"), 4, &["codex"], Some("gpt-5")));
        app.enqueue("follow-up".into());
        let report = app.status_report().join("\n");
        assert!(report.contains("network retry"), "{report}");
        assert!(report.contains("codex/gpt-5"), "{report}");
        assert!(report.contains("Queued follow-ups: 1"), "{report}");
        assert!(report.contains("not exposed"), "{report}");
    }

    #[test]
    fn selection_label_degrades_without_a_conversation() {
        let app = new_app();
        assert_eq!(app.selection_label(), "no conversation");
    }
}
