//! TUI state.
//!
//! The state is a plain struct mutated by explicit methods, so the interesting
//! behavior — composer editing, streaming assembly, picker navigation — is
//! testable without a terminal attached.

use crate::preferences::DefaultSelection;
use argo_core::event::{RunEventKind, RunStatus};
use argo_core::ids::{ConversationId, RunId};
use argo_core::message::{ContentBlock, ToolStatus};
use argo_daemon::protocol::{ConversationSummary, MessageView};
use argo_runtime::AgentInfo;

/// Returns only the detected version number for a CLI picker row.
///
/// Version commands often repeat the product name (`codex-cli 0.146.0`) or add
/// it after the number (`2.1.220 (Claude Code)`). The picker already displays
/// the friendly CLI name, so retaining that decoration would be noisy and can
/// look like two different products. If no version-shaped token was detected,
/// the row simply shows the CLI name instead of substituting model/capability
/// metadata that may be stale before a deep probe.
pub fn agent_display_version(info: &AgentInfo) -> Option<String> {
    info.version
        .as_deref()
        .and_then(|raw| raw.split_whitespace().find_map(version_token))
}

fn version_token(token: &str) -> Option<String> {
    let trimmed = token.trim_matches(|character: char| {
        matches!(
            character,
            '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '\'' | '"'
        )
    });
    let normalized = match trimmed.as_bytes() {
        [b'v' | b'V', next, ..] if next.is_ascii_digit() => &trimmed[1..],
        _ => trimmed,
    };
    let starts_with_digit = normalized
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_digit);
    let version_characters_only = normalized
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'));
    (starts_with_digit && normalized.contains('.') && version_characters_only)
        .then(|| normalized.to_string())
}

/// A rendered transcript line.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// Who produced it.
    pub kind: LineKind,
    /// Text content.
    pub text: String,
}

#[derive(Debug, Clone)]
struct EditSnapshot {
    input: String,
    cursor: usize,
    multiline_paste: bool,
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
    /// A one-field guided form, optionally masked for credentials.
    Input {
        /// Form title.
        title: String,
        /// Label describing the expected value.
        prompt: String,
        /// Current field contents.
        value: String,
        /// Whether the field must be rendered as bullets.
        secret: bool,
        /// Continuation invoked when Enter is pressed.
        action: InputAction,
    },
}

/// Continuation for a guided input field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    McpAddName,
    McpRemoteUrl,
    McpLocalCommand,
    McpBearerToken,
    McpHeaderName,
    McpHeaderEnv,
    McpLocalEnvName,
    McpLocalEnvSource,
}

/// In-progress MCP server created by the guided TUI flow.
#[derive(Debug, Clone, Default)]
pub struct McpDraft {
    pub name: String,
    pub url: Option<String>,
    pub command: Vec<String>,
    pub headers: Vec<(String, String)>,
    pub environment: Vec<(String, String)>,
    pub pending_key: Option<String>,
}

/// What a picker selection applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerAction {
    /// Choose a CLI from the launch screen.
    StartupAgent,
    /// Choose a model from the launch screen.
    StartupModel,
    /// Choose effort from the launch screen.
    StartupEffort,
    /// Choose a CLI while configuring the saved default.
    DefaultAgent,
    /// Choose a model while configuring the saved default.
    DefaultModel,
    /// Choose effort while configuring the saved default.
    DefaultEffort,
    /// Switch agent.
    Agent,
    /// Browse every supported CLI; Enter switches and Space configures default.
    Agents,
    /// Switch model.
    Model,
    /// Set reasoning effort.
    Effort,
    /// Open a conversation.
    Conversation,
    /// Inspect a delegated conversation without leaving the parent view.
    ChildConversation,
    /// Set the execution mode.
    Mode,
    /// Submit an option offered by the latest assistant response.
    ResponseOption,
    /// Choose local, remote, or imported MCP setup.
    McpAddTransport,
    /// Choose authentication for a remote MCP server.
    McpAddAuth,
    /// Add environment mapping or finish a local server.
    McpLocalConfig,
    /// Select an MCP server discovered in another CLI config.
    McpImport,
    /// Enable, disable, or edit project instructions.
    Instructions,
}

/// One terminal cell used by Argo-owned drag selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScreenPoint {
    /// Zero-based terminal column.
    pub column: u16,
    /// Zero-based terminal row.
    pub row: u16,
}

/// The visible range selected with the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseSelection {
    /// Cell where the drag began.
    pub anchor: ScreenPoint,
    /// Most recent drag/release cell.
    pub focus: ScreenPoint,
    /// Whether the left button is still held.
    pub dragging: bool,
}

impl MouseSelection {
    /// Ordered endpoints in terminal reading order.
    pub fn ordered(self) -> (ScreenPoint, ScreenPoint) {
        if (self.anchor.row, self.anchor.column) <= (self.focus.row, self.focus.column) {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    /// A press/release without travel is a click, not a text range.
    pub fn is_click(self) -> bool {
        self.anchor == self.focus
    }
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
    /// Explicit CLI/model selection applied to new conversations.
    pub default_selection: Option<DefaultSelection>,
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
    /// Short-lived guard requiring a second plain Ctrl+C before leaving the TUI.
    quit_confirmation_deadline: Option<std::time::Instant>,
    /// Transcript scrollback offset from the bottom.
    pub scroll_back: usize,
    /// Whether Argo's minimal mouse-wheel reporting is active.
    ///
    /// Explicit wheel events keep transcript scrolling separate from physical
    /// arrow keys. F2 restores fully terminal-owned mouse selection immediately.
    pub mouse_scroll_mode: bool,
    /// Visible range selected by Argo while mouse reporting remains enabled.
    pub mouse_selection: Option<MouseSelection>,
    /// Text captured from the last completed visible drag.
    selected_screen_text: Option<String>,
    /// Space on the startup CLI picker requests persistence after model/effort.
    pub startup_save_default: bool,
    /// State carried between fields in `/mcp add`.
    pub mcp_draft: Option<McpDraft>,
    /// Width-aware maximum rendered-row scroll, refreshed by the renderer.
    scroll_limit: std::cell::Cell<usize>,
    /// Recent inputs, newest last.
    history: Vec<String>,
    /// Position while browsing history.
    history_cursor: Option<usize>,
    /// Whether the composer contains a multiline bracketed-paste payload.
    multiline_paste: bool,
    /// Previous composer state restored by Ctrl+Y.
    edit_undo: Option<EditSnapshot>,
    /// Whether CLI-emitted reasoning is currently rendered.
    pub thinking_visible: bool,
    /// Assistant text accumulated for the streaming turn.
    streaming: String,
    /// Reasoning text accumulated for the streaming turn.
    thinking_streaming: String,
    /// Tool ids to display names, retained until completion events arrive.
    active_tools: std::collections::HashMap<String, String>,
    /// Adapter attribution for live delegated runs, keyed by durable child run id.
    child_agents: std::collections::HashMap<RunId, String>,
    /// Delegated runs that have spawned but not yet reached a terminal event.
    active_children: std::collections::HashSet<RunId>,
    /// Child tool ids retained independently so their lifecycle cannot disturb the parent.
    child_tools: std::collections::HashMap<(RunId, String), String>,
    /// Child streams already followed in this TUI session, preventing duplicate replay.
    followed_children: std::collections::HashSet<RunId>,
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
    /// Newer published version discovered by the background startup check.
    pub available_update: Option<String>,
    /// Requested self-update after terminal restoration; `true` forces reinstall.
    pub update_on_exit: Option<bool>,
    /// Messages typed while a turn was running, sent in order once it ends.
    ///
    /// Dropping them was the alternative, and it lost work: a follow-up thought
    /// typed mid-turn is exactly the thing a user does not want to retype.
    pub queued: std::collections::VecDeque<String>,
    /// Agent/model attribution captured when the parent run starts.
    /// Child headers may appear later, so reverse-scanning transcript headers at
    /// completion would attribute parent usage to the wrong CLI.
    active_usage_source: Option<String>,
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
    Some(format!("argo --resume {id}"))
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
            default_selection: None,
            lines: Vec::new(),
            input: String::new(),
            cursor: 0,
            active_run: None,
            overlay: Overlay::None,
            status: "Type a message, or /help for commands".to_string(),
            should_quit: false,
            quit_confirmation_deadline: None,
            scroll_back: 0,
            mouse_scroll_mode: false,
            mouse_selection: None,
            selected_screen_text: None,
            startup_save_default: false,
            mcp_draft: None,
            scroll_limit: std::cell::Cell::new(0),
            history: Vec::new(),
            history_cursor: None,
            multiline_paste: false,
            edit_undo: None,
            thinking_visible: true,
            streaming: String::new(),
            thinking_streaming: String::new(),
            active_tools: std::collections::HashMap::new(),
            child_agents: std::collections::HashMap::new(),
            active_children: std::collections::HashSet::new(),
            child_tools: std::collections::HashMap::new(),
            followed_children: std::collections::HashSet::new(),
            completions: Vec::new(),
            completion_index: 0,
            completion_touched: false,
            last_usage: None,
            last_usage_source: None,
            context_tokens: 0,
            version: env!("CARGO_PKG_VERSION").to_string(),
            available_update: None,
            update_on_exit: None,
            queued: std::collections::VecDeque::new(),
            active_usage_source: None,
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

    /// Rotating shortcut hint shown only while a turn is active.
    pub fn shortcut_tip(&self) -> &'static str {
        const TIPS: &[&str] = &[
            "Shift+Enter adds a line",
            "Option+Backspace deletes a word",
            "Cmd+Backspace deletes to line start",
            "Ctrl+Y restores the last edit",
            "F2 toggles wheel/selection mode",
            "Shift+Tab cycles the agent mode",
            "Ctrl+T hides or shows reasoning",
            "/usage shows reported token data",
            "Esc cancels the active turn",
        ];
        // Change roughly every three seconds. Mixing in the run id keeps the
        // first tip from being identical on every turn without needing an RNG.
        let run_hash = self
            .active_run
            .as_ref()
            .map(|run| {
                run.as_str()
                    .bytes()
                    .fold(0usize, |a, b| a.wrapping_add(b as usize))
            })
            .unwrap_or(0);
        TIPS[(self.tick / 33 + run_hash) % TIPS.len()]
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

    /// Arms exit on the first Ctrl+C and exits only on a prompt second press.
    ///
    /// The daemon owns active runs, so leaving the TUI never cancels them. The
    /// short deadline prevents an old, forgotten warning from turning a much
    /// later Ctrl+C into an accidental exit.
    pub fn request_ctrl_c_exit(&mut self) {
        let now = std::time::Instant::now();
        if self
            .quit_confirmation_deadline
            .is_some_and(|deadline| now <= deadline)
        {
            self.should_quit = true;
            self.quit_confirmation_deadline = None;
            return;
        }
        self.quit_confirmation_deadline = Some(now + std::time::Duration::from_secs(3));
        self.set_status("press Ctrl+C again to exit Argo · running agents will continue");
    }

    /// Disarms a pending Ctrl+C exit when the user continues interacting.
    pub fn clear_ctrl_c_exit_confirmation(&mut self) {
        self.quit_confirmation_deadline = None;
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
        self.remember_edit();
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

    /// Inserts pasted text atomically as a single multiline block.
    ///
    /// Bracketed paste delivers the entire clipboard in one event. Line endings
    /// are normalized to `\n` to match the internal composer representation.
    pub fn paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        self.remember_edit();
        let index = self.byte_index(self.cursor);
        self.input.insert_str(index, &normalized);
        self.cursor += normalized.chars().count();
        self.history_cursor = None;
        self.multiline_paste |= normalized.contains('\n');
        self.refresh_completions();
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
        self.input.split('\n').count()
    }

    /// True when a multiline clipboard block is still present in the composer.
    pub fn has_multiline_paste(&self) -> bool {
        self.multiline_paste
    }

    /// Deletes the character before the caret.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.remember_edit();
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
        self.remember_edit();
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.input.replace_range(start..end, "");
        self.refresh_completions();
    }

    /// Deletes the previous whitespace-delimited word (Option+Backspace/Ctrl+W).
    pub fn backspace_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() && chars[start - 1] != '\n' {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.delete_char_range(start, self.cursor);
    }

    /// Deletes from the caret to the start of its logical line (Cmd+Backspace).
    pub fn backspace_to_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let start = chars[..self.cursor]
            .iter()
            .rposition(|ch| *ch == '\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        if start < self.cursor {
            self.delete_char_range(start, self.cursor);
        }
    }

    /// Restores the composer state immediately before the latest edit.
    pub fn undo_edit(&mut self) -> bool {
        let Some(previous) = self.edit_undo.take() else {
            return false;
        };
        let current = EditSnapshot {
            input: std::mem::replace(&mut self.input, previous.input),
            cursor: std::mem::replace(&mut self.cursor, previous.cursor),
            multiline_paste: std::mem::replace(&mut self.multiline_paste, previous.multiline_paste),
        };
        self.edit_undo = Some(current);
        self.history_cursor = None;
        self.refresh_completions();
        true
    }

    fn remember_edit(&mut self) {
        self.edit_undo = Some(EditSnapshot {
            input: self.input.clone(),
            cursor: self.cursor,
            multiline_paste: self.multiline_paste,
        });
    }

    fn delete_char_range(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.remember_edit();
        let start_byte = self.byte_index(start);
        let end_byte = self.byte_index(end);
        self.input.replace_range(start_byte..end_byte, "");
        self.cursor = start;
        self.refresh_completions();
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
        self.multiline_paste = false;
        self.completions.clear();
        self.completion_index = 0;
        self.edit_undo = None;
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
        self.multiline_paste = false;
    }

    /// Moves forward through history, ending at an empty composer.
    pub fn history_next(&mut self) {
        match self.history_cursor {
            Some(current) if current + 1 < self.history.len() => {
                self.history_cursor = Some(current + 1);
                self.input = self.history[current + 1].clone();
                self.cursor = self.input.chars().count();
                self.multiline_paste = false;
            }
            Some(_) => {
                self.history_cursor = None;
                self.input.clear();
                self.cursor = 0;
                self.multiline_paste = false;
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
        self.active_usage_source = Some(format!("{agent}/{model}"));
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
        let final_only = is_plain_text_adapter(agent);
        if final_only {
            self.push(
                LineKind::Notice,
                "· final output only — this CLI does not stream intermediate activity",
            );
        }
        self.activity = Activity::Starting;
        self.activity_detail = final_only.then(|| "waiting for final output".to_string());
        self.tick = 0;
        self.set_status(if final_only {
            format!("{agent} · final output only (no streaming) · Esc to cancel")
        } else {
            format!("{agent} · waiting for first output · Esc to cancel")
        });
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
                ..
            } => {
                self.child_agents
                    .insert(child_run_id.clone(), child_agent_id.to_string());
                self.active_children.insert(child_run_id.clone());
                self.activity = Activity::Working;
                self.activity_detail = Some(format!("subagent {child_agent_id}"));
                self.push(
                    LineKind::Activity,
                    format!(
                        "↳ delegated agent {} ({}) — {} · /children to inspect",
                        child_agent_id,
                        child_run_id,
                        compact_activity(&task)
                    ),
                );
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::ChildEvent {
                child_run_id,
                event,
            } => {
                self.apply_child_event(child_run_id, *event);
                self.streaming.clear();
                self.thinking_streaming.clear();
            }
            RunEventKind::ChildCompleted {
                child_run_id,
                status,
            } => {
                self.active_children.remove(&child_run_id);
                self.push(
                    LineKind::Activity,
                    format!("✓ delegated agent {child_run_id} — {status:?} · /children"),
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
                    || code == "NATIVE_SUBAGENT_ACTIVITY"
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
                // Only update usage/source on Succeeded; failure/cancellation
                // leaves previous successful usage intact.
                if status == RunStatus::Succeeded {
                    self.last_usage_source = self.active_usage_source.clone();
                    self.last_usage = (!usage.is_empty()).then_some(usage);
                }
                self.active_usage_source = None;
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

    /// Registers one child stream for following, returning true only once.
    pub fn follow_child(&mut self, run_id: RunId, agent_id: impl Into<String>) -> bool {
        self.child_agents.insert(run_id.clone(), agent_id.into());
        self.active_children.insert(run_id.clone());
        self.followed_children.insert(run_id)
    }

    /// Number of delegated agents known in this live TUI session.
    pub fn delegated_agent_counts(&self) -> (usize, usize) {
        (self.active_children.len(), self.child_agents.len())
    }

    /// Applies an event from a delegated child without mutating parent run state.
    pub fn apply_child_event(&mut self, run_id: RunId, kind: RunEventKind) {
        let known_agent = self
            .child_agents
            .get(&run_id)
            .cloned()
            .unwrap_or_else(|| "subagent".to_string());

        match kind {
            RunEventKind::RunStarted {
                agent_id,
                model,
                resumed,
            } => {
                let agent = agent_id.to_string();
                self.child_agents.insert(run_id.clone(), agent.clone());
                let model = model.unwrap_or_else(|| "default".to_string());
                self.push(
                    LineKind::AgentHeader,
                    format!(
                        "{agent} · {model} · subagent {}{}",
                        run_id,
                        if resumed { " · resumed" } else { "" }
                    ),
                );
            }
            RunEventKind::TextDelta { text } if !text.is_empty() => {
                let prefix = format!("[{known_agent} subagent] ");
                match self.lines.last_mut() {
                    Some(line)
                        if line.kind == LineKind::Assistant && line.text.starts_with(&prefix) =>
                    {
                        line.text.push_str(&text)
                    }
                    _ => self.push(LineKind::Assistant, format!("{prefix}{text}")),
                }
            }
            RunEventKind::ThinkingDelta { text } if !text.is_empty() => {
                let prefix = format!("[{known_agent} subagent] ");
                match self.lines.last_mut() {
                    Some(line)
                        if line.kind == LineKind::Thinking && line.text.starts_with(&prefix) =>
                    {
                        line.text.push_str(&text)
                    }
                    _ => self.push(LineKind::Thinking, format!("{prefix}{text}")),
                }
            }
            RunEventKind::ToolStarted { id, name, input } => {
                self.child_tools.insert((run_id, id), name.clone());
                let detail = input
                    .as_deref()
                    .map(compact_activity)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!(" — {value}"))
                    .unwrap_or_default();
                self.push(
                    LineKind::Activity,
                    format!("[{known_agent} subagent] ↳ calling {name}{detail}"),
                );
            }
            RunEventKind::ToolCompleted { id, output, ok } => {
                let name = self
                    .child_tools
                    .remove(&(run_id, id))
                    .unwrap_or_else(|| "tool".to_string());
                let mark = if ok { "✓" } else { "✗" };
                let detail = output
                    .as_deref()
                    .map(compact_activity)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!(" — {value}"))
                    .unwrap_or_default();
                self.push(
                    LineKind::Activity,
                    format!("[{known_agent} subagent] {mark} {name}{detail}"),
                );
            }
            RunEventKind::FileWritten { path } => self.push(
                LineKind::Activity,
                format!("[{known_agent} subagent] ✎ wrote {path}"),
            ),
            RunEventKind::PlanUpdated { steps } => {
                self.push(
                    LineKind::Activity,
                    format!("[{known_agent} subagent] · plan updated"),
                );
                for step in steps {
                    self.push(LineKind::Activity, format!("  {step}"));
                }
            }
            RunEventKind::ChildSpawned {
                child_run_id,
                child_agent_id,
                task,
                ..
            } => {
                self.child_agents
                    .insert(child_run_id.clone(), child_agent_id.to_string());
                self.active_children.insert(child_run_id.clone());
                self.push(
                    LineKind::Activity,
                    format!(
                        "[{known_agent} subagent] ↳ delegated agent {} ({}) — {} · /children",
                        child_agent_id,
                        child_run_id,
                        compact_activity(&task)
                    ),
                );
            }
            RunEventKind::ChildCompleted {
                child_run_id,
                status,
            } => {
                self.active_children.remove(&child_run_id);
                self.push(
                    LineKind::Activity,
                    format!("[{known_agent} subagent] ✓ delegated agent {child_run_id} — {status:?} · /children"),
                );
            }
            RunEventKind::SessionReseeded { reason } => self.push(
                LineKind::Notice,
                format!("[{known_agent} subagent] · {reason}; retrying with full context"),
            ),
            RunEventKind::Diagnostic { code, detail }
                if matches!(
                    code.as_str(),
                    "PERMISSION_AUTO_APPROVED"
                        | "RUN_INTERRUPTED"
                        | "TRANSIENT_RETRY"
                        | "ACP_METHOD_UNSUPPORTED"
                        | "ACP_UPDATE"
                        | "THINKING_UNAVAILABLE"
                        | "NATIVE_SUBAGENT_ACTIVITY"
                ) =>
            {
                self.push(
                    LineKind::Notice,
                    format!("[{known_agent} subagent] · {detail}"),
                );
            }
            RunEventKind::Error { message, .. } => self.push(
                LineKind::Error,
                format!("[{known_agent} subagent] ! {message}"),
            ),
            RunEventKind::RunFinished { status, usage } => {
                self.active_children.remove(&run_id);
                let mut detail =
                    format!("[{known_agent} subagent] · child commit barrier — {status:?}");
                if let (Some(input), Some(output)) = (usage.input, usage.output) {
                    detail.push_str(&format!(" · {input} in / {output} out"));
                }
                self.push(LineKind::Activity, detail);
                self.child_tools.retain(|(child, _), _| child != &run_id);
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

    /// Opens one field in a guided form.
    pub fn open_input(
        &mut self,
        title: impl Into<String>,
        prompt: impl Into<String>,
        secret: bool,
        action: InputAction,
    ) {
        self.overlay = Overlay::Input {
            title: title.into(),
            prompt: prompt.into(),
            value: String::new(),
            secret,
            action,
        };
    }

    /// Inserts text into a guided form field.
    pub fn overlay_input_push_str(&mut self, text: &str) {
        if let Overlay::Input { value, .. } = &mut self.overlay {
            value.push_str(text);
        }
    }

    /// Deletes one character from a guided form field.
    pub fn overlay_input_pop(&mut self) {
        if let Overlay::Input { value, .. } = &mut self.overlay {
            value.pop();
        }
    }

    /// Takes a guided field value and closes it.
    pub fn overlay_submit_input(&mut self) -> Option<(InputAction, String)> {
        let submitted = match &self.overlay {
            Overlay::Input { value, action, .. } => Some((*action, value.clone())),
            _ => None,
        };
        if submitted.is_some() {
            self.close_overlay();
        }
        submitted
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

    /// Appends live progress when the matching read-only pane is still open.
    pub fn append_text_overlay(&mut self, expected_title: &str, line: String) -> bool {
        match &mut self.overlay {
            Overlay::Text { title, lines, .. } if title == expected_title => {
                lines.push(line);
                true
            }
            _ => false,
        }
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
            Overlay::Input { .. } => {}
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

    /// Replaces one adapter after its selected-agent deep probe completes.
    pub fn update_agent(&mut self, agent: argo_runtime::AgentInfo) {
        if let Some(existing) = self.agents.iter_mut().find(|item| item.id == agent.id) {
            *existing = agent;
        } else {
            self.agents.push(agent);
        }
    }

    /// Modes the selected adapter can actually enforce.
    pub fn mode_support(&self) -> argo_core::mode::ModeSupport {
        self.conversation
            .as_ref()
            .and_then(|c| c.selected_agent_id.as_deref())
            .and_then(argo_runtime::find)
            .map(|def| def.capabilities.modes)
            .unwrap_or(argo_core::mode::ModeSupport::NONE)
            .with_argo_plan()
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
        let Some(agent) = conversation.selected_agent_id.clone() else {
            return "choose CLI".into();
        };
        let model = conversation
            .selected_model
            .clone()
            .unwrap_or_else(|| "choose model".into());
        format!("{agent}/{model}")
    }

    /// Starts an application-owned selection without giving up wheel events.
    pub fn begin_mouse_selection(&mut self, column: u16, row: u16) {
        let point = ScreenPoint { column, row };
        self.mouse_selection = Some(MouseSelection {
            anchor: point,
            focus: point,
            dragging: true,
        });
        self.selected_screen_text = None;
    }

    /// Extends the current visible selection.
    pub fn update_mouse_selection(&mut self, column: u16, row: u16) {
        if let Some(selection) = &mut self.mouse_selection {
            selection.focus = ScreenPoint { column, row };
        }
    }

    /// Finishes a selection and remembers its visible text for the copy chord.
    pub fn finish_mouse_selection(&mut self, column: u16, row: u16, text: Option<String>) {
        self.update_mouse_selection(column, row);
        if let Some(selection) = &mut self.mouse_selection {
            selection.dragging = false;
        }
        self.selected_screen_text = text.filter(|value| !value.is_empty());
    }

    /// Removes the application-owned selection.
    pub fn clear_mouse_selection(&mut self) {
        self.mouse_selection = None;
        self.selected_screen_text = None;
    }

    /// Text captured by the most recent completed drag.
    pub fn selected_screen_text(&self) -> Option<&str> {
        self.selected_screen_text.as_deref()
    }

    /// Changes whether reasoning lines are drawn without deleting canonical data.
    pub fn set_thinking_visible(&mut self, visible: bool) {
        self.thinking_visible = visible;
        // Filtering reasoning changes the rendered row count. Following the live
        // tail prevents the old scroll offset from making the toggle appear to do
        // nothing or from leaving the viewport on unrelated earlier content.
        self.scroll_back = 0;
        self.clear_mouse_selection();
        self.set_status(if visible {
            "thinking is visible · Ctrl+T or /thinking hide to collapse it"
        } else {
            "thinking is hidden · Ctrl+T or /thinking show to reveal it"
        });
    }

    /// Exact last-turn token accounting reported by the selected CLI stream.
    pub fn usage_report(&self) -> Vec<String> {
        let Some(source) = self.last_usage_source.as_deref() else {
            return vec![
                "No successful completed turn yet.".to_string(),
                "Send a message first, then check /usage.".to_string(),
            ];
        };
        let mut lines = vec![format!("Last completed turn: {source}")];
        let agent_id = source.split('/').next().unwrap_or("");
        match self.last_usage {
            Some(usage) => {
                lines.push(format!("input:       {}", token_count(usage.input)));
                lines.push(format!("output:      {}", token_count(usage.output)));
                lines.push(format!("cached input: {}", token_count(usage.cached_input)));
                lines.push(format!("reasoning:   {}", token_count(usage.reasoning)));
                lines.push(String::new());
                lines.push(format!(
                    "Exact values reported by the {} stream for this turn.",
                    adapter_display_name(agent_id)
                ));
                lines.push(
                    "A field showing \"unavailable\" was not emitted; Argo does not estimate it."
                        .into(),
                );
            }
            None if is_plain_text_adapter(agent_id) => {
                lines.push(format!(
                    "{} uses plain-text output; token counts are structurally unavailable.",
                    adapter_display_name(agent_id)
                ));
            }
            None => {
                lines.push(format!(
                    "{} did not report exact token counts for this completed turn.",
                    adapter_display_name(agent_id)
                ));
                lines.push("Argo does not estimate values the CLI did not emit.".into());
            }
        }
        // OpenCode-specific tip only when the adapter is actually OpenCode.
        if agent_id == "opencode" {
            lines.push(String::new());
            lines.push("For cumulative stats, run `opencode stats` in a separate terminal.".into());
        }
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
            "Use /usage for provider allowance when the selected CLI exposes a safe status interface."
                .into(),
        );
        lines
    }

    /// Rebuilds canonical history using the same visual vocabulary as live events.
    pub fn replace_transcript(&mut self, messages: Vec<MessageView>) {
        self.lines.clear();
        self.history.clear();
        self.history_cursor = None;
        // Clear usage — will be restored from the last assistant message with Some(usage).
        self.last_usage = None;
        self.last_usage_source = None;
        for message in &messages {
            match message.role.as_str() {
                "user" => {
                    if !message.text.trim().is_empty() {
                        self.history.push(message.text.clone());
                    }
                    if message.blocks.is_empty() {
                        self.push(LineKind::User, message.text.clone());
                    } else {
                        for block in &message.blocks {
                            if let ContentBlock::Text { text } = block {
                                self.push(LineKind::User, text.clone());
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
                        self.push(LineKind::Assistant, message.text.clone());
                        continue;
                    }
                    for block in &message.blocks {
                        match block {
                            ContentBlock::Text { text } => {
                                if !text.trim().is_empty() {
                                    self.push(LineKind::Assistant, text.clone());
                                }
                            }
                            ContentBlock::Thinking { text } => {
                                if !text.trim().is_empty() {
                                    self.push(LineKind::Thinking, text.clone());
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
                            ContentBlock::ChildActivity {
                                run_id,
                                agent_id,
                                task,
                                status,
                                blocks,
                            } => {
                                self.push(
                                    LineKind::Activity,
                                    format!(
                                        "↳ subagent {agent_id} ({run_id}) — {}",
                                        compact_activity(task)
                                    ),
                                );
                                for child_block in blocks {
                                    match child_block {
                                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                                            self.push(
                                                LineKind::Assistant,
                                                format!("[{agent_id} subagent] {text}"),
                                            );
                                        }
                                        ContentBlock::Thinking { text }
                                            if !text.trim().is_empty() =>
                                        {
                                            self.push(
                                                LineKind::Thinking,
                                                format!("[{agent_id} subagent] {text}"),
                                            );
                                        }
                                        ContentBlock::Tool { call } => {
                                            self.push(
                                                LineKind::Activity,
                                                format!(
                                                    "[{agent_id} subagent] ↳ calling {}",
                                                    call.name
                                                ),
                                            );
                                            if call.status != ToolStatus::Pending {
                                                self.push(
                                                    LineKind::Activity,
                                                    format!(
                                                        "[{agent_id} subagent] {} {}",
                                                        if call.status == ToolStatus::Completed {
                                                            "✓"
                                                        } else {
                                                            "✗"
                                                        },
                                                        call.name
                                                    ),
                                                );
                                            }
                                        }
                                        ContentBlock::FileWrite { path } => self.push(
                                            LineKind::Activity,
                                            format!("[{agent_id} subagent] ✎ wrote {path}"),
                                        ),
                                        _ => {}
                                    }
                                }
                                if let Some(status) = status {
                                    self.push(
                                        LineKind::Activity,
                                        format!("✓ subagent {run_id} — {status:?}"),
                                    );
                                }
                            }
                        }
                    }
                }
                _ => {
                    if message.blocks.is_empty() {
                        self.push(LineKind::Notice, message.text.clone());
                    } else {
                        for block in &message.blocks {
                            if let ContentBlock::Text { text } = block {
                                self.push(LineKind::Notice, text.clone());
                            }
                        }
                    }
                }
            }
        }
        // Restore usage from the last assistant message that reports it (Succeeded).
        if let Some(last_with_usage) = messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && m.usage.is_some())
        {
            let usage = last_with_usage.usage.unwrap();
            let source = format!(
                "{}/{}",
                last_with_usage
                    .agent_id
                    .clone()
                    .unwrap_or_else(|| "default".into()),
                last_with_usage
                    .model
                    .clone()
                    .unwrap_or_else(|| "default".into())
            );
            self.last_usage_source = Some(source);
            self.last_usage = (!usage.is_empty()).then_some(usage);
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
        let description = summary
            .description
            .as_deref()
            .filter(|description| !description.trim().is_empty() && *description != title)
            .map(|description| format!(" — {description}"))
            .unwrap_or_default();
        let agents = if summary.agents_with_sessions.is_empty() {
            "no agent yet".to_string()
        } else {
            summary.agents_with_sessions.join("+")
        };
        format!(
            "{title}{description}  ·  {} msg  ·  {agents}  ·  {}",
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

/// Whether the registered adapter's transport can carry structured usage.
fn is_plain_text_adapter(agent_id: &str) -> bool {
    argo_runtime::find(agent_id).is_some_and(|definition| {
        definition.capabilities.stream_format == argo_core::runtime::StreamFormat::Plain
    })
}

/// Human-readable adapter name from the same registry used to execute turns.
fn adapter_display_name(agent_id: &str) -> &str {
    argo_runtime::find(agent_id).map_or(agent_id, |definition| definition.name)
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

/// Extracts a deliberate numbered, lettered, or bulleted choice request.
///
/// Numbered prose alone is not enough: a nearby choice phrase is required so
/// ordinary explanations and plans do not unexpectedly become interactive.
///
/// Guards against false positives:
/// - The choice phrase must appear within the last 200 characters before the
///   first numbered item (proximity).
/// - The numbered list must be terminal (only trailing whitespace after it).
/// - Each option must be brief (≤150 chars); long paragraphs are explanations.
/// - Code blocks (fenced with ```) are stripped before phrase detection.
fn response_options(response: &str) -> Option<Vec<String>> {
    // Strip fenced code blocks so SQL `SELECT` etc. do not trigger.
    let stripped = strip_fenced_code_blocks(response);
    let lower = stripped.to_ascii_lowercase();

    // Require a deliberate interactive phrase — bare "choose"/"select" removed.
    const CHOICE_PHRASES: &[&str] = &[
        "which do you want",
        "which option",
        "which approach",
        "which would you",
        "pick one",
        "pick an option",
        "please pick",
        "choose one",
        "choose an option",
        "select one",
        "select an option",
        "choose from",
        "select from",
        "reply with",
        "respond with",
        "pick between",
        "what would you like",
        "how would you like",
        "would you like me to",
        "what do you prefer",
    ];

    // Locate the numbered list first, then check proximity.
    let mut options = Vec::new();
    let mut expected: Option<usize> = None;
    let mut first_option_byte: Option<usize> = None;
    let mut last_option_end: usize = 0;

    let mut line_start = 0usize;
    for segment in stripped.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let current_line_start = line_start;
        line_start += segment.len();
        let trimmed = line.trim();
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            continue;
        }
        let Ok(number) = trimmed[..digits].parse::<usize>() else {
            continue;
        };
        let Some(rest) = trimmed[digits..].strip_prefix(['.', ')']) else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() {
            continue;
        }
        // Brevity check: explanatory paragraphs are not chooseable options.
        if rest.chars().count() > 150 {
            options.clear();
            expected = None;
            first_option_byte = None;
            continue;
        }
        match expected {
            None => {
                first_option_byte = Some(current_line_start);
                expected = Some(number + 1);
            }
            Some(next) if number == next => {
                expected = Some(number + 1);
            }
            Some(_) => {
                options.clear();
                first_option_byte = Some(current_line_start);
                expected = Some(number + 1);
            }
        }
        last_option_end = current_line_start + line.len();
        options.push(format!("{number}. {rest}"));
    }

    if options.len() < 2 {
        return terminal_bulleted_options(&stripped, &lower, CHOICE_PHRASES);
    }

    // Terminal check: only trailing whitespace after the last option.
    if !stripped[last_option_end..].trim().is_empty() {
        return None;
    }

    // Proximity check: a choice phrase must appear within the last 200 chars
    // before the first numbered item.
    let first_byte = first_option_byte.unwrap_or(0);
    if !choice_phrase_near(&lower, first_byte, CHOICE_PHRASES) {
        return None;
    }

    Some(options)
}

/// Extracts a contiguous terminal list such as `A) ...` or `- ...`.
fn terminal_bulleted_options(stripped: &str, lower: &str, phrases: &[&str]) -> Option<Vec<String>> {
    let mut lines = Vec::new();
    let mut byte = 0usize;
    for segment in stripped.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        lines.push((byte, line));
        byte += segment.len();
    }
    while lines.last().is_some_and(|(_, line)| line.trim().is_empty()) {
        lines.pop();
    }

    let mut options = Vec::new();
    let mut first_byte = 0usize;
    for (line_byte, line) in lines.into_iter().rev() {
        let trimmed = line.trim();
        let rest = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("• "))
            .or_else(|| {
                let mut chars = trimmed.chars();
                let letter = chars.next()?;
                let delimiter = chars.next()?;
                (letter.is_ascii_alphabetic() && matches!(delimiter, '.' | ')'))
                    .then(|| chars.as_str().trim_start())
            });
        let Some(rest) = rest else {
            break;
        };
        if rest.is_empty() || rest.chars().count() > 150 {
            return None;
        }
        first_byte = line_byte;
        options.push(trimmed.to_string());
        if options.len() > 12 {
            return None;
        }
    }
    options.reverse();
    if options.len() < 2 || !choice_phrase_near(lower, first_byte, phrases) {
        return None;
    }
    Some(options)
}

fn choice_phrase_near(lower: &str, first_byte: usize, phrases: &[&str]) -> bool {
    let prefix = &lower[..first_byte];
    let window_start = prefix
        .char_indices()
        .rev()
        .nth(199)
        .map_or(0, |(index, _)| index);
    let proximity_window = &lower[window_start..first_byte];
    phrases
        .iter()
        .any(|phrase| proximity_window.contains(phrase))
}

/// Removes fenced code blocks (``` … ```) so their content does not trigger
/// choice-phrase detection. Non-code text is preserved.
fn strip_fenced_code_blocks(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut inside_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            inside_fence = !inside_fence;
            result.push('\n');
            continue;
        }
        if inside_fence {
            result.push('\n');
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    use argo_core::event::TokenUsage;

    fn new_app() -> App {
        App::new("/repo")
    }

    #[test]
    fn picker_versions_remove_repeated_product_names() {
        let claude = argo_runtime::find("claude").expect("claude definition");
        let mut claude = AgentInfo::unavailable(claude, "test fixture");
        claude.version = Some("2.1.220 (Claude Code)".into());
        assert_eq!(agent_display_version(&claude).as_deref(), Some("2.1.220"));

        let codex = argo_runtime::find("codex").expect("codex definition");
        let mut codex = AgentInfo::unavailable(codex, "test fixture");
        codex.version = Some("codex-cli v0.146.0".into());
        assert_eq!(agent_display_version(&codex).as_deref(), Some("0.146.0"));
    }

    #[test]
    fn picker_omits_detail_when_no_version_was_detected() {
        let opencode = argo_runtime::find("opencode").expect("opencode definition");
        let mut opencode = AgentInfo::unavailable(opencode, "test fixture");
        assert_eq!(agent_display_version(&opencode), None);

        opencode.version = Some("OpenCode development build".into());
        assert_eq!(agent_display_version(&opencode), None);
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
        // Must be exactly one line: argo --resume <full-id>
        assert_eq!(
            message,
            "argo --resume abcd1234-5678-90ab-cdef-1234567890ab"
        );
        // No blank prefix.
        assert!(!message.starts_with('\n'));
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
    fn child_streams_are_attributed_without_finishing_the_parent() {
        let mut app = new_app();
        let parent = RunId::new("parent-run");
        let child = RunId::new("child-run");
        app.begin_run(parent.clone(), "claude", None, false);
        app.enqueue("follow up".into());
        app.last_usage = Some(argo_core::event::TokenUsage {
            input: Some(7),
            ..Default::default()
        });
        assert!(app.follow_child(child.clone(), "codex"));
        assert!(!app.follow_child(child.clone(), "codex"));

        app.apply_child_event(
            child.clone(),
            RunEventKind::RunStarted {
                agent_id: argo_core::ids::AgentId::new("codex"),
                model: Some("gpt-5.6-sol".into()),
                resumed: false,
            },
        );
        app.apply_child_event(
            child.clone(),
            RunEventKind::ThinkingDelta {
                text: "checking".into(),
            },
        );
        app.apply_child_event(
            child.clone(),
            RunEventKind::TextDelta {
                text: "found it".into(),
            },
        );
        app.apply_child_event(
            child,
            RunEventKind::RunFinished {
                status: RunStatus::Succeeded,
                usage: argo_core::event::TokenUsage {
                    input: Some(100),
                    output: Some(20),
                    ..Default::default()
                },
            },
        );

        assert_eq!(app.active_run, Some(parent));
        assert_eq!(app.delegated_agent_counts(), (0, 1));
        assert_eq!(app.queue_depth(), 1);
        assert_eq!(app.last_usage.and_then(|usage| usage.input), Some(7));
        assert!(app.lines.iter().any(|line| {
            line.kind == LineKind::Thinking && line.text.contains("[codex subagent] checking")
        }));
        assert!(app.lines.iter().any(|line| {
            line.kind == LineKind::Assistant && line.text.contains("[codex subagent] found it")
        }));
    }

    #[test]
    fn inline_native_child_events_never_merge_into_parent_prose() {
        let mut app = new_app();
        app.begin_run(RunId::new("parent"), "claude", None, false);
        let child = RunId::new("claude-native-t1");
        app.apply_event(RunEventKind::ChildSpawned {
            child_run_id: child.clone(),
            child_agent_id: argo_core::ids::AgentId::new("claude/explore"),
            task: "inspect the parser".into(),
            native: true,
        });
        app.apply_event(RunEventKind::ChildEvent {
            child_run_id: child,
            event: Box::new(RunEventKind::TextDelta {
                text: "native finding".into(),
            }),
        });
        assert!(app.lines.iter().any(|line| {
            line.kind == LineKind::Assistant
                && line
                    .text
                    .contains("[claude/explore subagent] native finding")
        }));
        assert!(app.streaming.is_empty());
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
        app.insert('l');
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
    fn ctrl_c_requires_two_prompt_presses_to_exit() {
        let mut app = new_app();

        app.request_ctrl_c_exit();
        assert!(!app.should_quit);
        assert!(app.status.contains("Ctrl+C again"));

        app.request_ctrl_c_exit();
        assert!(app.should_quit);
    }

    #[test]
    fn continuing_to_interact_disarms_ctrl_c_exit() {
        let mut app = new_app();

        app.request_ctrl_c_exit();
        app.clear_ctrl_c_exit_confirmation();
        app.request_ctrl_c_exit();

        assert!(!app.should_quit);
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
            description: None,
            selected_agent_id: Some("codex".into()),
            selected_model: model.map(str::to_string),
            selected_reasoning: None,
            selected_mode: None,
            message_count: messages,
            agents_with_sessions: agents.iter().map(|a| a.to_string()).collect(),
            parent_conversation_id: None,
            workspace: Some("/test".into()),
            updated_at: argo_core::now_millis(),
        }
    }

    #[test]
    fn authoritative_summary_updates_header_and_cached_description_together() {
        let mut app = new_app();
        app.set_conversation_summary(summary(None, 0, &[], None));

        let mut updated = summary(Some("fix immediate metadata"), 2, &["codex"], Some("gpt-5"));
        updated.description = Some("Started with setup. Current focus: metadata".into());
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
        assert!(
            description.contains("Current focus: metadata"),
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
                usage: None,
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
                usage: None,
                created_at: 2,
            },
        ]);

        assert_eq!(app.lines[0].kind, LineKind::User);
        app.history_previous();
        assert_eq!(app.input, "run it", "reopened user prompts enter history");
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
        assert!(report.contains("Codex CLI stream"), "{report}");
        // Must NEVER mention billing/quota/credits.
        assert!(!report.contains("quota"), "{report}");
        assert!(!report.contains("billing"), "{report}");
        assert!(!report.contains("credits"), "{report}");
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
        assert!(
            report.contains("plain-text output"),
            "grok should be identified as plain-text: {report}"
        );
        assert!(report.contains("structurally unavailable"), "{report}");
        // Must not mention unrelated adapters or billing.
        assert!(!report.contains("opencode"), "{report}");
        assert!(!report.contains("quota"), "{report}");
    }

    #[test]
    fn status_report_uses_current_argo_state_and_points_to_usage() {
        let mut app = new_app();
        app.conversation = Some(summary(Some("network retry"), 4, &["codex"], Some("gpt-5")));
        app.enqueue("follow-up".into());
        let report = app.status_report().join("\n");
        assert!(report.contains("network retry"), "{report}");
        assert!(report.contains("codex/gpt-5"), "{report}");
        assert!(report.contains("Queued follow-ups: 1"), "{report}");
        assert!(
            report.contains("Use /usage for provider allowance"),
            "{report}"
        );
    }

    #[test]
    fn selection_label_degrades_without_a_conversation() {
        let app = new_app();
        assert_eq!(app.selection_label(), "no conversation");
    }

    // --- Regression: Issue 1 — picker vs false positives ---

    #[test]
    fn sql_select_does_not_trigger_response_picker() {
        let mut app = new_app();
        app.push(LineKind::AgentHeader, "claude · sonnet · fresh session");
        app.push(
            LineKind::Assistant,
            "Here's the query:\n\n```sql\nSELECT * FROM users WHERE id = 1;\n```\n\nThe results:\n1. User found\n2. Row returned",
        );
        assert!(!app.open_latest_response_options());
    }

    #[test]
    fn numbered_list_in_middle_of_response_does_not_trigger_picker() {
        let mut app = new_app();
        app.push(LineKind::AgentHeader, "codex · gpt-5 · fresh session");
        app.push(
            LineKind::Assistant,
            "Which option would you like?\n\n1. Keep it\n2. Remove it\n\nEither way, I recommend testing first.",
        );
        // The numbered list is NOT terminal (prose follows), so no picker.
        assert!(!app.open_latest_response_options());
    }

    #[test]
    fn long_numbered_paragraphs_do_not_trigger_picker() {
        let mut app = new_app();
        app.push(LineKind::AgentHeader, "claude · sonnet · fresh session");
        let long_item = "a".repeat(200);
        app.push(
            LineKind::Assistant,
            format!("Which option do you want?\n\n1. {long_item}\n2. {long_item}"),
        );
        // Items are too long (>150 chars) — these are explanatory paragraphs.
        assert!(!app.open_latest_response_options());
    }

    #[test]
    fn choice_phrase_far_from_numbered_list_does_not_trigger() {
        let mut app = new_app();
        app.push(LineKind::AgentHeader, "claude · sonnet · fresh session");
        let filler = "This is explanation text. ".repeat(20); // >200 chars
        app.push(
            LineKind::Assistant,
            format!("Which option do you want?\n\n{filler}\n\n1. Option A\n2. Option B"),
        );
        // The choice phrase is >200 chars from the first numbered item.
        assert!(!app.open_latest_response_options());
    }

    #[test]
    fn unicode_near_choice_window_is_safe_and_detected() {
        let mut app = new_app();
        app.push(LineKind::AgentHeader, "kiro · auto · fresh session");
        let context = "é".repeat(80);
        app.push(
            LineKind::Assistant,
            format!(
                "{context} How would you like to proceed?

1. Keep it
2. Replace it"
            ),
        );
        assert!(app.open_latest_response_options());
    }

    #[test]
    fn common_imperative_choice_phrases_trigger_picker() {
        for prompt in [
            "Choose an option:
1. Keep it
2. Replace it",
            "Select an option:
1. Keep it
2. Replace it",
            "Would you like me to:
1. Keep it
2. Replace it",
        ] {
            assert!(response_options(prompt).is_some(), "{prompt}");
        }
    }

    #[test]
    fn lettered_and_bulleted_terminal_choices_trigger_picker() {
        for prompt in [
            "Choose from these:\nA) Keep the current implementation\nB) Replace it",
            "Reply with one:\n- Keep the current implementation\n- Replace it",
        ] {
            let options = response_options(prompt).expect(prompt);
            assert_eq!(options.len(), 2);
        }
    }

    #[test]
    fn ordinary_bulleted_steps_do_not_become_a_picker() {
        assert!(response_options("Next steps:\n- Run tests\n- Review the diff").is_none());
    }

    #[test]
    fn deliberate_choice_at_end_still_triggers_picker() {
        let mut app = new_app();
        app.push(LineKind::AgentHeader, "codex · o3 · resumed session");
        app.push(
            LineKind::Assistant,
            "I found two approaches. Which would you like?\n\n1. Refactor the module\n2. Add a wrapper",
        );
        assert!(app.open_latest_response_options());
        let Overlay::Picker { items, .. } = &app.overlay else {
            panic!("expected picker");
        };
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn code_block_with_choose_does_not_trigger_picker() {
        let mut app = new_app();
        app.push(LineKind::AgentHeader, "claude · sonnet · fresh session");
        app.push(
            LineKind::Assistant,
            "Here's the code:\n\n```rust\n// choose one of these\nlet x = vec![1, 2, 3];\n```\n\n1. First approach\n2. Second approach",
        );
        // "choose" is inside a code block — should not count.
        // And there's no choice phrase outside the code block near the list.
        assert!(!app.open_latest_response_options());
    }

    #[test]
    fn arrow_keys_navigate_picker_not_history() {
        let mut app = new_app();
        // Type something into composer history first.
        for ch in "previous message".chars() {
            app.insert(ch);
        }
        app.take_input();

        // Now open a response picker.
        app.push(LineKind::AgentHeader, "claude · sonnet · fresh session");
        app.push(
            LineKind::Assistant,
            "Pick one:\n\n1. Alpha\n2. Beta\n3. Gamma",
        );
        assert!(app.open_latest_response_options());
        assert!(app.has_overlay());

        // Up/Down should navigate the picker, not recall history.
        app.overlay_move(1); // Down
        let Overlay::Picker { selected, .. } = &app.overlay else {
            panic!("expected picker");
        };
        assert_eq!(*selected, 1);

        // Enter should choose from the picker.
        let (action, value) = app.overlay_choose().expect("selection");
        assert_eq!(action, PickerAction::ResponseOption);
        assert_eq!(value, "2. Beta");
        // Composer should be empty — history not recalled.
        assert!(app.input.is_empty());
    }

    // --- Regression: Issue 2 — adapter-specific /usage ---

    #[test]
    fn usage_report_for_claude_with_all_fields() {
        let mut app = new_app();
        app.begin_run(
            RunId::new("r1"),
            "claude",
            Some("claude-sonnet-4-20250514"),
            false,
        );
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Succeeded,
            usage: TokenUsage {
                input: Some(5_234),
                output: Some(1_891),
                cached_input: Some(3_100),
                reasoning: Some(450),
            },
        });
        let report = app.usage_report().join("\n");
        assert!(
            report.contains("claude/claude-sonnet-4-20250514"),
            "{report}"
        );
        assert!(report.contains("5,234"), "{report}");
        assert!(report.contains("1,891"), "{report}");
        assert!(report.contains("3,100"), "{report}");
        assert!(report.contains("450"), "{report}");
        assert!(report.contains("Claude Code stream"), "{report}");
        assert!(!report.contains("quota"), "{report}");
        assert!(!report.contains("billing"), "{report}");
        assert!(!report.contains("opencode"), "{report}");
    }

    #[test]
    fn usage_report_for_opencode_includes_stats_tip() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "opencode", Some("o3"), false);
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Succeeded,
            usage: TokenUsage {
                input: Some(1_000),
                output: Some(200),
                cached_input: None,
                reasoning: None,
            },
        });
        let report = app.usage_report().join("\n");
        assert!(report.contains("opencode/o3"), "{report}");
        assert!(report.contains("opencode stats"), "{report}");
        assert!(!report.contains("quota"), "{report}");
    }

    #[test]
    fn usage_report_for_command_code_says_plain_text() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "cmd", Some("gemini"), false);
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Succeeded,
            usage: TokenUsage::default(),
        });
        let report = app.usage_report().join("\n");
        assert!(report.contains("cmd/gemini"), "{report}");
        assert!(report.contains("Command Code"), "{report}");
        assert!(report.contains("plain-text output"), "{report}");
        assert!(report.contains("structurally unavailable"), "{report}");
        assert!(!report.contains("quota"), "{report}");
        assert!(!report.contains("opencode"), "{report}");
    }

    #[test]
    fn usage_report_for_kiro_truthfully_names_missing_token_counts() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "kiro", Some("auto"), false);
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Succeeded,
            usage: TokenUsage::default(),
        });
        let report = app.usage_report().join("\n");
        assert!(
            report.contains("Kiro CLI did not report exact token counts"),
            "{report}"
        );
        assert!(report.contains("does not estimate"), "{report}");
        assert!(!report.contains("quota"), "{report}");
        assert!(!report.contains("billing"), "{report}");
    }

    #[test]
    fn usage_report_never_contains_quota_billing_credits() {
        // Exhaustive: check every adapter scenario.
        for (agent, has_usage) in [
            ("claude", true),
            ("codex", true),
            ("opencode", true),
            ("kiro", true),
            ("antigravity", true),
            ("cmd", false),
            ("grok", false),
        ] {
            let mut app = new_app();
            app.begin_run(RunId::new("r1"), agent, None, false);
            let usage = if has_usage {
                TokenUsage {
                    input: Some(100),
                    output: Some(50),
                    ..Default::default()
                }
            } else {
                TokenUsage::default()
            };
            app.apply_event(RunEventKind::RunFinished {
                status: RunStatus::Succeeded,
                usage,
            });
            let report = app.usage_report().join("\n");
            assert!(!report.contains("quota"), "agent={agent}: {report}");
            assert!(!report.contains("billing"), "agent={agent}: {report}");
            assert!(!report.contains("credits"), "agent={agent}: {report}");
        }
    }

    // --- Regression: Issue 3 — bracketed paste ---

    #[test]
    fn paste_inserts_multiline_text_atomically() {
        let mut app = new_app();
        app.paste("line1\nline2\nline3");
        assert_eq!(app.input, "line1\nline2\nline3");
        assert_eq!(app.cursor, 17); // 17 chars total
        assert_eq!(app.input_line_count(), 3);
    }

    #[test]
    fn paste_normalizes_crlf_to_lf() {
        let mut app = new_app();
        app.paste("hello\r\nworld\rfoo");
        assert_eq!(app.input, "hello\nworld\nfoo");
    }

    #[test]
    fn paste_at_cursor_position_splices_correctly() {
        let mut app = new_app();
        for ch in "hello".chars() {
            app.insert(ch);
        }
        // Move cursor left 2 positions: cursor at "hel|lo"
        app.move_left();
        app.move_left();
        app.paste("WORLD\n");
        assert_eq!(app.input, "helWORLD\nlo");
        // Cursor should be after the pasted text (after the \n).
        assert_eq!(app.cursor, 9); // "helWORLD\n" = 9 chars
    }

    #[test]
    fn paste_does_not_submit() {
        let mut app = new_app();
        app.paste("line1\nline2\nline3");
        // The input should still be in the composer, not submitted.
        assert_eq!(app.input, "line1\nline2\nline3");
        // take_input would consume it (simulating Enter press).
        let taken = app.take_input();
        assert_eq!(taken, "line1\nline2\nline3");
        assert!(app.input.is_empty());
    }

    #[test]
    fn paste_clears_history_cursor() {
        let mut app = new_app();
        // Add history entries.
        for ch in "first message".chars() {
            app.insert(ch);
        }
        app.take_input();
        // Navigate to history.
        app.history_previous();
        assert_eq!(app.input, "first message");
        // Pasting should reset history navigation.
        app.paste(" appended");
        assert!(app.history_cursor.is_none());
    }

    #[test]
    fn large_paste_line_count_is_correct() {
        let mut app = new_app();
        let text = (1..=50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.paste(&text);
        assert_eq!(app.input_line_count(), 50);
    }

    #[test]
    fn paste_preserves_and_counts_a_trailing_newline() {
        let mut app = new_app();
        app.paste(
            "first
second
",
        );
        assert_eq!(
            app.input,
            "first
second
"
        );
        assert_eq!(app.input_line_count(), 3);
        assert_eq!(app.caret_row_column(), (2, 0));
        assert!(app.has_multiline_paste());
    }

    #[test]
    fn paste_empty_string_is_harmless() {
        let mut app = new_app();
        app.paste("");
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn paste_whitespace_only_is_preserved() {
        let mut app = new_app();
        app.paste("\n\n\n");
        assert_eq!(app.input, "\n\n\n");
        assert_eq!(app.cursor, 3);
    }

    // --- Regression: strip_fenced_code_blocks ---

    #[test]
    fn fenced_code_blocks_are_stripped_for_choice_detection() {
        // Bare "choose" inside code should NOT trigger.
        let text = "Here's an example:\n\n```\nchoose one:\n1. alpha\n2. beta\n```";
        let stripped = strip_fenced_code_blocks(text);
        assert!(!stripped.contains("choose"));
    }

    #[test]
    fn text_outside_fences_is_preserved() {
        let text = "Before\n```\ninside\n```\nAfter";
        let stripped = strip_fenced_code_blocks(text);
        assert!(stripped.contains("Before"));
        assert!(stripped.contains("After"));
        assert!(!stripped.contains("inside"));
    }
}

#[cfg(test)]
mod new_feature_tests {
    use super::*;
    use argo_core::event::{RunEventKind, RunStatus, TokenUsage};
    use argo_core::ids::RunId;
    use argo_daemon::protocol::{ConversationSummary, MessageView};

    fn new_app() -> App {
        App::new("/repo")
    }

    fn summary_with_id(id: &str) -> ConversationSummary {
        ConversationSummary {
            id: argo_core::ids::ConversationId::new(id),
            title: Some("test".into()),
            description: None,
            selected_agent_id: Some("codex".into()),
            selected_model: Some("gpt-5".into()),
            selected_reasoning: None,
            selected_mode: None,
            message_count: 2,
            agents_with_sessions: vec![],
            parent_conversation_id: None,
            workspace: Some("/repo".into()),
            updated_at: 42,
        }
    }

    #[test]
    fn usage_reopen_exact_values_from_message_view() {
        let mut app = new_app();
        let usage = TokenUsage {
            input: Some(500),
            output: Some(200),
            cached_input: Some(100),
            reasoning: Some(50),
        };
        app.replace_transcript(vec![
            MessageView {
                id: "m1".into(),
                role: "user".into(),
                text: "hello".into(),
                blocks: vec![],
                agent_id: None,
                model: None,
                usage: None,
                created_at: 1,
            },
            MessageView {
                id: "m2".into(),
                role: "assistant".into(),
                text: "hi".into(),
                blocks: vec![],
                agent_id: Some("claude".into()),
                model: Some("sonnet".into()),
                usage: Some(usage),
                created_at: 2,
            },
        ]);
        assert_eq!(app.last_usage, Some(usage));
        assert_eq!(app.last_usage_source.as_deref(), Some("claude/sonnet"));
    }

    #[test]
    fn usage_reopen_empty_kiro_plain_sets_source_but_no_values() {
        let mut app = new_app();
        // Kiro/plain adapters report empty usage on success.
        let empty_usage = TokenUsage::default();
        app.replace_transcript(vec![
            MessageView {
                id: "m1".into(),
                role: "user".into(),
                text: "do it".into(),
                blocks: vec![],
                agent_id: None,
                model: None,
                usage: None,
                created_at: 1,
            },
            MessageView {
                id: "m2".into(),
                role: "assistant".into(),
                text: "done".into(),
                blocks: vec![],
                agent_id: Some("kiro".into()),
                model: Some("default".into()),
                usage: Some(empty_usage),
                created_at: 2,
            },
        ]);
        // Source is set (the turn succeeded), but last_usage is None because
        // the usage was empty.
        assert_eq!(app.last_usage_source.as_deref(), Some("kiro/default"));
        assert_eq!(app.last_usage, None);
    }

    #[test]
    fn usage_no_completed_turn_shows_appropriate_message() {
        let app = new_app();
        let report = app.usage_report();
        assert!(
            report
                .iter()
                .any(|l| l.contains("No successful completed turn")),
            "should say no completed turn: {:?}",
            report
        );
        // Must never contain the old problematic phrasing.
        let joined = report.join(" ");
        assert!(
            !joined.contains("no completed turn did not report"),
            "problematic phrasing: {joined}"
        );
    }

    #[test]
    fn usage_cancellation_preserves_previous_successful_usage() {
        let mut app = new_app();
        // First: a successful turn sets usage.
        app.begin_run(RunId::new("r1"), "codex", Some("gpt-5"), false);
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Succeeded,
            usage: TokenUsage {
                input: Some(100),
                output: Some(50),
                ..Default::default()
            },
        });
        assert_eq!(app.last_usage.unwrap().input, Some(100));

        // Second: a cancelled turn should NOT clear the previous usage.
        app.begin_run(RunId::new("r2"), "codex", Some("gpt-5"), false);
        app.apply_event(RunEventKind::RunFinished {
            status: RunStatus::Cancelled,
            usage: TokenUsage::default(),
        });
        // Previous successful usage is preserved.
        assert_eq!(app.last_usage.unwrap().input, Some(100));
        assert_eq!(app.last_usage_source.as_deref(), Some("codex/gpt-5"));
    }

    #[test]
    fn new_conversation_clears_usage() {
        let mut app = new_app();
        app.last_usage = Some(TokenUsage {
            input: Some(999),
            ..Default::default()
        });
        app.last_usage_source = Some("old/model".into());
        // replace_transcript with empty messages simulates new conversation.
        app.replace_transcript(vec![]);
        assert_eq!(app.last_usage, None);
        assert_eq!(app.last_usage_source, None);
    }

    #[test]
    fn farewell_format_is_exactly_one_line() {
        let mut app = new_app();
        app.conversation = Some(summary_with_id("abc-def-ghi"));
        app.push(LineKind::User, "hello".to_string());
        let msg = farewell(&app).expect("farewell");
        assert_eq!(msg.matches('\n').count(), 0, "must be one line: {msg}");
        assert!(msg.starts_with("argo --resume "), "format: {msg}");
        assert!(msg.contains("abc-def-ghi"), "full id: {msg}");
    }

    #[test]
    fn plain_text_adapter_shows_no_streaming_notice() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "grok", None, false);
        let last_notice = app.lines.iter().rev().find(|l| l.kind == LineKind::Notice);
        assert!(
            last_notice
                .map(|l| l.text.contains("final output only"))
                .unwrap_or(false),
            "plain adapter should show 'final output only' notice: {:?}",
            app.lines
        );
    }

    #[test]
    fn structured_adapter_does_not_show_no_streaming_notice() {
        let mut app = new_app();
        app.begin_run(RunId::new("r1"), "claude", None, false);
        let has_notice = app
            .lines
            .iter()
            .any(|l| l.kind == LineKind::Notice && l.text.contains("final output only"));
        assert!(!has_notice, "structured adapter should NOT show the notice");
    }

    #[test]
    fn word_and_line_deletion_are_unicode_safe_and_undoable() {
        let mut app = new_app();
        app.paste("alpha café\nsecond line");
        app.backspace_word();
        assert_eq!(app.input, "alpha café\nsecond ");
        assert!(app.undo_edit());
        assert_eq!(app.input, "alpha café\nsecond line");

        app.backspace_to_line_start();
        assert_eq!(app.input, "alpha café\n");
        assert!(app.undo_edit());
        assert_eq!(app.input, "alpha café\nsecond line");
    }

    #[test]
    fn busy_shortcut_tips_rotate_slowly() {
        let mut app = new_app();
        app.begin_run(RunId::new("tip-run"), "codex", Some("default"), false);
        let initial = app.shortcut_tip();
        for _ in 0..32 {
            app.advance_tick();
        }
        assert_eq!(app.shortcut_tip(), initial);
        app.advance_tick();
        assert_ne!(app.shortcut_tip(), initial);
    }

    #[test]
    fn thinking_visibility_does_not_delete_reasoning() {
        let mut app = new_app();
        app.push(LineKind::Thinking, "retained reasoning");
        app.scroll_back = 12;
        app.set_thinking_visible(false);
        assert!(!app.thinking_visible);
        assert_eq!(app.scroll_back, 0, "toggle must return to the live tail");
        assert!(app
            .lines
            .iter()
            .any(|line| { line.kind == LineKind::Thinking && line.text == "retained reasoning" }));
        app.set_thinking_visible(true);
        assert!(app.thinking_visible);
    }
}
