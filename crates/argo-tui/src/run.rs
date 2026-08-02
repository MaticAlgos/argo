//! The TUI event loop.
//!
//! Terminal input, daemon replies, and streamed run events are multiplexed in one
//! `select!`. The terminal is restored on every exit path, including a panic:
//! leaving a user's shell in raw mode with no cursor is worse than any error
//! message.

use crate::app::{
    App, EnterAction, InputAction, LineKind, McpDraft, MouseSelection, PickerAction, ScreenPoint,
};
use crate::commands::{self, Command};
use argo_core::error::{ArgoError, Result};
use argo_core::event::RunEvent;
use argo_core::ids::ConversationId;
use argo_core::{ArgoPaths, IPC_PROTOCOL_VERSION};
use argo_daemon::protocol::{Request, Response};
use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::{
    Attribute, Color as CrosstermColor, Print, SetAttribute, SetForegroundColor,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Button/drag/wheel reporting plus unambiguous SGR coordinates.
///
/// Argo renders and copies the selected range itself, so wheel scrolling and text
/// selection no longer depend on a terminal-specific Shift-drag bypass. F2 still
/// disables reporting for fully native selection.
const ENABLE_MOUSE_WHEEL: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const DISABLE_MOUSE_WHEEL: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

#[derive(Debug, Default, Clone)]
struct ScreenSnapshot {
    area: ratatui::layout::Rect,
    cells: Vec<Vec<String>>,
}

#[derive(Debug)]
enum McpAuthEvent {
    Progress { name: String, message: String },
    Complete { name: String },
    Failed { name: String, message: String },
}

impl ScreenSnapshot {
    fn capture(buffer: &ratatui::buffer::Buffer) -> Self {
        let area = buffer.area;
        let cells = (area.top()..area.bottom())
            .map(|row| {
                (area.left()..area.right())
                    .map(|column| buffer[(column, row)].symbol().to_string())
                    .collect()
            })
            .collect();
        Self { area, cells }
    }

    fn selected_text(&self, selection: MouseSelection) -> Option<String> {
        if self.area.is_empty() || selection.is_click() {
            return None;
        }
        let (mut start, mut end) = selection.ordered();
        start.row = start
            .row
            .clamp(self.area.top(), self.area.bottom().saturating_sub(1));
        end.row = end
            .row
            .clamp(self.area.top(), self.area.bottom().saturating_sub(1));
        start.column = start
            .column
            .clamp(self.area.left(), self.area.right().saturating_sub(1));
        end.column = end
            .column
            .clamp(self.area.left(), self.area.right().saturating_sub(1));

        let mut rows = Vec::new();
        for row in start.row..=end.row {
            let left = if row == start.row {
                start.column
            } else {
                self.area.left()
            };
            let right = if row == end.row {
                end.column
            } else {
                self.area.right().saturating_sub(1)
            };
            let row_index = usize::from(row.saturating_sub(self.area.top()));
            let Some(cells) = self.cells.get(row_index) else {
                continue;
            };
            let from = usize::from(left.saturating_sub(self.area.left()));
            let through = usize::from(right.saturating_sub(self.area.left()));
            let line = cells
                .get(from..=through)
                .unwrap_or_default()
                .concat()
                .trim_end()
                .to_string();
            rows.push(line);
        }
        let text = rows.join("\n").trim_matches('\n').to_string();
        (!text.is_empty()).then_some(text)
    }
}

fn set_mouse_wheel_reporting<W: std::io::Write>(
    writer: &mut W,
    enabled: bool,
) -> std::io::Result<()> {
    crossterm::execute!(
        writer,
        Print(if enabled {
            ENABLE_MOUSE_WHEEL
        } else {
            DISABLE_MOUSE_WHEEL
        })
    )
}

fn set_mouse_scroll_mode<W: std::io::Write>(
    writer: &mut W,
    app: &mut App,
    enabled: bool,
) -> std::io::Result<()> {
    set_mouse_wheel_reporting(writer, enabled)?;
    app.mouse_scroll_mode = enabled;
    app.clear_mouse_selection();
    app.set_status(if enabled {
        "mouse ready · wheel scrolls · drag selects + copies · F2 native mode"
    } else {
        "native selection enabled · F2 restores wheel + drag selection"
    });
    Ok(())
}

fn enter_terminal_screen<W: std::io::Write>(writer: &mut W) -> std::io::Result<()> {
    crossterm::execute!(
        writer,
        EnterAlternateScreen,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
        )
    )
}

fn leave_terminal_screen<W: std::io::Write>(writer: &mut W) -> std::io::Result<()> {
    crossterm::execute!(
        writer,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        Print(DISABLE_MOUSE_WHEEL),
        LeaveAlternateScreen
    )
}

/// Restores the terminal, tolerating already-restored state.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = leave_terminal_screen(&mut std::io::stdout());
    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
}

/// Restores terminal state if setup or the event loop exits early.
struct TerminalRestoreGuard(bool);

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.0 {
            restore_terminal();
        }
    }
}

/// Runs the TUI against the daemon.
pub async fn run(paths: &ArgoPaths, workspace: String) -> Result<()> {
    run_inner(paths, workspace, None).await
}

/// Runs the TUI, resuming a specific conversation by its full id.
///
/// The TUI uses the conversation's authoritative workspace rather than requiring
/// it from the caller.
pub async fn run_with_conversation(paths: &ArgoPaths, conversation_id: String) -> Result<()> {
    run_inner(
        paths,
        // Temporary placeholder; overridden once the conversation is loaded.
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .to_string_lossy()
            .to_string(),
        Some(conversation_id),
    )
    .await
}

/// Internal run implementation with optional initial conversation.
async fn run_inner(
    paths: &ArgoPaths,
    workspace: String,
    initial_conversation: Option<String>,
) -> Result<()> {
    let mut connection = Connection::connect(paths).await?;
    let mut app = App::new(workspace.clone());

    // A direct resume resolves the conversation first so its authoritative
    // workspace is opened even when the command runs from another directory.
    if let Some(conversation_id) = initial_conversation {
        let requested = ConversationId::new(conversation_id);
        load_conversation(&mut connection, &mut app, &requested).await?;
        if app.conversation.as_ref().map(|summary| &summary.id) != Some(&requested) {
            return Err(ArgoError::not_found("conversation", requested.as_str()));
        }
    }

    match connection
        .request(Request::OpenWorkspace {
            root: app.workspace.clone(),
        })
        .await?
    {
        Response::Workspace {
            root,
            conversations,
        } => {
            app.workspace = root;
            app.set_conversation_summaries(conversations);
        }
        Response::Error {
            code,
            message,
            retryable,
        } => return Err(ArgoError::remote(code, message, retryable)),
        other => return Err(ArgoError::Protocol(format!("unexpected reply: {other:?}"))),
    }

    if let Response::Agents { agents } = connection
        .request(Request::ListAgents { refresh: false })
        .await?
    {
        app.agents = agents;
    }

    // The opening picker promises the user's installed CLI versions. Keep the
    // daemon's startup inventory lightweight, then run only the cheap `--version`
    // command for every available adapter concurrently. Full model/auth probing
    // remains deferred until the user chooses a CLI.
    let versions =
        futures::future::join_all(app.agents.iter().map(argo_runtime::detect_version)).await;
    for (agent, version) in app.agents.iter_mut().zip(versions) {
        if version.is_some() {
            agent.version = version;
        }
    }

    match crate::preferences::load(paths) {
        Ok(selection) => app.default_selection = selection,
        Err(error) => app.set_status(format!("startup default ignored: {error}")),
    }
    if let Some(missing) = clear_default_if_agent_missing(paths, &mut app)? {
        app.set_status(format!(
            "saved default {missing} is no longer detected · default cleared"
        ));
    }

    if app.conversation.is_none() {
        // Default launch remains a fresh conversation; resume is always explicit.
        new_conversation(&mut connection, &mut app, paths, None).await?;
    }

    // Any panic must not leave the terminal unusable.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let mut terminal_guard = TerminalRestoreGuard(true);
    enable_raw_mode().map_err(|e| ArgoError::Io(format!("enable raw mode: {e}")))?;
    let mut stdout = std::io::stdout();
    // Combined mode keeps wheel scrolling and lets Argo own drag selection. F2
    // remains a fully terminal-native selection escape hatch.
    enter_terminal_screen(&mut stdout)
        .map_err(|e| ArgoError::Io(format!("enter alternate screen: {e}")))?;
    set_mouse_scroll_mode(&mut stdout, &mut app, true)
        .map_err(|e| ArgoError::Io(format!("enable combined mouse mode: {e}")))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal =
        Terminal::new(backend).map_err(|e| ArgoError::Io(format!("create terminal: {e}")))?;

    let result = event_loop(&mut terminal, &mut connection, &mut app, paths).await;
    let update_on_exit = app.update_on_exit;

    restore_terminal();
    terminal_guard.0 = false;
    let _ = terminal.show_cursor();

    if result.is_ok() {
        if let Some(force) = update_on_exit {
            return update_after_exit(force).await;
        }
    }

    // The alternate screen is gone by now, so the transcript is no longer visible.
    // Leaving the id behind is the difference between a session the user can return
    // to and one they have to hunt for.
    if let Some(farewell) = crate::app::farewell(&app) {
        println!("{farewell}");
    }
    result
}

/// The main loop.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
) -> Result<()> {
    let mut keys = EventStream::new();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<RunEvent>();
    let (auth_tx, mut auth_rx) = mpsc::unbounded_channel::<McpAuthEvent>();
    let (update_tx, mut update_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _ = update_tx.send(argo_runtime::update::check().await);
    });
    let mut screen = ScreenSnapshot::default();

    loop {
        let mut hyperlinks = Vec::new();
        terminal
            .draw(|frame| {
                crate::render::draw(frame, app);
                hyperlinks = crate::render::native_hyperlinks(frame.buffer_mut(), app);
                screen = ScreenSnapshot::capture(frame.buffer_mut());
            })
            .map_err(|e| ArgoError::Io(format!("draw: {e}")))?;
        write_native_hyperlinks(&mut std::io::stdout(), &hyperlinks)
            .map_err(|e| ArgoError::Io(format!("draw hyperlinks: {e}")))?;

        if app.should_quit {
            return Ok(());
        }

        // Only tick while something is running: an idle TUI should not redraw.
        let animation = async {
            if app.is_busy() {
                tokio::time::sleep(std::time::Duration::from_millis(90)).await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        tokio::select! {
            Some(update) = update_rx.recv() => {
                if let Ok(status) = update {
                    if status.available() {
                        app.available_update = Some(status.latest.to_string());
                        app.set_status(format!(
                            "Argo v{} is available · /update to review",
                            status.latest
                        ));
                    }
                }
            }
            Some(first) = event_rx.recv() => {
                // ACP adapters may emit one event per token. Drain the ready burst
                // before redrawing so a 600-token answer does not trigger 600
                // complete Markdown/layout passes and appear frozen.
                let batch = ready_event_batch(first, &mut event_rx, 512);
                for event in batch {
                    apply_stream_event(connection, app, paths, &event_tx, event).await?;
                }
            }
            Some(auth) = auth_rx.recv() => {
                match auth {
                    McpAuthEvent::Progress { name, message } => {
                        if !app.append_text_overlay("mcp authentication", message.clone()) {
                            app.push(LineKind::Notice, format!("· MCP {name}: {message}"));
                        }
                    }
                    McpAuthEvent::Complete { name } => {
                        let message = format!("authenticated '{name}' · /mcp check {name}");
                        if !app.append_text_overlay("mcp authentication", message.clone()) {
                            app.push(LineKind::Notice, format!("✓ {message}"));
                        }
                        app.set_status(format!("MCP authentication complete · {name}"));
                    }
                    McpAuthEvent::Failed { name, message } => {
                        let detail = format!("authentication failed for '{name}': {message}");
                        if !app.append_text_overlay("mcp authentication", detail.clone()) {
                            app.push(LineKind::Error, format!("! {detail}"));
                        }
                        app.report_error(detail);
                    }
                }
            }
            _ = animation => {
                app.advance_tick();
            }
            maybe_key = keys.next() => {
                match maybe_key {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if key.code == KeyCode::F(2) {
                            let enabled = !app.mouse_scroll_mode;
                            set_mouse_scroll_mode(&mut std::io::stdout(), app, enabled)
                                .map_err(|e| ArgoError::Io(format!("toggle mouse wheel: {e}")))?;
                        } else {
                            handle_key(key, connection, app, paths, &event_tx, &auth_tx).await?;
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        if let Some(url) = handle_mouse(mouse, app, &hyperlinks, &screen) {
                            match open_web_url(&url) {
                                Ok(()) => app.set_status(format!("opened {url}")),
                                Err(error) => app.report_error(error),
                            }
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        handle_paste(app, &text);
                    }
                    Some(Ok(_)) => {}
                    // Terminal closed or errored: exit rather than spin.
                    Some(Err(error)) => {
                        return Err(ArgoError::Io(format!("terminal input: {error}")));
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

/// Collects an already-ready burst so token-sized ACP events share one redraw.
fn ready_event_batch(
    first: RunEvent,
    receiver: &mut mpsc::UnboundedReceiver<RunEvent>,
    limit: usize,
) -> Vec<RunEvent> {
    let mut batch = Vec::with_capacity(limit.clamp(1, 64));
    batch.push(first);
    while batch.len() < limit.max(1) {
        match receiver.try_recv() {
            Ok(event) => batch.push(event),
            Err(_) => break,
        }
    }
    batch
}

/// Applies one streamed event and performs terminal-state bookkeeping.
async fn apply_stream_event(
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    event_tx: &mpsc::UnboundedSender<RunEvent>,
    event: RunEvent,
) -> Result<()> {
    let belongs_to_parent = app.active_run.as_ref() == Some(&event.run_id);
    let child_to_follow = match &event.kind {
        argo_core::event::RunEventKind::ChildSpawned {
            child_run_id,
            child_agent_id,
            native: false,
            ..
        } => Some((child_run_id.clone(), child_agent_id.to_string())),
        _ => None,
    };
    if let Some((child_run_id, child_agent_id)) = child_to_follow {
        if app.follow_child(child_run_id.clone(), child_agent_id) {
            spawn_stream(paths, child_run_id, event_tx.clone());
        }
    }

    if !belongs_to_parent {
        app.apply_child_event(event.run_id, event.kind);
        return Ok(());
    }

    let terminal_status = match &event.kind {
        argo_core::event::RunEventKind::RunFinished { status, .. } => Some(*status),
        _ => None,
    };
    app.apply_event(event.kind);

    if let Some(status) = terminal_status {
        let offer_options =
            status == argo_core::event::RunStatus::Succeeded && app.queue_depth() == 0;
        refresh_conversation_summary(connection, app).await?;
        if should_drain_queue(status) {
            try_start_next_queued(connection, app, paths, event_tx).await?;
            if offer_options && !app.is_busy() {
                app.open_latest_response_options();
            }
        } else if app.queue_depth() > 0 {
            app.set_status(format!(
                "{} queued · paused after {status:?} · Enter retries · Esc discards",
                app.queue_depth()
            ));
        }
    }
    Ok(())
}

fn write_native_hyperlinks<W: std::io::Write>(
    writer: &mut W,
    hyperlinks: &[crate::render::NativeHyperlink],
) -> std::io::Result<()> {
    if hyperlinks.is_empty() {
        return Ok(());
    }

    crossterm::queue!(
        writer,
        SavePosition,
        SetForegroundColor(CrosstermColor::Rgb {
            r: 105,
            g: 190,
            b: 255,
        }),
        SetAttribute(Attribute::Underlined)
    )?;
    for hyperlink in hyperlinks {
        let osc8 = format!(
            "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\",
            hyperlink.url, hyperlink.text
        );
        crossterm::queue!(writer, MoveTo(hyperlink.column, hyperlink.row), Print(osc8))?;
    }
    crossterm::queue!(
        writer,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(CrosstermColor::Reset),
        RestorePosition
    )?;
    writer.flush()
}

fn is_safe_web_url(url: &str) -> bool {
    let remainder = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    remainder.is_some_and(|value| {
        !value.is_empty()
            && !value
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
    })
}

fn hyperlink_at(
    hyperlinks: &[crate::render::NativeHyperlink],
    column: u16,
    row: u16,
) -> Option<String> {
    hyperlinks
        .iter()
        .find(|link| {
            let width = link.text.chars().count().min(u16::MAX as usize) as u16;
            link.row == row
                && column >= link.column
                && column < link.column.saturating_add(width)
                && is_safe_web_url(&link.url)
        })
        .map(|link| link.url.clone())
}

fn open_web_url(url: &str) -> std::result::Result<(), String> {
    if !is_safe_web_url(url) {
        return Err("refusing to open a non-HTTP(S) link".into());
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open link: {error}"))
}

/// Applies wheel movement, Argo-owned drag selection, and safe link clicks.
fn handle_mouse(
    mouse: MouseEvent,
    app: &mut App,
    hyperlinks: &[crate::render::NativeHyperlink],
    screen: &ScreenSnapshot,
) -> Option<String> {
    const ROWS_PER_NOTCH: i32 = 3;
    let delta = match mouse.kind {
        MouseEventKind::ScrollUp => -ROWS_PER_NOTCH,
        MouseEventKind::ScrollDown => ROWS_PER_NOTCH,
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            app.begin_mouse_selection(mouse.column, mouse.row);
            return None;
        }
        MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
            app.update_mouse_selection(mouse.column, mouse.row);
            app.set_status("selecting visible text · release to copy");
            return None;
        }
        MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
            app.update_mouse_selection(mouse.column, mouse.row);
            let selection = app.mouse_selection.unwrap_or(MouseSelection {
                anchor: ScreenPoint {
                    column: mouse.column,
                    row: mouse.row,
                },
                focus: ScreenPoint {
                    column: mouse.column,
                    row: mouse.row,
                },
                dragging: false,
            });
            if selection.is_click() {
                app.clear_mouse_selection();
                return hyperlink_at(hyperlinks, mouse.column, mouse.row);
            }
            let text = screen.selected_text(selection);
            let count = text
                .as_deref()
                .map(|value| value.chars().count())
                .unwrap_or(0);
            app.finish_mouse_selection(mouse.column, mouse.row, text);
            copy_latest_response(app);
            if app.status == "copied selected text" {
                app.set_status(format!("selected and copied {count} characters"));
            }
            return None;
        }
        _ => return None,
    };
    app.clear_mouse_selection();
    if app.has_overlay() {
        app.overlay_move(delta);
    } else if delta < 0 {
        app.scroll_up((-delta) as usize);
    } else {
        app.scroll_down(delta as usize);
    }
    None
}

/// Moves through command completions or submitted user prompts.
fn navigate_vertical(app: &mut App, previous: bool) {
    if app.has_completions() {
        app.completion_move(if previous { -1 } else { 1 });
    } else if previous {
        app.history_previous();
    } else {
        app.history_next();
    }
}

/// Handles a bracketed paste event.
///
/// When an overlay picker is open, a single-line paste extends the picker filter.
/// Multi-line pastes into a picker are ignored (the picker is for navigation, not
/// bulk input). When no overlay is open, the full text is inserted atomically.
fn handle_paste(app: &mut App, text: &str) {
    if matches!(app.overlay, crate::app::Overlay::Input { .. }) {
        app.overlay_input_push_str(text.trim_end_matches(['\r', '\n']));
        return;
    }
    if app.has_overlay() {
        // Single-line paste into a picker filter is useful (e.g., pasting a model name).
        if !text.contains('\n') && !text.contains('\r') {
            for ch in text.chars() {
                app.picker_filter_push(ch);
            }
        }
        // Multi-line paste into a picker is ignored.
        return;
    }
    app.paste(text);
}

/// Handles one key press.
async fn handle_key(
    key: crossterm::event::KeyEvent,
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    event_tx: &mpsc::UnboundedSender<RunEvent>,
    auth_tx: &mpsc::UnboundedSender<McpAuthEvent>,
) -> Result<()> {
    let plain_ctrl_c = key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::SUPER);
    if plain_ctrl_c {
        app.request_ctrl_c_exit();
        return Ok(());
    }
    app.clear_ctrl_c_exit_confirmation();

    if matches!(app.overlay, crate::app::Overlay::Input { .. }) {
        match key.code {
            KeyCode::Esc => {
                app.close_overlay();
                app.mcp_draft = None;
                app.set_status("MCP setup cancelled");
            }
            KeyCode::Backspace => app.overlay_input_pop(),
            KeyCode::Enter => {
                if let Some((action, value)) = app.overlay_submit_input() {
                    apply_mcp_input(app, paths, auth_tx, action, value)?;
                }
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                app.overlay_input_push_str(&ch.to_string());
            }
            _ => {}
        }
        return Ok(());
    }

    // Overlays capture navigation keys while open.
    if app.has_overlay() {
        match key.code {
            KeyCode::Esc => app.close_overlay(),
            KeyCode::Up => app.overlay_move(-1),
            KeyCode::Down => app.overlay_move(1),
            KeyCode::PageUp => app.overlay_move(-10),
            KeyCode::PageDown => app.overlay_move(10),
            KeyCode::Enter => match app.overlay_choose() {
                Some((action, value)) => {
                    if action == PickerAction::StartupAgent {
                        app.startup_save_default = false;
                    }
                    apply_choice(connection, app, paths, event_tx, auth_tx, action, value).await?
                }
                // A read-only pane has nothing to choose, so Enter dismisses it
                // rather than appearing to do nothing.
                None => app.close_overlay(),
            },
            // Typing narrows a picker, which is the only practical way through a
            // list of several hundred models.
            KeyCode::Backspace => app.picker_filter_pop(),
            KeyCode::Char(' ') => {
                let space_action = match &app.overlay {
                    crate::app::Overlay::Picker {
                        action: PickerAction::StartupAgent,
                        ..
                    } => Some(PickerAction::StartupAgent),
                    crate::app::Overlay::Picker {
                        action: PickerAction::Agents,
                        ..
                    } => Some(PickerAction::DefaultAgent),
                    _ => None,
                };
                if let Some(space_action) = space_action {
                    if space_action == PickerAction::StartupAgent {
                        app.startup_save_default = true;
                    }
                    if let Some((_, value)) = app.overlay_choose() {
                        apply_choice(
                            connection,
                            app,
                            paths,
                            event_tx,
                            auth_tx,
                            space_action,
                            value,
                        )
                        .await?;
                    }
                } else {
                    app.picker_filter_push(' ');
                }
            }
            KeyCode::Delete
                if matches!(
                    &app.overlay,
                    crate::app::Overlay::Picker {
                        action: PickerAction::Agents,
                        ..
                    }
                ) =>
            {
                crate::preferences::save(paths, None)?;
                app.default_selection = None;
                app.set_status("startup default cleared · new chats will ask for a CLI");
                open_agents_picker(app);
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                app.picker_filter_push(ch)
            }
            _ => {}
        }
        return Ok(());
    }

    if is_mode_cycle_key(&key) {
        cycle_mode(connection, app).await?;
        return Ok(());
    }

    if is_multiline_enter(&key) {
        app.insert_newline();
        return Ok(());
    }

    match key.code {
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.input.is_empty() {
                app.should_quit = true;
            }
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.undo_edit() {
                app.set_status("restored previous composer edit");
            }
        }
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.set_thinking_visible(!app.thinking_visible);
        }
        KeyCode::Char('c')
            if key.modifiers.contains(KeyModifiers::SUPER)
                || (key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT)) =>
        {
            copy_latest_response(app);
        }
        KeyCode::Esc => {
            if app.has_completions() {
                // Dismiss the list without abandoning what was typed.
                app.completions.clear();
            } else if app.is_busy() {
                // Cancellation retains queued follow-ups; the terminal cancelled
                // event advances immediately to the next FIFO item.
                cancel_active(connection, app).await?;
                if app.queue_depth() > 0 {
                    app.set_status(format!(
                        "cancelling · {} queued message(s) will continue",
                        app.queue_depth()
                    ));
                }
            } else {
                let retry_dropped = usize::from(app.clear_retry_prompt());
                let dropped = app.clear_queue() + retry_dropped;
                if dropped > 0 {
                    app.push(
                        LineKind::Notice,
                        format!("discarded {dropped} paused/queued message(s)"),
                    );
                }
            }
        }
        // Ctrl+J remains a portable fallback for terminals that cannot preserve
        // modified Enter at all.
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.insert_newline();
        }
        KeyCode::Enter => match app.enter_action() {
            // Only pick from the list when that is clearly what the user is doing;
            // otherwise Enter would never run a fully typed command.
            EnterAction::AcceptCompletion => {
                app.accept_completion();
            }
            EnterAction::Submit => {
                let line = app.take_input();
                if line.trim().is_empty() {
                    // Failed turns pause the queue. Enter on an empty composer
                    // explicitly retries; cancellations advance automatically.
                    if !app.is_busy() {
                        if app.retry_prompt().is_some() {
                            try_retry_prompt(connection, app, paths, event_tx).await?;
                        } else if app.queue_depth() > 0 {
                            try_start_next_queued(connection, app, paths, event_tx).await?;
                        }
                    }
                    return Ok(());
                }
                submit(connection, app, paths, line, event_tx, auth_tx).await?;
            }
        },
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::SUPER) => {
            app.backspace_to_line_start()
        }
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => app.backspace_word(),
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => app.backspace_word(),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.backspace_to_line_start()
        }
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) || app.input.is_empty() => {
            app.scroll_up(usize::MAX)
        }
        KeyCode::End
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || (app.input.is_empty() && app.scroll_back > 0) =>
        {
            app.scroll_down(usize::MAX)
        }
        KeyCode::Home => app.move_home(),
        KeyCode::End => app.move_end(),
        // Ctrl+P/N remain aliases for composer-history navigation.
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.history_previous()
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => app.history_next(),
        KeyCode::Up => navigate_vertical(app, true),
        KeyCode::Down => navigate_vertical(app, false),
        KeyCode::PageUp => app.scroll_up(10),
        KeyCode::PageDown => app.scroll_down(10),
        KeyCode::Tab => {
            // Accept the highlighted suggestion; the popup already shows it.
            app.accept_completion();
        }
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            app.insert(ch)
        }
        _ => {}
    }
    Ok(())
}

/// True when an Enter-like event requests a composer line break.
fn is_multiline_enter(key: &crossterm::event::KeyEvent) -> bool {
    if !matches!(
        key.code,
        KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n')
    ) {
        return false;
    }
    is_multiline_enter_with_native_shift(key, native_shift_key_is_pressed())
}

fn is_multiline_enter_with_native_shift(
    key: &crossterm::event::KeyEvent,
    native_shift_pressed: bool,
) -> bool {
    let enter = matches!(
        key.code,
        KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n')
    );
    enter
        && (key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
            || native_shift_pressed)
}

/// Apple Terminal collapses Shift+Enter to the same `\r` byte as plain Enter.
/// Querying the current Quartz modifier flags recovers the information while the
/// key chord is held, without installing an event tap or requesting input access.
#[cfg(target_os = "macos")]
fn native_shift_key_is_pressed() -> bool {
    const COMBINED_SESSION_STATE: i32 = 0;
    const SHIFT_MASK: u64 = 0x0002_0000;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceFlagsState(state_id: i32) -> u64;
    }

    // SAFETY: this CoreGraphics query has no pointer arguments and is available
    // on every supported macOS release (10.4+).
    unsafe { CGEventSourceFlagsState(COMBINED_SESSION_STATE) & SHIFT_MASK != 0 }
}

#[cfg(not(target_os = "macos"))]
const fn native_shift_key_is_pressed() -> bool {
    false
}

/// Recognizes both legacy CSI-Z and enhanced-keyboard Shift+Tab encodings.
fn is_mode_cycle_key(key: &crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::BackTab)
        || (key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Tab | KeyCode::Char('\t')))
}

/// Copies an Argo-selected visible range, or the latest response as a fallback.
fn copy_latest_response(app: &mut App) {
    let selected = app.selected_screen_text().map(str::to_string);
    let text = selected.clone().or_else(|| {
        app.lines
            .iter()
            .rev()
            .find(|line| line.kind == LineKind::Assistant)
            .map(|line| line.text.clone())
    });
    let Some(text) = text else {
        app.set_status("nothing to copy yet");
        return;
    };

    let copier = if cfg!(target_os = "macos") {
        "pbcopy"
    } else {
        "wl-copy"
    };
    let result = std::process::Command::new(copier)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(text.as_bytes())?;
            }
            child.wait().map(|_| ())
        });
    match result {
        Ok(()) if selected.is_some() => app.set_status("copied selected text"),
        Ok(()) => app.set_status("copied latest response"),
        Err(error) => app.report_error(format!("copy failed: {error}")),
    }
}

/// Routes a submitted line to a command or a message.
async fn submit(
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    line: String,
    event_tx: &mpsc::UnboundedSender<RunEvent>,
    auth_tx: &mpsc::UnboundedSender<McpAuthEvent>,
) -> Result<()> {
    let line = if commands::is_command(&line) {
        match commands::parse(&line) {
            Ok(command) => match run_command(connection, app, paths, auth_tx, command).await? {
                // A command may queue a message, as `/delegate` does.
                Some(followup) => followup,
                None => return Ok(()),
            },
            Err(error) => {
                app.report_error(error.to_string());
                return Ok(());
            }
        }
    } else {
        line
    };

    if app.is_busy() {
        // Queue rather than discard: a follow-up typed mid-turn is exactly what a
        // user does not want to retype, and blocking the composer would be worse.
        let depth = app.enqueue(line);
        app.push(
            LineKind::Notice,
            format!("queued ({depth} waiting) — sends when this turn succeeds"),
        );
        return Ok(());
    }

    // A retryable failed prompt has priority over newly typed text. Preserve the
    // new text behind it and retry the failed turn first, keeping FIFO semantics.
    if app.retry_prompt().is_some() {
        app.enqueue(line);
        return try_retry_prompt(connection, app, paths, event_tx).await;
    }

    // A queue retained after a non-retryable error has priority over newly typed
    // text. Append the new text and restart the oldest item.
    if app.queue_depth() > 0 {
        app.enqueue(line);
        return try_start_next_queued(connection, app, paths, event_tx).await;
    }

    let _accepted = send_message(connection, app, paths, line, event_tx).await?;
    Ok(())
}

/// Sends one already-parsed user message.
///
/// Returns true only after the daemon accepted it and supplied a run id. The user
/// line is added to the transcript at that point, so a rejected send cannot create
/// a ghost message that never entered canonical history.
async fn send_message(
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    line: String,
    event_tx: &mpsc::UnboundedSender<RunEvent>,
) -> Result<bool> {
    let Some(conversation) = app.conversation.as_ref().map(|c| c.id.clone()) else {
        app.report_error("no conversation is open; use /new");
        return Ok(false);
    };

    match connection
        .request(Request::SendMessage {
            conversation_id: conversation,
            prompt: line.clone(),
        })
        .await?
    {
        Response::RunStarted {
            run_id,
            agent_id,
            model,
            resumed,
            context_transfer_reason,
            conversation: authoritative_summary,
        } => {
            if let Some(summary) = authoritative_summary {
                app.set_conversation_summary(summary);
            } else if let Some(mut summary) = app.conversation.clone() {
                // Compatibility with a daemon from before RunStarted carried the
                // authoritative summary. Keep both header and history cache fresh.
                if summary
                    .title
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default()
                    .is_empty()
                {
                    summary.title = Some(argo_core::conversation_title(&line));
                }
                summary.selected_agent_id = Some(agent_id.clone());
                summary.selected_model = model.clone();
                summary.message_count += 2; // accepted user + assistant placeholder
                summary.updated_at = argo_core::now_millis();
                app.set_conversation_summary(summary);
            }
            app.track_active_prompt(line.clone());
            app.push(LineKind::User, line);
            app.begin_run_with_reason(
                run_id.clone(),
                &agent_id,
                model.as_deref(),
                resumed,
                context_transfer_reason.as_deref(),
            );
            // Streaming runs on its own connection so the request channel stays
            // free for commands issued mid-turn.
            spawn_stream(paths, run_id, event_tx.clone());
            Ok(true)
        }
        Response::Error { message, .. } => {
            app.report_error(message);
            Ok(false)
        }
        other => {
            app.report_error(format!("unexpected reply: {other:?}"));
            Ok(false)
        }
    }
}

/// Retries the failed active prompt with the same two-phase acknowledgement used
/// by the FIFO queue. The prompt is removed only after `RunStarted`.
async fn try_retry_prompt(
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    event_tx: &mpsc::UnboundedSender<RunEvent>,
) -> Result<()> {
    if app.is_busy() {
        return Ok(());
    }
    let Some(prompt) = app.retry_prompt().map(str::to_string) else {
        return Ok(());
    };

    match send_message(connection, app, paths, prompt, event_tx).await {
        Ok(true) => {
            let _ = app.commit_retry_prompt();
            app.set_status("retry started");
        }
        Ok(false) => app.set_status("retry rejected · Enter retries · Esc discards"),
        Err(error) => {
            app.set_status("daemon error · Enter retries · Esc discards");
            return Err(error);
        }
    }
    Ok(())
}

/// Starts the oldest queued message using a two-phase peek/commit protocol.
///
/// The item stays in the queue until `send_message` receives `RunStarted`, so an
/// IPC failure, validation error, or daemon restart cannot silently lose it.
async fn try_start_next_queued(
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    event_tx: &mpsc::UnboundedSender<RunEvent>,
) -> Result<()> {
    if app.is_busy() {
        return Ok(());
    }
    let Some(next) = app.queued_front().map(str::to_string) else {
        return Ok(());
    };

    match send_message(connection, app, paths, next, event_tx).await {
        Ok(true) => {
            let _ = app.commit_queued();
        }
        Ok(false) => {
            app.set_status(format!(
                "{} queued · send rejected · Enter retries · Esc discards",
                app.queue_depth()
            ));
        }
        Err(error) => {
            app.set_status(format!(
                "{} queued · daemon error · Enter retries · Esc discards",
                app.queue_depth()
            ));
            return Err(error);
        }
    }
    Ok(())
}

/// Queued messages advance after success or an explicit cancellation.
///
/// A failure pauses because a follow-up may depend on work that did not happen.
/// Cancellation is different: stopping the current turn explicitly skips it and
/// proceeds to the next FIFO item without making the user press Enter again.
const fn should_drain_queue(status: argo_core::event::RunStatus) -> bool {
    matches!(
        status,
        argo_core::event::RunStatus::Succeeded | argo_core::event::RunStatus::Cancelled
    )
}

/// Executes a slash command.
/// Executes a command, optionally returning a message to submit afterwards.
///
/// Returning the follow-up rather than sending it inline keeps this function
/// non-recursive, which async fns cannot be without boxing.
async fn run_command(
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    auth_tx: &mpsc::UnboundedSender<McpAuthEvent>,
    command: Command,
) -> Result<Option<String>> {
    match command {
        Command::Help => {
            let lines = commands::help()
                .into_iter()
                .map(|entry| format!("{:<26} {}", entry.usage, entry.detail))
                .collect();
            app.open_text("commands", lines);
        }

        Command::Quit => app.should_quit = true,

        Command::Agent(None) => {
            let items: Vec<String> = app
                .agents
                .iter()
                .map(|info| {
                    let mark = if info.available { "✓" } else { "·" };
                    let version = info.version.clone().unwrap_or_else(|| {
                        if info.available {
                            "installed".into()
                        } else {
                            "not installed".into()
                        }
                    });
                    format!("{mark} {:<10} {version}", info.id)
                })
                .collect();
            let values = app.agents.iter().map(|info| info.id.clone()).collect();
            app.open_picker("switch agent", items, values, PickerAction::Agent);
        }
        Command::Agent(Some(id)) => match commands::resolve_agent(&id) {
            Ok(agent) => {
                start_agent_flow(connection, app, agent, PickerAction::Model, true).await?;
            }
            Err(message) => app.report_error(message),
        },

        Command::Model(None) => {
            let current = app
                .conversation
                .as_ref()
                .and_then(|c| c.selected_agent_id.clone());
            let agent = current.or_else(|| {
                app.agents
                    .iter()
                    .find(|a| a.available)
                    .map(|a| a.id.clone())
            });
            match agent {
                Some(id) => {
                    if let Some(info) = probe_agent(connection, app, &id, false).await? {
                        let items = info
                            .models
                            .iter()
                            .map(|model| model.label.clone())
                            .collect();
                        let values = info.models.iter().map(|model| model.id.clone()).collect();
                        app.open_picker(
                            format!("model for {}", info.id),
                            items,
                            values,
                            PickerAction::Model,
                        );
                    }
                }
                None => app.report_error("no agent is selected yet; use /agent first"),
            }
        }
        Command::Model(Some(model)) => {
            select(connection, app, commands::model_change(model)).await?;
            offer_effort_for_current_model(connection, app).await?;
        }

        Command::Effort(None) => {
            // Fall back to the first available agent so `/effort` works before the
            // user has explicitly run `/agent`.
            let selected = app
                .conversation
                .as_ref()
                .and_then(|c| c.selected_agent_id.clone())
                .or_else(|| {
                    app.agents
                        .iter()
                        .find(|a| a.available)
                        .map(|a| a.id.clone())
                });
            let model = app
                .conversation
                .as_ref()
                .and_then(|c| c.selected_model.clone());

            match selected {
                Some(id) => {
                    if let Some(info) = probe_agent(connection, app, &id, false).await? {
                        let levels = info.reasoning_for(model.as_deref());
                        if levels.is_empty() {
                            app.report_error(format!(
                                "{} does not expose reasoning levels",
                                info.id
                            ));
                        } else {
                            let title = match &model {
                                Some(model) => format!("reasoning effort for {model}"),
                                None => format!("reasoning effort for {}", info.id),
                            };
                            let items = levels.iter().map(|r| r.label.clone()).collect();
                            let values = levels.iter().map(|r| r.id.clone()).collect();
                            app.open_picker(title, items, values, PickerAction::Effort);
                        }
                    }
                }
                None => app.report_error("no coding CLI was detected; run /agents"),
            }
        }
        Command::Effort(Some(level)) => {
            select(
                connection,
                app,
                argo_core::session::SelectionChange {
                    agent_id: None,
                    model: None,
                    reasoning: Some(level),
                },
            )
            .await?
        }

        Command::Default(action) => match action {
            commands::DefaultCommand::Configure => {
                open_agent_picker(app, "configure default CLI", PickerAction::DefaultAgent);
            }
            commands::DefaultCommand::Current => save_current_default(paths, app)?,
            commands::DefaultCommand::Clear => {
                crate::preferences::save(paths, None)?;
                app.default_selection = None;
                app.set_status("startup default cleared · next new chat will ask for a CLI");
            }
        },

        Command::Mode(None) => {
            let support = app.mode_support();
            if !support.has_any() {
                app.report_error(
                    "the selected CLI cannot enforce an execution mode; it always runs with full access",
                );
                return Ok(None);
            }
            let modes = support.available();
            let items = modes
                .iter()
                .map(|m| format!("{:<14} {}", m.label(), m.detail()))
                .collect();
            let values = modes.iter().map(|m| m.id().to_string()).collect();
            app.open_picker("execution mode", items, values, PickerAction::Mode);
        }
        Command::Mode(Some(requested)) => {
            set_mode(connection, app, Some(requested)).await?;
        }

        Command::Usage => {
            let mut lines = app.usage_report();
            lines.push(String::new());
            lines.push(format!("Current conversation: {}", app.context_label()));
            lines.push(String::new());
            let agent = app
                .conversation
                .as_ref()
                .and_then(|summary| summary.selected_agent_id.as_deref())
                .or_else(|| {
                    app.agents
                        .iter()
                        .find(|agent| agent.available)
                        .map(|agent| agent.id.as_str())
                })
                .unwrap_or("")
                .to_string();
            lines.extend(provider_usage_report(&agent).await);
            app.open_text("usage and provider allowance", lines);
        }

        Command::Status => {
            let lines = app.status_report();
            app.open_text("Argo status", lines);
        }

        Command::Update(action) => {
            let status = match argo_runtime::update::check().await {
                Ok(status) => status,
                Err(error) => {
                    app.report_error(format!("could not check for updates: {error}"));
                    return Ok(None);
                }
            };
            if status.available() {
                app.available_update = Some(status.latest.to_string());
            } else {
                app.available_update = None;
            }
            match action {
                commands::UpdateCommand::Check => {
                    let mut lines = vec![
                        format!("current: v{}", status.current),
                        format!("latest:  v{}", status.latest),
                        String::new(),
                    ];
                    if status.available() {
                        lines.extend([
                            "A newer Argo build is available.".into(),
                            "Run /update install to exit, update, and return to your shell.".into(),
                        ]);
                    } else {
                        lines.push("Argo is up to date.".into());
                    }
                    app.open_text("Argo update", lines);
                }
                commands::UpdateCommand::Install | commands::UpdateCommand::Force => {
                    let force = action == commands::UpdateCommand::Force;
                    if app.is_busy() {
                        app.report_error(
                            "wait for the active agent or use /cancel before updating Argo",
                        );
                    } else if status.available() || force {
                        app.update_on_exit = Some(force);
                        app.should_quit = true;
                    } else {
                        app.set_status(format!("Argo v{} is already up to date", status.current));
                    }
                }
            }
        }

        Command::Agents => {
            open_agents_picker(app);
        }

        Command::Skills => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            match argo_resources_discover(&app.workspace, paths, home.as_deref()) {
                Ok(skills) if skills.is_empty() => {
                    app.open_text(
                        "skills",
                        vec![
                            "No skills found.".to_string(),
                            "Looked in .argo/skills, .claude/skills, .agents/skills,".to_string(),
                            ".opencode/skills, .kiro/skills, and their global equivalents."
                                .to_string(),
                        ],
                    );
                }
                Ok(skills) => {
                    let mut lines =
                        vec![format!("{} skills available to every agent:", skills.len())];
                    for skill in &skills {
                        let first = skill
                            .description
                            .split(['.', '\n'])
                            .next()
                            .unwrap_or_default()
                            .trim();
                        lines.push(format!(
                            "  {:<26} {:<16} {first}",
                            skill.name,
                            skill.origin.label()
                        ));
                        for shadowed in &skill.shadows {
                            lines.push(format!("      shadows {shadowed}"));
                        }
                    }
                    app.open_text("skills", lines);
                }
                Err(error) => app.report_error(error.to_string()),
            }
        }

        Command::Instructions(action) => {
            run_instructions_command(app, action)?;
        }

        Command::Thinking(action) => {
            let visible = match action {
                commands::ThinkingCommand::Show => true,
                commands::ThinkingCommand::Hide => false,
                commands::ThinkingCommand::Toggle => !app.thinking_visible,
            };
            app.set_thinking_visible(visible);
        }

        Command::Mcp(action) => {
            run_mcp_command(app, paths, auth_tx, action).await?;
        }

        Command::Context => {
            let Some(conversation) = app.conversation.as_ref().map(|c| c.id.clone()) else {
                app.report_error("no conversation is open");
                return Ok(None);
            };
            match connection
                .request(Request::PreviewContext {
                    conversation_id: conversation,
                    prompt: "<your next message>".to_string(),
                })
                .await?
            {
                Response::ContextPreview {
                    resuming,
                    reason,
                    body,
                } => {
                    let mut lines = vec![if resuming {
                        "Resuming this agent's own session: only your new message is sent."
                            .to_string()
                    } else {
                        format!(
                            "Fresh session ({}): the context below is sent with your message.",
                            reason.unwrap_or_else(|| "no saved session for this agent".into())
                        )
                    }];
                    lines.push(String::new());
                    lines.extend(body.lines().map(|l| l.to_string()));
                    app.open_text("next turn", lines);
                }
                Response::Error { message, .. } => app.report_error(message),
                other => app.report_error(format!("unexpected reply: {other:?}")),
            }
        }

        Command::Resume(None) => {
            refresh_conversations(connection, app).await?;
            if app.conversations.is_empty() {
                app.open_text(
                    "sessions",
                    vec!["No earlier sessions in this workspace yet.".to_string()],
                );
                return Ok(None);
            }
            // Descriptions rather than bare ids: a session is recognizable by what
            // was discussed and which agents touched it.
            let items = app
                .conversations
                .iter()
                .enumerate()
                .map(|(index, summary)| format!("{:>2}. {}", index + 1, App::describe(summary)))
                .collect();
            let values = app
                .conversations
                .iter()
                .map(|summary| summary.id.to_string())
                .collect();
            app.open_picker(
                "resume a session",
                items,
                values,
                PickerAction::Conversation,
            );
        }

        Command::Resume(Some(target)) => {
            refresh_conversations(connection, app).await?;
            // Accept a list position or a full id, since both are natural.
            let resolved = target
                .parse::<usize>()
                .ok()
                .and_then(|index| app.conversation_at(index.saturating_sub(1)))
                .unwrap_or_else(|| ConversationId::new(target));
            load_conversation(connection, app, &resolved).await?;
        }

        Command::New(title) => new_conversation(connection, app, paths, title).await?,

        Command::ClearHistory => {
            match connection
                .request(Request::ClearConversations {
                    root: Some(app.workspace.clone()),
                })
                .await?
            {
                Response::Cleared { count } => {
                    app.conversations.clear();
                    new_conversation(connection, app, paths, None).await?;
                    app.push(
                        LineKind::Notice,
                        format!("cleared {count} stored conversation(s) in this workspace"),
                    );
                }
                Response::Error { message, .. } => app.report_error(message),
                other => app.report_error(format!("unexpected reply: {other:?}")),
            }
        }

        Command::Children => {
            let Some(conversation) = app.conversation.as_ref().map(|c| c.id.clone()) else {
                app.report_error("no conversation is open");
                return Ok(None);
            };
            match connection
                .request(Request::ListChildren {
                    conversation_id: conversation,
                })
                .await?
            {
                Response::Children { children } if children.is_empty() => app.open_text(
                    "subagents",
                    vec![
                        "No subagent conversations yet.".to_string(),
                        "Use /delegate <agent> <task> to hand work to another CLI.".to_string(),
                    ],
                ),
                Response::Children { children } => {
                    let mut depths = std::collections::HashMap::new();
                    let items: Vec<String> = children
                        .iter()
                        .map(|child| {
                            let depth = child
                                .parent_conversation_id
                                .as_ref()
                                .and_then(|parent| depths.get(parent))
                                .copied()
                                .unwrap_or(0usize)
                                + 1;
                            depths.insert(child.id.clone(), depth);
                            let branch = format!("{}↳", "  ".repeat(depth.saturating_sub(1)));
                            let agent = child.selected_agent_id.as_deref().unwrap_or("?");
                            let model = child.selected_model.as_deref().unwrap_or("default");
                            let title = child.title.as_deref().unwrap_or("(untitled)");
                            let short_id = child
                                .id
                                .to_string()
                                .split('-')
                                .next()
                                .unwrap_or_default()
                                .to_string();
                            format!(
                                "{branch} {:<8} {agent}/{model}  {} msgs  {title}",
                                short_id, child.message_count
                            )
                        })
                        .collect();
                    let values: Vec<String> = children.iter().map(|c| c.id.to_string()).collect();
                    app.open_picker(
                        "orchestrated agents — Enter inspect · Esc stay with parent",
                        items,
                        values,
                        PickerAction::ChildConversation,
                    );
                }
                Response::Error { message, .. } => app.report_error(message),
                other => app.report_error(format!("unexpected reply: {other:?}")),
            }
        }

        Command::Parent => {
            let parent = app
                .conversation
                .as_ref()
                .and_then(|summary| summary.parent_conversation_id.clone());
            if let Some(parent) = parent {
                load_conversation(connection, app, &parent).await?;
                app.set_status("returned to parent conversation · agents continue running");
            } else {
                app.set_status("already at the main conversation");
            }
        }

        Command::Delegate { agent, task } => match commands::resolve_agent(&agent) {
            Ok(agent) => {
                let Some(conversation) = app.conversation.as_ref().map(|c| c.id.clone()) else {
                    app.report_error("no conversation is open");
                    return Ok(None);
                };
                // A real child session: its own conversation and upstream session,
                // seeded with a capsule of this one. The parent conversation is not
                // switched.
                app.push(LineKind::Notice, format!("· delegating to {agent}: {task}"));
                app.set_status(format!("{agent} is working on the delegated task…"));

                match connection
                    .request(Request::Delegate {
                        parent_conversation_id: conversation,
                        parent_run_id: None,
                        agent_id: agent.clone(),
                        model: None,
                        task: task.clone(),
                        timeout_ms: None,
                    })
                    .await?
                {
                    Response::DelegateResult {
                        conversation_id,
                        agent_id,
                        ok,
                        output,
                        ..
                    } => {
                        app.push(
                            LineKind::AgentHeader,
                            format!(
                                "{agent_id} · subagent · {}",
                                if ok { "completed" } else { "failed" }
                            ),
                        );
                        app.push(LineKind::Assistant, output);
                        app.push(
                            LineKind::Activity,
                            format!("· /open {conversation_id} to see the subagent's full session"),
                        );
                        app.set_status(if ok {
                            format!("{agent_id} finished the delegated task")
                        } else {
                            format!("{agent_id} did not complete the task")
                        });
                        app.recompute_context();
                    }
                    Response::Error { message, .. } => app.report_error(message),
                    other => app.report_error(format!("unexpected reply: {other:?}")),
                }
            }
            Err(message) => app.report_error(message),
        },

        Command::Cancel => cancel_active(connection, app).await?,

        Command::Config => {
            app.open_text(
                "configuration",
                vec![
                    format!("workspace     {}", app.workspace),
                    format!("data dir      {}", paths.root().display()),
                    format!("database      {}", paths.database().display()),
                    format!("socket        {}", paths.socket().display()),
                    format!("mcp registry  {}", paths.root().join("mcp.json").display()),
                    format!(
                        "instructions  {} · {}",
                        if argo_resources::instructions::is_enabled(std::path::Path::new(
                            &app.workspace
                        )) {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        argo_resources::instructions::instructions_path(std::path::Path::new(
                            &app.workspace
                        ))
                        .display()
                    ),
                    format!(
                        "startup       {}",
                        app.default_selection
                            .as_ref()
                            .map(|selection| selection.label())
                            .unwrap_or_else(|| "ask on launch".into())
                    ),
                    format!(
                        "preferences   {}",
                        paths.root().join("tui-preferences.json").display()
                    ),
                    format!("user skills   {}", paths.user_skills().display()),
                    format!("protocol      v{IPC_PROTOCOL_VERSION}"),
                    String::new(),
                    "ARGO_DATA_DIR              relocate all state".to_string(),
                    "ARGO_TURN_TIMEOUT_MS       per-turn ceiling, 0 disables".to_string(),
                    "ARGO_LOG=debug             daemon log level".to_string(),
                ],
            );
        }

        Command::Doctor => {
            let mut lines = vec![
                format!("protocol v{IPC_PROTOCOL_VERSION}"),
                format!("database {}", paths.database().display()),
            ];
            match connection.request(Request::Ping).await {
                Ok(Response::Ok) => lines.push("daemon responding".to_string()),
                Ok(other) => lines.push(format!("daemon replied unexpectedly: {other:?}")),
                Err(error) => lines.push(format!("daemon error: {error}")),
            }
            if let Ok(Response::Agents { agents }) = connection
                .request(Request::ListAgents { refresh: true })
                .await
            {
                for info in &agents {
                    lines.push(format!(
                        "{} {} {}",
                        if info.available { "✓" } else { "·" },
                        info.id,
                        info.version.clone().unwrap_or_else(|| if info.available {
                            "installed".into()
                        } else {
                            "not installed".into()
                        })
                    ));
                }
                app.agents = agents;
            }
            app.open_text("doctor", lines);
        }
    }
    Ok(None)
}

/// Runs only after the alternate screen and raw mode have been restored, so the
/// installer can show normal build progress without corrupting the TUI.
async fn update_after_exit(force: bool) -> Result<()> {
    let status = argo_runtime::update::check().await?;
    if !status.available() && !force {
        println!("Argo v{} is already up to date", status.current);
        return Ok(());
    }
    if force && !status.available() {
        println!("reinstalling Argo v{}…", status.latest);
    } else {
        println!("updating Argo v{} → v{}…", status.current, status.latest);
    }
    argo_runtime::update::install_latest().await?;
    println!("update complete · restart Argo to use v{}", status.latest);
    Ok(())
}

/// Advances to the next mode the adapter supports.
async fn cycle_mode(connection: &mut Connection, app: &mut App) -> Result<()> {
    if !app.mode_support().has_any() {
        app.set_status("the selected CLI has no execution modes");
        return Ok(());
    }
    let next = app.next_mode();
    set_mode(connection, app, Some(next.id().to_string())).await
}

/// Records the execution mode and reports what it permits.
async fn set_mode(connection: &mut Connection, app: &mut App, mode: Option<String>) -> Result<()> {
    let Some(conversation) = app.conversation.as_ref().map(|c| c.id.clone()) else {
        app.report_error("no conversation is open");
        return Ok(());
    };
    match connection
        .request(Request::SetMode {
            conversation_id: conversation,
            mode,
        })
        .await?
    {
        Response::Conversation { summary, .. } => {
            app.set_conversation_summary(summary);
            let mode = app.mode();
            // State what the mode actually permits: "plan" alone is ambiguous.
            app.push(
                LineKind::Notice,
                format!("· mode: {} — {}", mode.label(), mode.detail()),
            );
            app.set_status(format!("mode: {}", mode.label()));
        }
        Response::Error {
            code,
            message,
            retryable,
        } => {
            let _ = (code, retryable);
            app.report_error(message);
        }
        other => app.report_error(format!("unexpected reply: {other:?}")),
    }
    Ok(())
}

/// Applies a picker choice.
async fn apply_choice(
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    event_tx: &mpsc::UnboundedSender<RunEvent>,
    auth_tx: &mpsc::UnboundedSender<McpAuthEvent>,
    action: PickerAction,
    value: String,
) -> Result<()> {
    match action {
        PickerAction::StartupAgent => match commands::resolve_agent(&value) {
            Ok(agent) => {
                start_agent_flow(connection, app, agent, PickerAction::StartupModel, false).await
            }
            Err(message) => {
                app.report_error(message);
                Ok(())
            }
        },
        PickerAction::DefaultAgent => match commands::resolve_agent(&value) {
            Ok(agent) => {
                start_agent_flow(connection, app, agent, PickerAction::DefaultModel, true).await
            }
            Err(message) => {
                app.report_error(message);
                Ok(())
            }
        },
        PickerAction::Agent => match commands::resolve_agent(&value) {
            Ok(agent) => start_agent_flow(connection, app, agent, PickerAction::Model, true).await,
            Err(message) => {
                app.report_error(message);
                Ok(())
            }
        },
        PickerAction::Agents => match commands::resolve_agent(&value) {
            Ok(agent) => start_agent_flow(connection, app, agent, PickerAction::Model, true).await,
            Err(message) => {
                app.report_error(message);
                Ok(())
            }
        },
        PickerAction::Model => {
            select(connection, app, commands::model_change(value)).await?;
            offer_effort_for_current_model(connection, app).await
        }
        PickerAction::StartupModel => {
            if select_with_visibility(connection, app, commands::model_change(value), false).await?
                && !open_effort_picker(connection, app, PickerAction::StartupEffort).await?
            {
                finalize_startup_selection(paths, app)?;
            }
            Ok(())
        }
        PickerAction::DefaultModel => {
            if select_with_visibility(connection, app, commands::model_change(value), true).await?
                && !open_effort_picker(connection, app, PickerAction::DefaultEffort).await?
            {
                save_current_default(paths, app)?;
            }
            Ok(())
        }
        PickerAction::Effort => {
            select(
                connection,
                app,
                argo_core::session::SelectionChange {
                    agent_id: None,
                    model: None,
                    reasoning: Some(value),
                },
            )
            .await
        }
        PickerAction::StartupEffort => {
            if select_with_visibility(
                connection,
                app,
                argo_core::session::SelectionChange {
                    agent_id: None,
                    model: None,
                    reasoning: Some(value),
                },
                false,
            )
            .await?
            {
                finalize_startup_selection(paths, app)?;
            }
            Ok(())
        }
        PickerAction::DefaultEffort => {
            if select_with_visibility(
                connection,
                app,
                argo_core::session::SelectionChange {
                    agent_id: None,
                    model: None,
                    reasoning: Some(value),
                },
                true,
            )
            .await?
            {
                save_current_default(paths, app)?;
            }
            Ok(())
        }
        PickerAction::Mode => set_mode(connection, app, Some(value)).await,
        PickerAction::Conversation => {
            let id = ConversationId::new(value);
            load_conversation(connection, app, &id).await
        }
        PickerAction::ChildConversation => {
            let id = ConversationId::new(value);
            open_child_conversation(connection, app, &id).await
        }
        PickerAction::ResponseOption => {
            submit(connection, app, paths, value, event_tx, auth_tx).await
        }
        PickerAction::McpAddTransport => {
            match value.as_str() {
                "remote" => app.open_input(
                    "add remote MCP · endpoint",
                    "HTTP/SSE endpoint URL",
                    false,
                    InputAction::McpRemoteUrl,
                ),
                "local" => app.open_input(
                    "add local MCP · command",
                    "Command and arguments (shell quoting is supported)",
                    false,
                    InputAction::McpLocalCommand,
                ),
                "import" => open_mcp_import_picker(app)?,
                _ => app.report_error("unknown MCP transport"),
            }
            Ok(())
        }
        PickerAction::McpAddAuth => {
            match value.as_str() {
                "none" => {
                    let name = save_mcp_draft(app, paths)?;
                    app.set_status(format!("added MCP server {name}"));
                }
                "oauth" => {
                    let name = save_mcp_draft(app, paths)?;
                    start_mcp_login(app, paths, auth_tx, &name)?;
                }
                "bearer" => app.open_input(
                    "add MCP server · bearer token",
                    "Paste the token (masked and excluded from composer history)",
                    true,
                    InputAction::McpBearerToken,
                ),
                "header" => app.open_input(
                    "add MCP server · custom header",
                    "Header name (for example X-API-Key)",
                    false,
                    InputAction::McpHeaderName,
                ),
                _ => app.report_error("unknown MCP authentication method"),
            }
            Ok(())
        }
        PickerAction::McpLocalConfig => {
            if value == "env" {
                app.open_input(
                    "add local MCP · environment",
                    "Environment key passed to the MCP process",
                    false,
                    InputAction::McpLocalEnvName,
                );
            } else {
                let name = save_mcp_draft(app, paths)?;
                app.set_status(format!("added local MCP server {name}"));
            }
            Ok(())
        }
        PickerAction::McpImport => {
            import_mcp_choice(app, paths, &value)?;
            Ok(())
        }
        PickerAction::Instructions => {
            let action = match value.as_str() {
                "enable" => commands::InstructionsCommand::Enable,
                "disable" => commands::InstructionsCommand::Disable,
                "edit" => commands::InstructionsCommand::Edit,
                _ => {
                    app.report_error("unknown instructions action");
                    return Ok(());
                }
            };
            run_instructions_command(app, action)
        }
    }
}

fn run_instructions_command(app: &mut App, action: commands::InstructionsCommand) -> Result<()> {
    let workspace = std::path::Path::new(&app.workspace);
    match action {
        commands::InstructionsCommand::Menu => {
            let enabled = argo_resources::instructions::is_enabled(workspace);
            app.open_picker(
                "project instructions",
                vec![
                    format!(
                        "Enable automatic capture and injection{}",
                        if enabled { " · active" } else { "" }
                    ),
                    format!(
                        "Disable automatic capture and injection{}",
                        if enabled { "" } else { " · active" }
                    ),
                    "Edit .argo-instructions.md in your editor".into(),
                ],
                vec!["enable".into(), "disable".into(), "edit".into()],
                PickerAction::Instructions,
            );
        }
        commands::InstructionsCommand::Enable => {
            let path = argo_resources::instructions::set_enabled(workspace, true)?;
            let existing_prompts = app
                .lines
                .iter()
                .filter(|line| line.kind == LineKind::User)
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let captured = argo_resources::instructions::capture_user_directives(
                workspace,
                &existing_prompts,
            )?;
            app.push(
                LineKind::Notice,
                format!(
                    "project instructions enabled · {} existing directive(s) captured · durable directives will be saved to {}",
                    captured.len(), path.display()
                ),
            );
            app.set_status("project instructions enabled · /instructions edit to review");
        }
        commands::InstructionsCommand::Disable => {
            let path = argo_resources::instructions::set_enabled(workspace, false)?;
            app.push(
                LineKind::Notice,
                format!(
                    "project instructions disabled · retained {} but it will not be sent",
                    path.display()
                ),
            );
            app.set_status("project instructions disabled · existing file retained");
        }
        commands::InstructionsCommand::Edit => edit_project_instructions(app)?,
    }
    Ok(())
}

fn edit_project_instructions(app: &mut App) -> Result<()> {
    let path = argo_resources::instructions::ensure_file(std::path::Path::new(&app.workspace))?;
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".into());
    let parts = shlex::split(&editor)
        .filter(|parts| !parts.is_empty())
        .ok_or_else(|| ArgoError::Invalid("VISUAL/EDITOR contains invalid quoting".into()))?;
    let restore_mouse = app.mouse_scroll_mode;

    restore_terminal();
    let editor_result = std::process::Command::new(&parts[0])
        .args(&parts[1..])
        .arg(&path)
        .status();

    enable_raw_mode().map_err(|error| ArgoError::Io(format!("restore raw mode: {error}")))?;
    let mut stdout = std::io::stdout();
    enter_terminal_screen(&mut stdout)
        .map_err(|error| ArgoError::Io(format!("restore terminal screen: {error}")))?;
    set_mouse_scroll_mode(&mut stdout, app, restore_mouse)
        .map_err(|error| ArgoError::Io(format!("restore mouse mode: {error}")))?;

    let status =
        editor_result.map_err(|error| ArgoError::Process(format!("open {editor}: {error}")))?;
    if !status.success() {
        return Err(ArgoError::Process(format!(
            "{editor} exited with status {status}"
        )));
    }
    app.set_status(format!(
        "saved {} · {}",
        path.display(),
        if argo_resources::instructions::is_enabled(std::path::Path::new(&app.workspace)) {
            "active for future turns"
        } else {
            "currently disabled"
        }
    ));
    Ok(())
}

fn open_mcp_import_picker(app: &mut App) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| ArgoError::Io("HOME is not set".into()))?;
    let found = argo_resources::discover_importable(&home);
    if found.is_empty() {
        app.mcp_draft = None;
        app.open_text(
            "import MCP server",
            vec!["No MCP servers were found in supported CLI configuration files.".into()],
        );
        return Ok(());
    }
    let items = found
        .iter()
        .map(|entry| {
            let transport = match &entry.server.transport {
                argo_resources::McpTransport::Local { command, .. } => command.join(" "),
                argo_resources::McpTransport::Remote { url, .. } => url.clone(),
            };
            format!("{:<18} {transport} · {}", entry.server.name, entry.source)
        })
        .collect();
    let values = (0..found.len()).map(|index| index.to_string()).collect();
    app.open_picker(
        "import MCP server from CLI config",
        items,
        values,
        PickerAction::McpImport,
    );
    Ok(())
}

fn import_mcp_choice(app: &mut App, paths: &ArgoPaths, value: &str) -> Result<()> {
    let index = value
        .parse::<usize>()
        .map_err(|_| ArgoError::Invalid("invalid MCP import selection".into()))?;
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| ArgoError::Io("HOME is not set".into()))?;
    let found = argo_resources::discover_importable(&home);
    let entry = found
        .into_iter()
        .nth(index)
        .ok_or_else(|| ArgoError::Invalid("MCP import list changed; run /mcp add again".into()))?;
    let name = entry.server.name.clone();
    let path = paths.root().join("mcp.json");
    let mut registry = argo_resources::McpRegistry::load(&path)?;
    registry.upsert(entry.server)?;
    registry.save(&path)?;
    app.mcp_draft = None;
    app.push(LineKind::Notice, format!("· imported MCP server '{name}'"));
    app.set_status(format!("imported MCP server {name}"));
    Ok(())
}

fn open_agent_picker(app: &mut App, title: &str, action: PickerAction) {
    if action == PickerAction::StartupAgent {
        app.startup_save_default = false;
    }
    let available: Vec<_> = app.agents.iter().filter(|info| info.available).collect();
    let items = available
        .iter()
        .map(|info| {
            let is_default = app
                .default_selection
                .as_ref()
                .is_some_and(|selection| selection.agent == info.id);
            let default_mark = if is_default { " ★ default" } else { "" };
            match crate::app::agent_display_version(info) {
                Some(version) => format!("{:<18} {version}{default_mark}", info.name),
                None => format!("{}{default_mark}", info.name),
            }
        })
        .collect();
    let values = available.iter().map(|info| info.id.clone()).collect();
    app.open_picker(title, items, values, action);
}

fn open_agents_picker(app: &mut App) {
    let items = app
        .agents
        .iter()
        .map(|info| {
            let is_default = app
                .default_selection
                .as_ref()
                .is_some_and(|selection| selection.agent == info.id);
            let default_mark = if is_default { " ★ default" } else { "" };
            if info.available {
                match crate::app::agent_display_version(info) {
                    Some(version) => format!("✓ {:<16} {version}{default_mark}", info.name),
                    None => format!("✓ {}{default_mark}", info.name),
                }
            } else {
                format!("· {:<16} not detected{default_mark}", info.name)
            }
        })
        .collect();
    let values = app.agents.iter().map(|info| info.id.clone()).collect();
    app.open_picker("coding CLIs", items, values, PickerAction::Agents);
}

async fn start_agent_flow(
    connection: &mut Connection,
    app: &mut App,
    agent: argo_core::AgentId,
    model_action: PickerAction,
    announce: bool,
) -> Result<()> {
    let agent_id = agent.to_string();
    if !select_with_visibility(
        connection,
        app,
        argo_core::session::SelectionChange {
            agent_id: Some(agent),
            model: None,
            reasoning: None,
        },
        announce,
    )
    .await?
    {
        return Ok(());
    }
    let Some(info) = probe_agent(connection, app, &agent_id, false).await? else {
        return Ok(());
    };
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
    let items = models.iter().map(|model| model.label.clone()).collect();
    let values = models.iter().map(|model| model.id.clone()).collect();
    app.open_picker(
        format!("choose model for {}", info.id),
        items,
        values,
        model_action,
    );
    Ok(())
}

/// Opens the model-specific effort picker, returning whether one was shown.
async fn open_effort_picker(
    connection: &mut Connection,
    app: &mut App,
    action: PickerAction,
) -> Result<bool> {
    let Some(summary) = app.conversation.clone() else {
        return Ok(false);
    };
    let Some(agent_id) = summary.selected_agent_id else {
        return Ok(false);
    };
    let model = summary
        .selected_model
        .unwrap_or_else(|| argo_runtime::DEFAULT_MODEL_ID.to_string());
    let Some(info) = probe_agent(connection, app, &agent_id, false).await? else {
        return Ok(false);
    };
    let levels = info.reasoning_for(Some(&model));
    if levels.is_empty() {
        return Ok(false);
    }
    let items = levels.iter().map(|level| level.label.clone()).collect();
    let values = levels.iter().map(|level| level.id.clone()).collect();
    app.open_picker(
        format!("reasoning effort for {agent_id}/{model}"),
        items,
        values,
        action,
    );
    Ok(true)
}

/// Opens the effort picker immediately after a model selection when applicable.
async fn offer_effort_for_current_model(connection: &mut Connection, app: &mut App) -> Result<()> {
    let _ = open_effort_picker(connection, app, PickerAction::Effort).await?;
    Ok(())
}

fn finalize_startup_selection(paths: &ArgoPaths, app: &mut App) -> Result<()> {
    if std::mem::take(&mut app.startup_save_default) {
        save_current_default(paths, app)
    } else {
        let label = app.selection_label();
        app.set_status(format!("using {label} for this chat · /default to save it"));
        Ok(())
    }
}

fn save_current_default(paths: &ArgoPaths, app: &mut App) -> Result<()> {
    let Some(summary) = &app.conversation else {
        app.report_error("no conversation is open");
        return Ok(());
    };
    let Some(agent) = summary.selected_agent_id.clone() else {
        app.report_error("choose a CLI before saving a default");
        return Ok(());
    };
    let Some(model) = summary.selected_model.clone() else {
        app.report_error("choose a model before saving a default");
        return Ok(());
    };
    if model == argo_runtime::DEFAULT_MODEL_ID {
        app.report_error("choose a concrete model before saving a default");
        return Ok(());
    }
    let selection = crate::preferences::DefaultSelection {
        agent,
        model,
        effort: summary.selected_reasoning.clone(),
    };
    crate::preferences::save(paths, Some(selection.clone()))?;
    app.default_selection = Some(selection.clone());
    app.set_status(format!("saved startup default · {}", selection.label()));
    Ok(())
}

/// Provider allowance is intentionally separate from per-turn token accounting.
/// Only local, non-inference CLI surfaces are called here.
async fn provider_usage_report(agent_id: &str) -> Vec<String> {
    let mut lines = vec!["Provider allowance / local history:".into()];
    match agent_id {
        "codex" => match codex_rate_limits().await {
            Ok(report) => lines.extend(report),
            Err(error) => lines.push(format!("Codex allowance unavailable: {error}")),
        },
        "claude" => match claude_usage().await {
            Ok(report) => lines.extend(report),
            Err(error) => lines.push(format!("Claude allowance unavailable: {error}")),
        },
        "opencode" => match opencode_local_stats().await {
            Ok(report) => {
                lines.push("OpenCode local history (not provider quota):".into());
                lines.extend(report);
            }
            Err(error) => lines.push(format!("OpenCode local history unavailable: {error}")),
        },
        "kiro" => match kiro_usage().await {
            Ok(report) => lines.extend(report),
            Err(error) => lines.push(format!("Kiro usage unavailable: {error}")),
        },
        "cmd" => match interactive_slash_usage(
            "commandcode",
            &["--skip-onboarding", "--trust"],
            "Command Code",
        )
        .await
        {
            Ok(report) => lines.extend(report),
            Err(error) => lines.push(format!("Command Code usage unavailable: {error}")),
        },
        "antigravity" => match interactive_slash_usage(
            "agy",
            &["--dangerously-skip-permissions"],
            "Antigravity",
        )
        .await
        {
            Ok(report) => lines.extend(report),
            Err(error) => lines.push(format!("Antigravity usage unavailable: {error}")),
        },
        "grok" => lines.push(
            "Grok stores local historical token totals, but exposes no remaining xAI quota command."
                .into(),
        ),
        _ => lines.push("No provider allowance surface is available for this CLI.".into()),
    }
    lines
}

async fn codex_rate_limits() -> Result<Vec<String>> {
    let mut child = tokio::process::Command::new("codex")
        .args(["app-server", "--listen", "stdio://"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ArgoError::Process(format!("start codex app-server: {error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ArgoError::Process("codex app-server has no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ArgoError::Process("codex app-server has no stdout".into()))?;
    let mut reader = BufReader::new(stdout).lines();

    write_json_line(
        &mut stdin,
        serde_json::json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": { "name": "argo", "title": "Argo", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": null
            }
        }),
    )
    .await?;
    let _ = read_json_id(&mut reader, 1).await?;
    write_json_line(&mut stdin, serde_json::json!({ "method": "initialized" })).await?;
    write_json_line(
        &mut stdin,
        serde_json::json!({ "method": "account/rateLimits/read", "id": 2, "params": null }),
    )
    .await?;
    let response = read_json_id(&mut reader, 2).await?;
    drop(stdin);
    let _ = child.start_kill();
    let _ = child.wait().await;

    let result = response
        .get("result")
        .ok_or_else(|| ArgoError::Protocol("Codex rate-limit response had no result".into()))?;
    let limits = result
        .get("rateLimitsByLimitId")
        .and_then(|groups| groups.get("codex"))
        .filter(|value| !value.is_null())
        .or_else(|| result.get("rateLimits"))
        .unwrap_or(result);
    let mut lines = Vec::new();
    if let Some(plan) = limits.get("planType").and_then(serde_json::Value::as_str) {
        lines.push(format!("Codex plan: {plan}"));
    }
    for (label, key) in [("primary", "primary"), ("secondary", "secondary")] {
        let Some(window) = limits.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let Some(used) = window
            .get("usedPercent")
            .and_then(serde_json::Value::as_f64)
        else {
            continue;
        };
        let remaining = (100.0 - used).clamp(0.0, 100.0);
        let duration = window
            .get("windowDurationMins")
            .and_then(serde_json::Value::as_u64)
            .map(|minutes| format!(" · {minutes} min window"))
            .unwrap_or_default();
        let reset = window
            .get("resetsAt")
            .and_then(serde_json::Value::as_i64)
            .map(|epoch| format!(" · resets at epoch {epoch}"))
            .unwrap_or_default();
        lines.push(format!(
            "Codex {label}: {remaining:.0}% remaining ({used:.0}% used){duration}{reset}"
        ));
    }
    if let Some(credits) = limits.get("credits").filter(|value| !value.is_null()) {
        if credits
            .get("unlimited")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            lines.push("Codex credits: unlimited".into());
        } else if let Some(balance) = credits.get("balance").and_then(serde_json::Value::as_str) {
            lines.push(format!("Codex credit balance: {balance}"));
        }
    }
    if lines.is_empty() {
        lines.push("Codex returned no rate-limit windows.".into());
    }
    Ok(lines)
}

async fn write_json_line(
    writer: &mut tokio::process::ChildStdin,
    value: serde_json::Value,
) -> Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|error| ArgoError::Io(format!("write usage request: {error}")))?;
    writer
        .flush()
        .await
        .map_err(|error| ArgoError::Io(format!("flush usage request: {error}")))
}

async fn read_json_id(
    reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    wanted: i64,
) -> Result<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let line = tokio::time::timeout_at(deadline, reader.next_line())
            .await
            .map_err(|_| ArgoError::Timeout(8_000))?
            .map_err(|error| ArgoError::Io(format!("read usage response: {error}")))?
            .ok_or_else(|| ArgoError::Protocol("usage process closed its output".into()))?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(serde_json::Value::as_i64) == Some(wanted) {
            return Ok(value);
        }
    }
}

async fn claude_usage() -> Result<Vec<String>> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("claude")
            .args([
                "-p",
                "/usage",
                "--output-format",
                "json",
                "--no-session-persistence",
            ])
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| ArgoError::Timeout(15_000))?
    .map_err(|error| ArgoError::Process(format!("run Claude /usage: {error}")))?;
    if !output.status.success() {
        return Err(ArgoError::Process(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let inference_used = value
        .get("duration_api_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        > 0
        || value
            .get("num_turns")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            > 0
        || value
            .get("total_cost_usd")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
            > 0.0;
    if inference_used {
        return Err(ArgoError::Invalid(
            "Claude unexpectedly treated /usage as a model turn; result suppressed".into(),
        ));
    }
    let result = value
        .get("result")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ArgoError::Protocol("Claude /usage returned no result text".into()))?;
    let mut lines = vec!["Claude account usage (local /usage command):".into()];
    lines.extend(
        result
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(30)
            .map(str::to_string),
    );
    Ok(lines)
}

async fn opencode_local_stats() -> Result<Vec<String>> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(12),
        tokio::process::Command::new("opencode")
            .args(["stats", "--pure", "--days", "30", "--models", "5"])
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| ArgoError::Timeout(12_000))?
    .map_err(|error| ArgoError::Process(format!("run opencode stats: {error}")))?;
    if !output.status.success() {
        return Err(ArgoError::Process(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .take(30)
        .map(str::to_string)
        .collect())
}

async fn kiro_usage() -> Result<Vec<String>> {
    // Use the same candidate binaries as detection. Some installations expose
    // only the historical `kiro` symlink, so hardcoding `kiro-cli` made Argo say
    // usage was unavailable even though the selected adapter was working.
    let candidates = argo_runtime::find("kiro")
        .map(argo_runtime::RuntimeDef::candidate_bins)
        .unwrap_or_else(|| vec!["kiro-cli", "kiro"]);
    let mut output = None;
    for binary in candidates {
        let attempt = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::process::Command::new(binary)
                .args(["chat", "--no-interactive", "/usage"])
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| ArgoError::Timeout(15_000))?;
        match attempt {
            Ok(found) => {
                output = Some(found);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ArgoError::Process(format!(
                    "run Kiro /usage with {binary}: {error}"
                )))
            }
        }
    }
    let output = output.ok_or_else(|| {
        ArgoError::Process("run Kiro /usage: neither kiro-cli nor kiro was found".into())
    })?;
    if !output.status.success() {
        return Err(ArgoError::Process(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let clean = strip_terminal_sequences(&String::from_utf8_lossy(&output.stdout));
    let lines = useful_usage_lines(&clean, "Kiro");
    if lines.iter().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("estimated usage") || (lower.contains("credits") && lower.contains("plan"))
    }) {
        Ok(lines)
    } else {
        Err(ArgoError::Protocol(
            "Kiro /usage returned no usage summary".into(),
        ))
    }
}

/// Runs a slash command inside a real pseudo-terminal so it is handled by the
/// CLI's local command palette, never by its model-facing print mode.
async fn interactive_slash_usage(program: &str, args: &[&str], label: &str) -> Result<Vec<String>> {
    let mut command = tokio::process::Command::new("script");
    #[cfg(target_os = "macos")]
    {
        command.args(["-q", "/dev/null", program]);
        command.args(args);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let invocation = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        command.args(["-q", "-c", &invocation, "/dev/null"]);
    }
    #[cfg(not(unix))]
    {
        let _ = (program, args, label);
        return Err(ArgoError::Invalid(
            "interactive /usage capture is currently supported on Unix terminals".into(),
        ));
    }

    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ArgoError::Process(format!("start {label} /usage: {error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ArgoError::Process(format!("{label} usage process has no stdin")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ArgoError::Process(format!("{label} usage process has no stdout")))?;

    // The command palette accepts Tab completion before Enter across the two
    // installed Ink/terminal implementations. Input written before their first
    // paint is buffered by the PTY.
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    stdin
        .write_all(b"/usage")
        .await
        .map_err(|error| ArgoError::Io(format!("write {label} /usage: {error}")))?;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    stdin
        .write_all(b"\t")
        .await
        .map_err(|error| ArgoError::Io(format!("complete {label} /usage: {error}")))?;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    stdin
        .write_all(b"\r")
        .await
        .map_err(|error| ArgoError::Io(format!("submit {label} /usage: {error}")))?;
    stdin
        .flush()
        .await
        .map_err(|error| ArgoError::Io(format!("flush {label} /usage: {error}")))?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = tokio::time::timeout_at(deadline, stdout.read(&mut buffer))
            .await
            .map_err(|_| ArgoError::Timeout(25_000))?
            .map_err(|error| ArgoError::Io(format!("read {label} /usage: {error}")))?;
        if count == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..count]);
        if raw.len() > 256 * 1024 {
            return Err(ArgoError::Invalid(format!(
                "{label} /usage output exceeded 256 KiB"
            )));
        }
        let clean = strip_terminal_sequences(&String::from_utf8_lossy(&raw));
        let lower = clean.to_ascii_lowercase();
        if lower.contains("usage limits")
            || (lower.contains("usage")
                && lower.contains("plan")
                && (lower.contains("used") || lower.contains("remaining")))
        {
            drop(stdin);
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(useful_usage_lines(&clean, label));
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    Err(ArgoError::Protocol(format!(
        "{label} local /usage overlay returned no usage summary"
    )))
}

fn useful_usage_lines(text: &str, label: &str) -> Vec<String> {
    let mut lines = vec![format!("{label} account usage (local /usage command):")];
    let mut seen = std::collections::HashSet::new();
    let relevant = text
        .rfind("\n USAGE")
        .or_else(|| text.rfind("Estimated Usage"))
        .map(|index| &text[index..])
        .unwrap_or(text);
    for line in relevant.lines() {
        let line = line.trim().trim_matches('\0');
        if line.is_empty()
            || line.eq_ignore_ascii_case("press esc to close")
            || line.eq_ignore_ascii_case("loading…")
            || line.starts_with('❯')
            || line.contains("Ask your question")
        {
            continue;
        }
        if seen.insert(line.to_string()) {
            lines.push(line.to_string());
        }
        if lines.len() >= 30 {
            break;
        }
    }
    lines
}

fn strip_terminal_sequences(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index >= bytes.len() {
                break;
            }
            match bytes[index] {
                b'[' => {
                    index += 1;
                    while index < bytes.len() {
                        let byte = bytes[index];
                        index += 1;
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                }
                b']' => {
                    index += 1;
                    while index < bytes.len() {
                        if bytes[index] == 0x07 {
                            index += 1;
                            break;
                        }
                        if bytes[index] == 0x1b && bytes.get(index + 1).copied() == Some(b'\\') {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                _ => index += 1,
            }
            continue;
        }
        match bytes[index] {
            b'\r' => output.push('\n'),
            b'\n' | b'\t' => output.push(bytes[index] as char),
            byte if byte >= 0x20 => {
                let rest = &input[index..];
                let Some(ch) = rest.chars().next() else { break };
                output.push(ch);
                index += ch.len_utf8();
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    output
}

async fn run_mcp_command(
    app: &mut App,
    paths: &ArgoPaths,
    auth_tx: &mpsc::UnboundedSender<McpAuthEvent>,
    action: commands::McpCommand,
) -> Result<()> {
    let registry_path = paths.root().join("mcp.json");
    match action {
        commands::McpCommand::List => {
            let registry = argo_resources::McpRegistry::load(&registry_path)?;
            let token_path = argo_resources::oauth::token_store_path(paths.root());
            let tokens = argo_resources::oauth::TokenStore::load(&token_path)?;
            let mut lines = vec![
                "Commands: /mcp add · check [name] · reconnect · reauth · logout · delete".into(),
                String::new(),
                "argo                 built-in  delegation · injected safely per turn".into(),
            ];
            if registry.servers.is_empty() {
                lines.extend([
                    String::new(),
                    "No additional MCP servers configured.".into(),
                    format!("Registry: {}", registry_path.display()),
                    "Run /mcp add for guided local, remote, auth, or import setup.".into(),
                ]);
            }
            for server in &registry.servers {
                let auth = if tokens.tokens.contains_key(&server.name) {
                    "authenticated"
                } else {
                    "no Argo token"
                };
                let transport = match &server.transport {
                    argo_resources::McpTransport::Local { command, .. } => {
                        format!("local · {}", command.join(" "))
                    }
                    argo_resources::McpTransport::Remote { url, .. } => {
                        format!("remote · {url} · {auth}")
                    }
                };
                lines.push(format!(
                    "{:<20} {:<8} {transport}",
                    server.name,
                    if server.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ));
            }
            app.open_text("mcp servers", lines);
        }
        commands::McpCommand::Add => {
            app.mcp_draft = Some(McpDraft::default());
            app.open_input(
                "add MCP server · 1/3",
                "Unique server name (used as the tool prefix)",
                false,
                InputAction::McpAddName,
            );
        }
        commands::McpCommand::Check(name) => {
            let lines = check_mcp_servers(paths, name.as_deref()).await?;
            app.open_text("mcp connection check", lines);
        }
        commands::McpCommand::Reconnect(name) => {
            let mut lines = vec![
                "Argo creates MCP connections per agent turn; no stale shared socket is retained."
                    .into(),
                "The server was re-checked and the next turn will receive the current configuration."
                    .into(),
                String::new(),
            ];
            lines.extend(check_mcp_servers(paths, Some(&name)).await?);
            app.open_text("mcp reconnect", lines);
        }
        commands::McpCommand::Login(name) => {
            start_mcp_login(app, paths, auth_tx, &name)?;
        }
        commands::McpCommand::Logout(name) => {
            let token_path = argo_resources::oauth::token_store_path(paths.root());
            let mut tokens = argo_resources::oauth::TokenStore::load(&token_path)?;
            let removed = tokens.tokens.remove(&name).is_some();
            tokens.save(&token_path)?;
            app.set_status(if removed {
                format!("forgot MCP credentials for {name}")
            } else {
                format!("no stored MCP credentials for {name}")
            });
        }
        commands::McpCommand::Remove(name) => {
            let mut registry = argo_resources::McpRegistry::load(&registry_path)?;
            if !registry.remove(&name) {
                app.report_error(format!("MCP server not found: {name}"));
                return Ok(());
            }
            registry.save(&registry_path)?;
            let token_path = argo_resources::oauth::token_store_path(paths.root());
            let mut tokens = argo_resources::oauth::TokenStore::load(&token_path)?;
            tokens.tokens.remove(&name);
            tokens.save(&token_path)?;
            app.push(
                LineKind::Notice,
                format!("deleted MCP server '{name}' and its Argo credentials"),
            );
            app.set_status(format!("deleted MCP server {name}"));
        }
    }
    Ok(())
}

fn apply_mcp_input(
    app: &mut App,
    paths: &ArgoPaths,
    auth_tx: &mpsc::UnboundedSender<McpAuthEvent>,
    action: InputAction,
    value: String,
) -> Result<()> {
    let value = value.trim().to_string();
    if value.is_empty() {
        app.report_error("this MCP setup field cannot be empty");
        return Ok(());
    }
    match action {
        InputAction::McpAddName => {
            if !value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
            {
                app.report_error("MCP names may contain letters, numbers, '-' and '_' only");
                return Ok(());
            }
            app.mcp_draft.get_or_insert_with(McpDraft::default).name = value;
            app.open_picker(
                "add MCP server · transport",
                vec![
                    "Remote HTTP/SSE endpoint".into(),
                    "Local stdio command".into(),
                    "Import from another CLI config".into(),
                ],
                vec!["remote".into(), "local".into(), "import".into()],
                PickerAction::McpAddTransport,
            );
        }
        InputAction::McpRemoteUrl => {
            if !is_safe_web_url(&value) {
                app.report_error("remote MCP endpoint must be a valid http:// or https:// URL");
                return Ok(());
            }
            app.mcp_draft.get_or_insert_with(McpDraft::default).url = Some(value);
            open_mcp_auth_picker(app);
        }
        InputAction::McpLocalCommand => {
            let Some(command) = shlex::split(&value).filter(|parts| !parts.is_empty()) else {
                app.report_error("local MCP command has invalid or unmatched shell quoting");
                return Ok(());
            };
            app.mcp_draft.get_or_insert_with(McpDraft::default).command = command;
            open_mcp_local_config_picker(app);
        }
        InputAction::McpBearerToken => {
            let name = save_mcp_draft(app, paths)?;
            let token_path = argo_resources::oauth::token_store_path(paths.root());
            let mut tokens = argo_resources::oauth::TokenStore::load(&token_path)?;
            tokens.tokens.insert(
                name.clone(),
                argo_resources::oauth::StoredToken {
                    access_token: value,
                    refresh_token: None,
                    expires_at: None,
                    client_id: "manual-bearer-token".into(),
                    token_endpoint: String::new(),
                },
            );
            tokens.save(&token_path)?;
            app.set_status(format!("added and authenticated MCP server {name}"));
        }
        InputAction::McpHeaderName => {
            app.mcp_draft
                .get_or_insert_with(McpDraft::default)
                .pending_key = Some(value);
            app.open_input(
                "add MCP server · header authentication",
                "Environment variable containing the header value (the secret is not stored)",
                false,
                InputAction::McpHeaderEnv,
            );
        }
        InputAction::McpHeaderEnv => {
            let draft = app.mcp_draft.get_or_insert_with(McpDraft::default);
            let Some(key) = draft.pending_key.take() else {
                app.report_error("missing header name; restart /mcp add");
                return Ok(());
            };
            draft.headers.push((key, format!("{{env:{value}}}")));
            let name = save_mcp_draft(app, paths)?;
            app.set_status(format!(
                "added MCP server {name} with environment-backed header"
            ));
        }
        InputAction::McpLocalEnvName => {
            app.mcp_draft
                .get_or_insert_with(McpDraft::default)
                .pending_key = Some(value);
            app.open_input(
                "add local MCP · environment",
                "Source environment variable (its current value is not stored)",
                false,
                InputAction::McpLocalEnvSource,
            );
        }
        InputAction::McpLocalEnvSource => {
            let draft = app.mcp_draft.get_or_insert_with(McpDraft::default);
            let Some(key) = draft.pending_key.take() else {
                app.report_error("missing environment key; restart /mcp add");
                return Ok(());
            };
            draft.environment.push((key, format!("{{env:{value}}}")));
            open_mcp_local_config_picker(app);
        }
    }
    let _ = auth_tx;
    Ok(())
}

fn open_mcp_auth_picker(app: &mut App) {
    app.open_picker(
        "add remote MCP · authentication",
        vec![
            "No authentication".into(),
            "OAuth 2.1 · browser + fallback link".into(),
            "Bearer token · paste securely".into(),
            "Custom header · value from environment".into(),
        ],
        vec![
            "none".into(),
            "oauth".into(),
            "bearer".into(),
            "header".into(),
        ],
        PickerAction::McpAddAuth,
    );
}

fn open_mcp_local_config_picker(app: &mut App) {
    app.open_picker(
        "add local MCP · configuration",
        vec![
            "Save and enable server".into(),
            "Add environment-variable mapping".into(),
        ],
        vec!["save".into(), "env".into()],
        PickerAction::McpLocalConfig,
    );
}

fn save_mcp_draft(app: &mut App, paths: &ArgoPaths) -> Result<String> {
    let Some(draft) = app.mcp_draft.take() else {
        return Err(ArgoError::Invalid(
            "MCP setup expired; run /mcp add again".into(),
        ));
    };
    let transport = if let Some(url) = draft.url {
        argo_resources::McpTransport::Remote {
            url,
            headers: draft.headers,
        }
    } else if !draft.command.is_empty() {
        argo_resources::McpTransport::Local {
            command: draft.command,
            environment: draft.environment,
        }
    } else {
        return Err(ArgoError::Invalid("MCP transport is incomplete".into()));
    };
    let server = argo_resources::McpServer {
        name: draft.name.clone(),
        transport,
        enabled: true,
    };
    let path = paths.root().join("mcp.json");
    let mut registry = argo_resources::McpRegistry::load(&path)?;
    registry.upsert(server)?;
    registry.save(&path)?;
    app.push(
        LineKind::Notice,
        format!(
            "· added MCP server '{}' for every supported CLI",
            draft.name
        ),
    );
    Ok(draft.name)
}

fn start_mcp_login(
    app: &mut App,
    paths: &ArgoPaths,
    auth_tx: &mpsc::UnboundedSender<McpAuthEvent>,
    name: &str,
) -> Result<()> {
    let registry = argo_resources::McpRegistry::load(&paths.root().join("mcp.json"))?;
    let server = registry
        .servers
        .iter()
        .find(|server| server.name == name)
        .ok_or_else(|| ArgoError::not_found("mcp server", name))?;
    let url = match &server.transport {
        argo_resources::McpTransport::Remote { url, .. } => url.clone(),
        argo_resources::McpTransport::Local { .. } => {
            app.report_error(format!("'{name}' is local and does not use OAuth"));
            return Ok(());
        }
    };
    let owned_name = name.to_string();
    let token_path = argo_resources::oauth::token_store_path(paths.root());
    let sender = auth_tx.clone();
    let task_name = owned_name.clone();
    tokio::task::spawn_blocking(move || {
        let progress_name = task_name.clone();
        let progress_sender = sender.clone();
        let mut announce = move |message: &str| {
            let _ = progress_sender.send(McpAuthEvent::Progress {
                name: progress_name.clone(),
                message: message.to_string(),
            });
        };
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = sender.send(McpAuthEvent::Failed {
                    name: task_name,
                    message: format!("start OAuth runtime: {error}"),
                });
                return;
            }
        };
        match runtime.block_on(argo_resources::oauth::login(
            &task_name,
            &url,
            &token_path,
            &mut announce,
        )) {
            Ok(()) => {
                let _ = sender.send(McpAuthEvent::Complete { name: task_name });
            }
            Err(error) => {
                let _ = sender.send(McpAuthEvent::Failed {
                    name: task_name,
                    message: error.to_string(),
                });
            }
        }
    });
    app.open_text(
        "mcp authentication",
        vec![
            format!("Reauthentication started for '{owned_name}'."),
            "Discovering OAuth metadata…".into(),
            "The exact authorization link will appear here; click it or drag-copy it if the browser does not open.".into(),
            String::new(),
            "Esc closes this pane; login continues in the background.".into(),
        ],
    );
    Ok(())
}

async fn check_mcp_servers(paths: &ArgoPaths, only: Option<&str>) -> Result<Vec<String>> {
    let registry = argo_resources::McpRegistry::load(&paths.root().join("mcp.json"))?;
    let mut lines = Vec::new();
    if only.is_none_or(|name| name == "argo") {
        let verdict = match argo_daemon::mcp::delegation_health(paths).await {
            Ok(()) => "ok · daemon connected · configuration refreshed every turn".into(),
            Err(error) => format!("unavailable · automatic repair failed: {error}"),
        };
        lines.push(format!("{:<20} {verdict}", "argo (delegation)"));
        if only == Some("argo") {
            return Ok(lines);
        }
    }
    let selected: Vec<_> = registry
        .servers
        .iter()
        .filter(|server| only.is_none_or(|name| server.name == name))
        .collect();
    if selected.is_empty() {
        return if let Some(name) = only {
            Err(ArgoError::not_found("mcp server", name))
        } else {
            Ok(lines)
        };
    }

    for server in selected {
        let verdict = match &server.transport {
            argo_resources::McpTransport::Local { command, .. } => {
                let binary = command.first().map(String::as_str).unwrap_or_default();
                if binary_on_path(binary) {
                    format!("ok · local executable {binary}")
                } else {
                    format!("not found · {binary} is not on PATH")
                }
            }
            argo_resources::McpTransport::Remote { url, headers } => {
                let authorized = argo_resources::oauth::stored_access_token(
                    &server.name,
                    &argo_resources::oauth::token_store_path(paths.root()),
                )
                .map(|(token, _)| argo_resources::with_bearer_token(server, &token))
                .unwrap_or_else(|| (*server).clone());
                let effective_headers = match authorized.transport {
                    argo_resources::McpTransport::Remote { headers, .. } => headers,
                    _ => headers.clone(),
                };
                match probe_remote_mcp(url, &effective_headers).await {
                    Some(code) if (200..300).contains(&code) => format!("ok · HTTP {code}"),
                    Some(401 | 403) => "authentication required · use /mcp reauth".into(),
                    Some(code) => format!("HTTP {code}"),
                    None => "unreachable".into(),
                }
            }
        };
        lines.push(format!("{:<20} {verdict}", server.name));
    }
    Ok(lines)
}

fn binary_on_path(binary: &str) -> bool {
    if binary.contains('/') {
        return std::path::Path::new(binary).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
}

async fn probe_remote_mcp(url: &str, headers: &[(String, String)]) -> Option<u16> {
    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"argo-tui","version":"0.1.0"}}}"#;
    let mut command = tokio::process::Command::new("curl");
    command
        .args(["-sS", "-m", "12", "-o", "/dev/null", "-w", "%{http_code}"])
        .args(["-X", "POST", url])
        .args(["-H", "Content-Type: application/json"])
        .args(["-H", "Accept: application/json, text/event-stream"])
        .args(["-d", INIT]);
    for (name, value) in headers {
        command.args(["-H", &format!("{name}: {value}")]);
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
        .await
        .ok()?
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Deep-probes only the adapter whose live choices the user requested.
async fn probe_agent(
    connection: &mut Connection,
    app: &mut App,
    agent_id: &str,
    refresh: bool,
) -> Result<Option<argo_runtime::AgentInfo>> {
    match connection
        .request(Request::ProbeAgent {
            agent_id: agent_id.to_string(),
            refresh,
        })
        .await?
    {
        Response::Agent { agent } => {
            app.update_agent(agent.clone());
            Ok(Some(agent))
        }
        Response::Error { message, .. } => {
            app.report_error(message);
            Ok(None)
        }
        other => {
            app.report_error(format!("unexpected probe reply: {other:?}"));
            Ok(None)
        }
    }
}

/// Records a selection change and reports what it will do.
async fn select(
    connection: &mut Connection,
    app: &mut App,
    change: argo_core::session::SelectionChange,
) -> Result<()> {
    select_with_visibility(connection, app, change, true)
        .await
        .map(|_| ())
}

/// Applies a selection, optionally keeping the empty launch screen pristine.
async fn select_with_visibility(
    connection: &mut Connection,
    app: &mut App,
    change: argo_core::session::SelectionChange,
    announce: bool,
) -> Result<bool> {
    let Some(conversation) = app.conversation.as_ref().map(|c| c.id.clone()) else {
        app.report_error("no conversation is open");
        return Ok(false);
    };
    match connection
        .request(Request::Select {
            conversation_id: conversation,
            change,
        })
        .await?
    {
        Response::Conversation { summary, .. } => {
            let label = format!(
                "{}/{}",
                summary
                    .selected_agent_id
                    .clone()
                    .unwrap_or_else(|| "auto".into()),
                summary
                    .selected_model
                    .clone()
                    .unwrap_or_else(|| "default".into())
            );
            app.set_conversation_summary(summary);
            if announce {
                // Selections apply at the next turn, never to a running child.
                app.push(
                    LineKind::Notice,
                    format!("· {label} — applies to your next message"),
                );
            }
            app.set_status(format!("switched to {label}"));
            Ok(true)
        }
        Response::Error { message, .. } => {
            app.report_error(message);
            Ok(false)
        }
        other => {
            app.report_error(format!("unexpected reply: {other:?}"));
            Ok(false)
        }
    }
}

/// Cancels the running turn.
async fn cancel_active(connection: &mut Connection, app: &mut App) -> Result<()> {
    let Some(run_id) = app.active_run.clone() else {
        app.set_status("nothing is running");
        return Ok(());
    };
    match connection.request(Request::Cancel { run_id }).await? {
        Response::Ok => app.set_status("cancelling…"),
        Response::Error { message, .. } => app.report_error(message),
        other => app.report_error(format!("unexpected reply: {other:?}")),
    }
    Ok(())
}

/// Creates a conversation and makes it current.
async fn new_conversation(
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    title: Option<String>,
) -> Result<()> {
    match connection
        .request(Request::NewConversation {
            root: app.workspace.clone(),
            title,
        })
        .await?
    {
        Response::Conversation { summary, .. } => {
            app.replace_transcript(Vec::new());
            app.set_conversation_summary(summary);
            if let Some(missing) = clear_default_if_agent_missing(paths, app)? {
                app.set_status(format!(
                    "saved default {missing} is no longer detected · default cleared"
                ));
                open_agent_picker(app, "choose a coding CLI", PickerAction::StartupAgent);
            } else if let Some(configured) = app.default_selection.clone() {
                if select_with_visibility(
                    connection,
                    app,
                    argo_core::session::SelectionChange {
                        agent_id: Some(argo_core::AgentId::new(configured.agent.clone())),
                        model: Some(configured.model.clone()),
                        reasoning: configured.effort.clone(),
                    },
                    false,
                )
                .await?
                {
                    app.set_status(format!(
                        "default selected · {} · /agent to change",
                        configured.label()
                    ));
                } else {
                    crate::preferences::save(paths, None)?;
                    app.default_selection = None;
                    app.set_status("saved default could not be applied · default cleared");
                    open_agent_picker(app, "choose a coding CLI", PickerAction::StartupAgent);
                }
            } else if app.agents.iter().any(|agent| agent.available) {
                open_agent_picker(app, "choose a coding CLI", PickerAction::StartupAgent);
            } else {
                app.set_status("new conversation · no coding CLI detected");
            }
        }
        Response::Error { message, .. } => app.report_error(message),
        other => app.report_error(format!("unexpected reply: {other:?}")),
    }
    Ok(())
}

/// Clears a persisted default when its executable disappeared between launches.
/// Returning the missing id lets startup explain why it reopened the picker.
fn clear_default_if_agent_missing(paths: &ArgoPaths, app: &mut App) -> Result<Option<String>> {
    let Some(configured) = app.default_selection.as_ref() else {
        return Ok(None);
    };
    if app
        .agents
        .iter()
        .any(|agent| agent.available && agent.id == configured.agent)
    {
        return Ok(None);
    }
    let missing = configured.agent.clone();
    crate::preferences::save(paths, None)?;
    app.default_selection = None;
    Ok(Some(missing))
}

/// Loads a conversation's transcript into the view.
async fn load_conversation(
    connection: &mut Connection,
    app: &mut App,
    id: &ConversationId,
) -> Result<()> {
    match connection
        .request(Request::GetConversation {
            conversation_id: id.clone(),
        })
        .await?
    {
        Response::Conversation { summary, messages } => {
            if let Some(workspace) = &summary.workspace {
                app.workspace = workspace.clone();
            }
            app.replace_transcript(messages);
            // When viewing a child, surface its parent for easy navigation.
            if let Some(ref parent_id) = summary.parent_conversation_id {
                app.push(
                    LineKind::Notice,
                    format!("↑ child of parent conversation — /open {parent_id} to return"),
                );
            }
            app.set_conversation_summary(summary);
            app.set_status("loaded conversation");
        }
        Response::Error { message, .. } => app.report_error(message),
        other => app.report_error(format!("unexpected reply: {other:?}")),
    }
    Ok(())
}

/// Opens a delegated chat as a snapshot over the parent conversation.
///
/// Keeping the parent's live state in place is important: switching the active
/// transcript while its run is streaming would otherwise render parent deltas in
/// the child's chat. Closing this pane therefore needs no reconnect or reload.
async fn open_child_conversation(
    connection: &mut Connection,
    app: &mut App,
    id: &ConversationId,
) -> Result<()> {
    match connection
        .request(Request::GetConversation {
            conversation_id: id.clone(),
        })
        .await?
    {
        Response::Conversation { summary, messages } => {
            let agent = summary.selected_agent_id.as_deref().unwrap_or("subagent");
            let model = summary.selected_model.as_deref().unwrap_or("default");
            let mut lines = vec![
                format!("{agent}/{model} · {} messages", summary.message_count),
                "Snapshot view · close and reopen to refresh live progress.".into(),
                String::new(),
            ];
            for message in messages {
                let label = match message.role.as_str() {
                    "user" => "You".to_string(),
                    "assistant" => format!(
                        "{} / {}",
                        message.agent_id.as_deref().unwrap_or(agent),
                        message.model.as_deref().unwrap_or(model)
                    ),
                    role => role.to_string(),
                };
                lines.push(label);
                if message.text.trim().is_empty() {
                    lines.push("(no text output)".into());
                } else {
                    lines.extend(message.text.lines().map(str::to_string));
                }
                lines.push(String::new());
            }
            let title = summary.title.as_deref().unwrap_or("subagent conversation");
            app.open_text(
                format!("{title} · Esc/Enter back to parent · agents keep running"),
                lines,
            );
        }
        Response::Error { message, .. } => app.report_error(message),
        other => app.report_error(format!("unexpected reply: {other:?}")),
    }
    Ok(())
}

/// Refreshes the conversation list.
async fn refresh_conversations(connection: &mut Connection, app: &mut App) -> Result<()> {
    if let Response::Conversations { conversations } = connection
        .request(Request::ListConversations {
            root: app.workspace.clone(),
        })
        .await?
    {
        app.set_conversation_summaries(conversations);
    }
    Ok(())
}

/// Refreshes title, message count, selection, and live-session badges after a turn.
///
/// The transcript is already streamed locally, so replacing it here would flicker;
/// only the authoritative summary metadata is updated.
async fn refresh_conversation_summary(connection: &mut Connection, app: &mut App) -> Result<()> {
    let Some(id) = app.conversation.as_ref().map(|summary| summary.id.clone()) else {
        return Ok(());
    };
    match connection
        .request(Request::GetConversation {
            conversation_id: id,
        })
        .await?
    {
        Response::Conversation { summary, .. } => app.set_conversation_summary(summary),
        Response::Error { message, .. } => app.report_error(message),
        other => app.report_error(format!("unexpected summary reply: {other:?}")),
    }
    Ok(())
}

/// Follows a run on its own connection, forwarding events to the UI.
///
/// The store is the durable cursor. A transient socket/read failure reconnects
/// and asks only for events after the last delivered sequence instead of silently
/// abandoning the run and leaving the TUI spinning with a completed reply hidden.
fn spawn_stream(
    paths: &ArgoPaths,
    run_id: argo_core::ids::RunId,
    sender: mpsc::UnboundedSender<RunEvent>,
) {
    let socket = paths.socket();
    tokio::spawn(async move {
        let mut after_seq = 0;
        let mut delay = std::time::Duration::from_millis(50);

        loop {
            if sender.is_closed() {
                return;
            }
            let mut connection = match Connection::connect_to(&socket).await {
                Ok(connection) => connection,
                Err(_) => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(1));
                    continue;
                }
            };
            if connection
                .send(Request::Subscribe {
                    run_id: run_id.clone(),
                    after_seq,
                })
                .await
                .is_err()
            {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(1));
                continue;
            }
            delay = std::time::Duration::from_millis(50);

            loop {
                match connection.next_response().await {
                    Ok(Response::Event { event }) => {
                        // Replayed backlogs may overlap the last delivered event
                        // if the socket dropped between reading and reconnecting.
                        if event.seq <= after_seq {
                            continue;
                        }
                        after_seq = event.seq;
                        let terminal = event.is_terminal();
                        // A closed receiver means the UI exited; stop quietly.
                        if sender.send(event).is_err() || terminal {
                            return;
                        }
                    }
                    // StreamEnd without a terminal event can occur if the socket
                    // raced durable finalization. Reconnect and replay the cursor.
                    Ok(Response::StreamEnd { .. }) | Err(_) => break,
                    Ok(_) => continue,
                }
            }

            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(std::time::Duration::from_secs(1));
        }
    });
}

/// Discovers skills for the `/skills` command.
fn argo_resources_discover(
    workspace: &str,
    paths: &ArgoPaths,
    home: Option<&std::path::Path>,
) -> Result<Vec<argo_resources::Skill>> {
    argo_resources::discover(std::path::Path::new(workspace), &paths.user_skills(), home)
}

/// One daemon connection.
struct Connection {
    writer: tokio::net::unix::OwnedWriteHalf,
    reader: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
}

impl Connection {
    /// Connects and completes the handshake, starting the daemon if needed.
    async fn connect(paths: &ArgoPaths) -> Result<Self> {
        match Self::connect_to(&paths.socket()).await {
            Ok(connection) => return Ok(connection),
            Err(error) => {
                if let Some(protocol) = argo_daemon::mismatched_daemon_protocol(&error) {
                    argo_daemon::stop_older_daemon(paths, protocol, "argo-tui").await?;
                }
            }
        }
        // The TUI is often the user's first command, so start the daemon rather
        // than telling them to run something else first.
        spawn_daemon(paths)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Ok(connection) = Self::connect_to(&paths.socket()).await {
                return Ok(connection);
            }
        }
        Err(ArgoError::Process(
            "the argo daemon did not start; run `argo daemon` to see why".into(),
        ))
    }

    async fn connect_to(socket: &std::path::Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| ArgoError::Io(format!("connect: {e}")))?;
        let (read_half, writer) = stream.into_split();
        let mut connection = Self {
            writer,
            reader: BufReader::new(read_half).lines(),
        };
        match connection
            .request(Request::Hello {
                protocol: IPC_PROTOCOL_VERSION,
                client: format!("argo-tui/{}", env!("CARGO_PKG_VERSION")),
            })
            .await?
        {
            Response::Welcome { .. } => Ok(connection),
            Response::Error {
                code,
                message,
                retryable,
            } => Err(ArgoError::remote(code, message, retryable)),
            other => Err(ArgoError::Protocol(format!("bad handshake: {other:?}"))),
        }
    }

    async fn send(&mut self, request: Request) -> Result<()> {
        let line = serde_json::to_string(&request)?;
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| ArgoError::Io(format!("send: {e}")))?;
        Ok(())
    }

    async fn request(&mut self, request: Request) -> Result<Response> {
        self.send(request).await?;
        self.next_response().await
    }

    async fn next_response(&mut self) -> Result<Response> {
        let line = self
            .reader
            .next_line()
            .await
            .map_err(|e| ArgoError::Io(format!("read: {e}")))?
            .ok_or_else(|| ArgoError::Protocol("daemon closed the connection".into()))?;
        serde_json::from_str(&line)
            .map_err(|e| ArgoError::Protocol(format!("malformed reply: {e}")))
    }
}

/// Starts a detached daemon.
fn spawn_daemon(paths: &ArgoPaths) -> Result<()> {
    let exe =
        std::env::current_exe().map_err(|e| ArgoError::Process(format!("locate argo: {e}")))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--data-dir")
        .arg(paths.root())
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
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
        .map_err(|e| ArgoError::Process(format!("start daemon: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queues_advance_after_success_or_cancellation() {
        use argo_core::event::RunStatus;

        assert!(should_drain_queue(RunStatus::Succeeded));
        assert!(!should_drain_queue(RunStatus::Failed));
        assert!(should_drain_queue(RunStatus::Cancelled));
        assert!(!should_drain_queue(RunStatus::Running));
        assert!(!should_drain_queue(RunStatus::Pending));
    }

    #[test]
    fn terminal_screen_never_translates_wheel_motion_into_arrow_keys() {
        let mut output = Vec::new();
        enter_terminal_screen(&mut output).expect("enter screen");
        leave_terminal_screen(&mut output).expect("leave screen");

        let output = String::from_utf8(output).expect("terminal output");
        assert!(!output.contains("\x1b[?1007h"), "{output:?}");
        assert!(!output.contains("\x1b[?1007l"), "{output:?}");
    }

    #[test]
    fn combined_mouse_reporting_includes_button_drag_and_sgr_coordinates() {
        let mut output = Vec::new();
        set_mouse_wheel_reporting(&mut output, true).expect("enable wheel");
        set_mouse_wheel_reporting(&mut output, false).expect("disable wheel");
        let output = String::from_utf8(output).expect("terminal output");

        assert!(output.contains("\x1b[?1000h"), "{output:?}");
        assert!(output.contains("\x1b[?1002h"), "{output:?}");
        assert!(output.contains("\x1b[?1006h"), "{output:?}");
        assert!(output.contains("\x1b[?1006l"), "{output:?}");
        assert!(output.contains("\x1b[?1002l"), "{output:?}");
        assert!(output.contains("\x1b[?1000l"), "{output:?}");
        for invasive in ["\x1b[?1003h", "\x1b[?1015h"] {
            assert!(!output.contains(invasive), "{output:?}");
        }
    }

    #[test]
    fn f2_mode_switch_updates_terminal_and_app_state() {
        let mut app = App::new("/repo");
        let mut output = Vec::new();
        set_mouse_scroll_mode(&mut output, &mut app, true).expect("wheel mode");
        assert!(app.mouse_scroll_mode);
        assert!(app.status.contains("drag selects"));
        set_mouse_scroll_mode(&mut output, &mut app, false).expect("selection mode");
        assert!(!app.mouse_scroll_mode);
        assert!(app.status.contains("native selection"));
    }

    #[test]
    fn visible_drag_selection_preserves_rows_for_clipboard_copy() {
        let screen = ScreenSnapshot {
            area: ratatui::layout::Rect::new(0, 0, 8, 2),
            cells: vec![
                "hello   ".chars().map(|ch| ch.to_string()).collect(),
                "world   ".chars().map(|ch| ch.to_string()).collect(),
            ],
        };
        let selection = MouseSelection {
            anchor: ScreenPoint { column: 1, row: 0 },
            focus: ScreenPoint { column: 3, row: 1 },
            dragging: false,
        };

        assert_eq!(
            screen.selected_text(selection).as_deref(),
            Some("ello\nworl")
        );
    }

    #[test]
    fn terminal_usage_output_is_cleaned_and_reduced_to_the_usage_panel() {
        let raw = "\x1b[2Jstartup screen\r\n\x1b[1m USAGE  Pro Plan\x1b[0m\r\n20% used\r\nUsage limits\r\nWeekly 80% remaining\r\nPress Esc to close\r\n";
        let clean = strip_terminal_sequences(raw);
        let lines = useful_usage_lines(&clean, "Command Code");
        assert!(
            lines.iter().any(|line| line.contains("Pro Plan")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("80% remaining")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("startup screen")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("Press Esc")),
            "{lines:?}"
        );
    }

    #[test]
    fn kiro_native_usage_panel_is_recognized() {
        let raw = "\x1b[1mEstimated Usage\x1b[0m | resets on 2026-09-01 | KIRO POWER\nCredits (285.33 of 10000 covered in plan)\n2%\nTip: to see context window usage, run /context\n";
        let clean = strip_terminal_sequences(raw);
        let lines = useful_usage_lines(&clean, "Kiro");
        assert!(lines.iter().any(|line| line.contains("Estimated Usage")));
        assert!(lines.iter().any(|line| line.contains("Credits")));
    }

    #[test]
    fn arrows_recall_user_prompts_without_scrolling_the_transcript() {
        let mut app = App::new("/repo");
        for prompt in ["first", "second"] {
            for ch in prompt.chars() {
                app.insert(ch);
            }
            app.take_input();
        }
        app.set_scroll_limit(30);
        app.scroll_up(10);

        navigate_vertical(&mut app, true);
        assert_eq!(app.input, "second");
        assert_eq!(app.scroll_back, 10);
        navigate_vertical(&mut app, true);
        assert_eq!(app.input, "first");
        assert_eq!(app.scroll_back, 10);
        navigate_vertical(&mut app, false);
        assert_eq!(app.input, "second");
        assert_eq!(app.scroll_back, 10);
    }

    #[test]
    fn shift_tab_recognizes_legacy_and_enhanced_terminal_encodings() {
        use crossterm::event::KeyEvent;

        assert!(is_mode_cycle_key(&KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::NONE,
        )));
        assert!(is_mode_cycle_key(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT,
        )));
        assert!(is_mode_cycle_key(&KeyEvent::new(
            KeyCode::Char('\t'),
            KeyModifiers::SHIFT,
        )));
        assert!(!is_mode_cycle_key(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn multiline_enter_recovers_shift_when_the_terminal_drops_it() {
        use crossterm::event::KeyEvent;

        let plain = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!is_multiline_enter_with_native_shift(&plain, false));
        assert!(is_multiline_enter_with_native_shift(&plain, true));
        assert!(is_multiline_enter_with_native_shift(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            false,
        ));
        assert!(is_multiline_enter_with_native_shift(
            &KeyEvent::new(KeyCode::Char('\r'), KeyModifiers::SHIFT),
            false,
        ));
        assert!(is_multiline_enter_with_native_shift(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            false,
        ));
        assert!(!is_multiline_enter_with_native_shift(
            &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::SHIFT),
            true,
        ));
    }

    #[test]
    fn mouse_wheel_scrolls_and_link_clicks_resolve_safe_destinations() {
        use crossterm::event::MouseButton;

        let mut app = App::new("/repo");
        app.set_scroll_limit(30);
        app.scroll_up(10);
        let event = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let links = vec![crate::render::NativeHyperlink {
            column: 4,
            row: 7,
            text: "https://example.com".into(),
            url: "https://example.com/report".into(),
        }];
        let screen = ScreenSnapshot::default();

        assert!(handle_mouse(
            event(MouseEventKind::ScrollUp, 1, 1),
            &mut app,
            &links,
            &screen
        )
        .is_none());
        assert_eq!(app.scroll_back, 13);
        assert!(handle_mouse(
            event(MouseEventKind::ScrollDown, 1, 1),
            &mut app,
            &links,
            &screen
        )
        .is_none());
        assert_eq!(app.scroll_back, 10);
        assert!(handle_mouse(
            event(MouseEventKind::Down(MouseButton::Left), 8, 7),
            &mut app,
            &links,
            &screen,
        )
        .is_none());
        assert_eq!(
            handle_mouse(
                event(MouseEventKind::Up(MouseButton::Left), 8, 7),
                &mut app,
                &links,
                &screen,
            )
            .as_deref(),
            Some("https://example.com/report")
        );
        assert!(handle_mouse(
            event(MouseEventKind::Down(MouseButton::Left), 3, 7),
            &mut app,
            &links,
            &screen,
        )
        .is_none());
    }

    #[test]
    fn browser_opening_accepts_only_http_and_https() {
        assert!(is_safe_web_url("https://example.com/report"));
        assert!(is_safe_web_url("http://localhost:3000/path"));
        assert!(!is_safe_web_url("file:///tmp/secret"));
        assert!(!is_safe_web_url("javascript:alert(1)"));
        assert!(!is_safe_web_url("https://"));
        assert!(!is_safe_web_url("https://example.com/has space"));
        assert_eq!(
            open_web_url("file:///tmp/secret").expect_err("unsafe scheme"),
            "refusing to open a non-HTTP(S) link"
        );
    }

    #[test]
    fn native_hyperlink_writer_emits_osc8_without_mouse_events() {
        let hyperlink = crate::render::NativeHyperlink {
            column: 4,
            row: 7,
            text: "https://example.com/report".into(),
            url: "https://example.com/report/123".into(),
        };
        let mut output = Vec::new();

        write_native_hyperlinks(&mut output, &[hyperlink]).expect("write hyperlink");

        let output = String::from_utf8(output).expect("terminal output");
        assert!(
            output.contains(
                "\x1b]8;;https://example.com/report/123\x1b\\https://example.com/report\x1b]8;;\x1b\\"
            ),
            "{output:?}"
        );
    }

    #[tokio::test]
    async fn ready_token_events_are_batched_before_redraw() {
        let run_id = argo_core::ids::RunId::new("burst");
        let (sender, mut receiver) = mpsc::unbounded_channel();
        for seq in 1..=620 {
            sender
                .send(RunEvent::new(
                    run_id.clone(),
                    seq,
                    argo_core::event::RunEventKind::TextDelta { text: "x".into() },
                ))
                .expect("send event");
        }
        let first = receiver.recv().await.expect("first");
        let batch = ready_event_batch(first, &mut receiver, 512);
        assert_eq!(batch.len(), 512);
        let next = receiver.recv().await.expect("next");
        let remainder = ready_event_batch(next, &mut receiver, 512);
        assert_eq!(remainder.len(), 108);
    }

    #[tokio::test]
    async fn stream_follower_reconnects_from_the_last_durable_sequence() {
        use argo_core::event::{RunEventKind, RunStatus, TokenUsage};
        use tokio::net::UnixListener;

        let root = std::env::temp_dir().join(format!(
            "argo-stream-test-{}-{}",
            std::process::id(),
            argo_core::now_millis()
        ));
        std::fs::create_dir_all(&root).expect("create test root");
        let paths = ArgoPaths::with_root(&root);
        let socket = paths.socket();
        let listener = UnixListener::bind(&socket).expect("bind test socket");
        let run_id = argo_core::ids::RunId::new("replayed-run");
        let server_run_id = run_id.clone();

        let server = tokio::spawn(async move {
            for (expected_after, event) in [
                (
                    0,
                    RunEvent::new(
                        server_run_id.clone(),
                        1,
                        RunEventKind::TextDelta {
                            text: "persisted reply".into(),
                        },
                    ),
                ),
                (
                    1,
                    RunEvent::new(
                        server_run_id.clone(),
                        2,
                        RunEventKind::RunFinished {
                            status: RunStatus::Succeeded,
                            usage: TokenUsage::default(),
                        },
                    ),
                ),
            ] {
                let (stream, _) = listener.accept().await.expect("accept");
                let (read_half, mut write_half) = stream.into_split();
                let mut lines = BufReader::new(read_half).lines();

                let hello = lines.next_line().await.expect("read hello").expect("hello");
                assert!(matches!(Request::decode(&hello), Ok(Request::Hello { .. })));
                write_half
                    .write_all(
                        Response::Welcome {
                            protocol: IPC_PROTOCOL_VERSION,
                            version: "test".into(),
                            database: "test.sqlite".into(),
                        }
                        .encode()
                        .as_bytes(),
                    )
                    .await
                    .expect("welcome");

                let subscribe = lines
                    .next_line()
                    .await
                    .expect("read subscribe")
                    .expect("subscribe");
                match Request::decode(&subscribe).expect("decode subscribe") {
                    Request::Subscribe { run_id, after_seq } => {
                        assert_eq!(run_id, server_run_id);
                        assert_eq!(after_seq, expected_after);
                    }
                    other => panic!("unexpected request: {other:?}"),
                }
                write_half
                    .write_all(Response::Event { event }.encode().as_bytes())
                    .await
                    .expect("event");
                // First connection drops here without StreamEnd. The follower
                // must reconnect with after_seq=1; the second event is terminal.
            }
        });

        let (sender, mut receiver) = mpsc::unbounded_channel();
        spawn_stream(&paths, run_id, sender);
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
            .await
            .expect("first timeout")
            .expect("first event");
        let second = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
            .await
            .expect("second timeout")
            .expect("second event");
        assert_eq!((first.seq, second.seq), (1, 2));
        assert!(second.is_terminal());
        server.await.expect("server task");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_default_agent_is_cleared_from_memory_and_disk() {
        let root = std::env::temp_dir().join(format!(
            "argo-missing-default-{}-{}",
            std::process::id(),
            argo_core::now_millis()
        ));
        let paths = ArgoPaths::with_root(&root);
        let selection = crate::preferences::DefaultSelection {
            agent: "codex".into(),
            model: "gpt-test".into(),
            effort: Some("high".into()),
        };
        crate::preferences::save(&paths, Some(selection.clone())).expect("save default");

        let mut app = App::new("/repo");
        app.default_selection = Some(selection);
        app.agents.push(argo_runtime::AgentInfo::unavailable(
            argo_runtime::require("codex").expect("codex definition"),
            "not found",
        ));

        assert_eq!(
            clear_default_if_agent_missing(&paths, &mut app).expect("validate default"),
            Some("codex".into())
        );
        assert!(app.default_selection.is_none());
        assert_eq!(
            crate::preferences::load(&paths).expect("load default"),
            None
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agents_picker_marks_the_default_and_keeps_missing_clis_visible() {
        let mut codex = argo_runtime::AgentInfo::unavailable(
            argo_runtime::require("codex").expect("codex definition"),
            "not found",
        );
        codex.available = true;
        codex.version = Some("codex 1.2.3".into());
        let claude = argo_runtime::AgentInfo::unavailable(
            argo_runtime::require("claude").expect("claude definition"),
            "not found",
        );

        let mut app = App::new("/repo");
        app.agents = vec![codex, claude];
        app.default_selection = Some(crate::preferences::DefaultSelection {
            agent: "codex".into(),
            model: "gpt-test".into(),
            effort: None,
        });
        open_agents_picker(&mut app);

        match &app.overlay {
            crate::app::Overlay::Picker { action, items, .. } => {
                assert_eq!(*action, PickerAction::Agents);
                assert!(items.iter().any(|item| item.contains("★ default")));
                let codex = items
                    .iter()
                    .find(|item| item.contains("Codex"))
                    .expect("Codex row");
                assert!(codex.contains("1.2.3"), "{codex}");
                assert!(!codex.contains("model"), "{codex}");
                assert!(!codex.contains("delegation"), "{codex}");
                assert!(items
                    .iter()
                    .any(|item| item.contains("Claude") && item.contains("not detected")));
            }
            other => panic!("unexpected overlay: {other:?}"),
        }
    }

    #[test]
    fn restoring_the_terminal_twice_is_safe() {
        // Both the normal exit path and the panic hook call this.
        restore_terminal();
        restore_terminal();
    }

    #[test]
    fn project_instructions_menu_enables_and_disables_without_deleting_the_file() {
        let root = std::env::temp_dir().join(format!(
            "argo-instructions-ui-{}-{}",
            std::process::id(),
            argo_core::now_millis()
        ));
        std::fs::create_dir_all(&root).expect("workspace");
        let mut app = App::new(root.to_string_lossy());

        run_instructions_command(&mut app, commands::InstructionsCommand::Menu).expect("menu");
        assert!(matches!(
            app.overlay,
            crate::app::Overlay::Picker {
                action: PickerAction::Instructions,
                ..
            }
        ));
        app.close_overlay();

        run_instructions_command(&mut app, commands::InstructionsCommand::Enable).expect("enable");
        let file = argo_resources::instructions::instructions_path(&root);
        assert!(file.is_file());
        assert!(argo_resources::instructions::is_enabled(&root));

        run_instructions_command(&mut app, commands::InstructionsCommand::Disable)
            .expect("disable");
        assert!(!argo_resources::instructions::is_enabled(&root));
        assert!(file.is_file());
        let _ = std::fs::remove_dir_all(root);
    }
}
