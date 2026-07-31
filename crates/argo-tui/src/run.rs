//! The TUI event loop.
//!
//! Terminal input, daemon replies, and streamed run events are multiplexed in one
//! `select!`. The terminal is restored on every exit path, including a panic:
//! leaving a user's shell in raw mode with no cursor is worse than any error
//! message.

use crate::app::{App, EnterAction, LineKind, PickerAction};
use crate::commands::{self, Command};
use argo_core::error::{ArgoError, Result};
use argo_core::event::RunEvent;
use argo_core::ids::ConversationId;
use argo_core::{ArgoPaths, IPC_PROTOCOL_VERSION};
use argo_daemon::protocol::{Request, Response};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Restores the terminal, tolerating already-restored state.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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
    let mut connection = Connection::connect(paths).await?;
    let mut app = App::new(workspace.clone());

    // Open the workspace before drawing so the first frame is already populated.
    match connection
        .request(Request::OpenWorkspace {
            root: workspace.clone(),
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

    // Always open a fresh session. Reopening the last conversation silently
    // continues work the user may not have meant to resume; `/resume` is explicit.
    new_conversation(&mut connection, &mut app, None).await?;

    // Any panic must not leave the terminal unusable.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let mut terminal_guard = TerminalRestoreGuard(true);
    enable_raw_mode().map_err(|e| ArgoError::Io(format!("enable raw mode: {e}")))?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|e| ArgoError::Io(format!("enter alternate screen: {e}")))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal =
        Terminal::new(backend).map_err(|e| ArgoError::Io(format!("create terminal: {e}")))?;

    let result = event_loop(&mut terminal, &mut connection, &mut app, paths).await;

    restore_terminal();
    terminal_guard.0 = false;
    let _ = terminal.show_cursor();

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

    loop {
        terminal
            .draw(|frame| crate::render::draw(frame, app))
            .map_err(|e| ArgoError::Io(format!("draw: {e}")))?;

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
            // Streamed run events take priority so output stays responsive.
            Some(event) = event_rx.recv() => {
                let terminal_status = match &event.kind {
                    argo_core::event::RunEventKind::RunFinished { status, .. } => Some(*status),
                    _ => None,
                };
                app.apply_event(event.kind);

                if let Some(status) = terminal_status {
                    let offer_options = status == argo_core::event::RunStatus::Succeeded
                        && app.queue_depth() == 0;
                    refresh_conversation_summary(connection, app).await?;
                    if should_drain_queue(status) {
                        try_start_next_queued(connection, app, paths, &event_tx).await?;
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
            }
            _ = animation => {
                app.advance_tick();
            }
            maybe_key = keys.next() => {
                match maybe_key {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(key, connection, app, paths, &event_tx).await?;
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        let area = terminal
                            .size()
                            .map_err(|error| ArgoError::Io(format!("terminal size: {error}")))?;
                        handle_mouse(mouse, area.width, area.height, app);
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

fn handle_mouse(mouse: MouseEvent, width: u16, height: u16, app: &mut App) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if app.has_overlay() {
                app.overlay_move(-3);
            } else {
                app.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if app.has_overlay() {
                app.overlay_move(3);
            } else {
                app.scroll_down(3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) if is_open_gesture(&mouse) => {
            let Some(url) = crate::render::url_at(app, width, height, mouse.column, mouse.row)
            else {
                app.set_status("no URL under pointer");
                return;
            };
            match open_url(&url) {
                Ok(()) => app.set_status(format!("opened {url}")),
                Err(error) => app.report_error(error),
            }
        }
        _ => {}
    }
}

fn is_open_gesture(mouse: &MouseEvent) -> bool {
    mouse.kind == MouseEventKind::Down(MouseButton::Left)
        && (mouse.modifiers.contains(KeyModifiers::SUPER)
            || mouse.modifiers.contains(KeyModifiers::CONTROL))
}

fn open_url(url: &str) -> std::result::Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("only http:// and https:// links can be opened".into());
    }

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err("opening links is supported on macOS and Linux".into());

    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open URL: {error}"))
}

/// Handles one key press.
async fn handle_key(
    key: crossterm::event::KeyEvent,
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    event_tx: &mpsc::UnboundedSender<RunEvent>,
) -> Result<()> {
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
                    apply_choice(connection, app, paths, event_tx, action, value).await?
                }
                // A read-only pane has nothing to choose, so Enter dismisses it
                // rather than appearing to do nothing.
                None => app.close_overlay(),
            },
            // Typing narrows a picker, which is the only practical way through a
            // list of several hundred models.
            KeyCode::Backspace => app.picker_filter_pop(),
            KeyCode::Char(ch) => app.picker_filter_push(ch),
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.input.is_empty() {
                app.should_quit = true;
            }
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
                let dropped = app.clear_queue();
                if dropped > 0 {
                    app.push(
                        LineKind::Notice,
                        format!("discarded {dropped} queued message(s)"),
                    );
                }
            }
        }
        // Shift+Enter and Ctrl+J insert a line break instead of submitting, which
        // is what a multi-paragraph prompt or a pasted stack trace needs.
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.insert_newline();
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
            app.insert_newline();
        }
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
                    if !app.is_busy() && app.queue_depth() > 0 {
                        try_start_next_queued(connection, app, paths, event_tx).await?;
                    }
                    return Ok(());
                }
                submit(connection, app, paths, line, event_tx).await?;
            }
        },
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Home => app.move_home(),
        KeyCode::End => app.move_end(),
        KeyCode::Up => {
            if app.has_completions() {
                app.completion_move(-1);
            } else {
                app.history_previous();
            }
        }
        KeyCode::Down => {
            if app.has_completions() {
                app.completion_move(1);
            } else {
                app.history_next();
            }
        }
        KeyCode::PageUp => app.scroll_up(10),
        KeyCode::PageDown => app.scroll_down(10),
        // Shift+Tab cycles execution mode, matching the convention other coding
        // TUIs use for plan mode.
        KeyCode::BackTab => {
            cycle_mode(connection, app).await?;
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            cycle_mode(connection, app).await?;
        }
        KeyCode::Tab => {
            // Accept the highlighted suggestion; the popup already shows it.
            app.accept_completion();
        }
        KeyCode::Char(ch) => app.insert(ch),
        _ => {}
    }
    Ok(())
}

/// Routes a submitted line to a command or a message.
async fn submit(
    connection: &mut Connection,
    app: &mut App,
    paths: &ArgoPaths,
    line: String,
    event_tx: &mpsc::UnboundedSender<RunEvent>,
) -> Result<()> {
    let line = if commands::is_command(&line) {
        match commands::parse(&line) {
            Ok(command) => match run_command(connection, app, paths, command).await? {
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

    // A queue retained after an error has priority over newly typed text. Append
    // the new text and restart the oldest item, preserving FIFO ordering.
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
                    let version = info
                        .version
                        .clone()
                        .unwrap_or_else(|| "not installed".into());
                    format!("{mark} {:<10} {version}", info.id)
                })
                .collect();
            let values = app.agents.iter().map(|info| info.id.clone()).collect();
            app.open_picker("switch agent", items, values, PickerAction::Agent);
        }
        Command::Agent(Some(id)) => match commands::resolve_agent(&id) {
            Ok(agent) => select(connection, app, commands::agent_change(agent)).await?,
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
            match agent.and_then(|id| app.agents.iter().find(|a| a.id == id)) {
                Some(info) => {
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
                None => app.report_error("no agent is selected yet; use /agent first"),
            }
        }
        Command::Model(Some(model)) => {
            select(connection, app, commands::model_change(model)).await?
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

            match selected.and_then(|id| app.agents.iter().find(|a| a.id == id)) {
                Some(info) => {
                    // Levels are per model: `gpt-5.6-sol` accepts six, another
                    // model may accept one, and Claude exposes none.
                    let levels = info.reasoning_for(model.as_deref());
                    if levels.is_empty() {
                        app.report_error(format!("{} does not expose reasoning levels", info.id));
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

        Command::Agents => {
            let mut lines = Vec::new();
            for info in &app.agents {
                let mark = if info.available { "✓" } else { "·" };
                lines.push(format!(
                    "{mark} {} {}",
                    info.id,
                    info.version
                        .clone()
                        .unwrap_or_else(|| "not installed".into())
                ));
                if info.available {
                    let models: Vec<&str> =
                        info.models.iter().map(|m| m.id.as_str()).take(8).collect();
                    lines.push(format!("    models: {}", models.join(", ")));
                }
                for diagnostic in &info.diagnostics {
                    lines.push(format!("    {diagnostic}"));
                }
            }
            app.open_text("detected agents", lines);
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

        Command::Mcp => {
            let path = paths.root().join("mcp.json");
            match argo_resources::McpRegistry::load(&path) {
                Ok(registry) if registry.servers.is_empty() => app.open_text(
                    "mcp servers",
                    vec![
                        "No MCP servers configured.".to_string(),
                        format!("Add them to {}", path.display()),
                        "Every agent that supports MCP receives them automatically.".to_string(),
                    ],
                ),
                Ok(registry) => {
                    let lines = registry
                        .servers
                        .iter()
                        .map(|server| {
                            let state = if server.enabled {
                                "enabled"
                            } else {
                                "disabled"
                            };
                            let transport = match &server.transport {
                                argo_resources::McpTransport::Local { command, .. } => {
                                    command.join(" ")
                                }
                                argo_resources::McpTransport::Remote { url, .. } => url.clone(),
                            };
                            format!("{:<20} {state:<9} {transport}", server.name)
                        })
                        .collect();
                    app.open_text("mcp servers", lines);
                }
                Err(error) => app.report_error(error.to_string()),
            }
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

        Command::New(title) => new_conversation(connection, app, title).await?,

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
                    let lines = children
                        .iter()
                        .map(|child| {
                            format!(
                                "{}  {}",
                                child.id,
                                child.title.clone().unwrap_or_else(|| "(untitled)".into())
                            )
                        })
                        .collect();
                    app.open_text("subagents — /open <id> to view", lines);
                }
                Response::Error { message, .. } => app.report_error(message),
                other => app.report_error(format!("unexpected reply: {other:?}")),
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
                        info.version
                            .clone()
                            .unwrap_or_else(|| "not installed".into())
                    ));
                }
                app.agents = agents;
            }
            app.open_text("doctor", lines);
        }
    }
    Ok(None)
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
    action: PickerAction,
    value: String,
) -> Result<()> {
    match action {
        PickerAction::Agent => match commands::resolve_agent(&value) {
            Ok(agent) => select(connection, app, commands::agent_change(agent)).await,
            Err(message) => {
                app.report_error(message);
                Ok(())
            }
        },
        PickerAction::Model => select(connection, app, commands::model_change(value)).await,
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
        PickerAction::Mode => set_mode(connection, app, Some(value)).await,
        PickerAction::Conversation => {
            let id = ConversationId::new(value);
            load_conversation(connection, app, &id).await
        }
        PickerAction::ResponseOption => submit(connection, app, paths, value, event_tx).await,
    }
}

/// Records a selection change and reports what it will do.
async fn select(
    connection: &mut Connection,
    app: &mut App,
    change: argo_core::session::SelectionChange,
) -> Result<()> {
    let Some(conversation) = app.conversation.as_ref().map(|c| c.id.clone()) else {
        app.report_error("no conversation is open");
        return Ok(());
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
            // Selections apply at the next turn, never to a running child.
            app.push(
                LineKind::Notice,
                format!("· {label} — applies to your next message"),
            );
            app.set_status(format!("switched to {label}"));
        }
        Response::Error { message, .. } => app.report_error(message),
        other => app.report_error(format!("unexpected reply: {other:?}")),
    }
    Ok(())
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
            app.lines.clear();
            app.set_conversation_summary(summary);
            app.set_status("new conversation");
        }
        Response::Error { message, .. } => app.report_error(message),
        other => app.report_error(format!("unexpected reply: {other:?}")),
    }
    Ok(())
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
            app.replace_transcript(messages);
            app.set_conversation_summary(summary);
            app.set_status("loaded conversation");
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
fn spawn_stream(
    paths: &ArgoPaths,
    run_id: argo_core::ids::RunId,
    sender: mpsc::UnboundedSender<RunEvent>,
) {
    let socket = paths.socket();
    tokio::spawn(async move {
        let Ok(mut connection) = Connection::connect_to(&socket).await else {
            return;
        };
        if connection
            .send(Request::Subscribe {
                run_id: run_id.clone(),
                after_seq: 0,
            })
            .await
            .is_err()
        {
            return;
        }
        loop {
            match connection.next_response().await {
                Ok(Response::Event { event }) => {
                    // A closed receiver means the UI exited; stop quietly.
                    if sender.send(event).is_err() {
                        return;
                    }
                }
                Ok(Response::StreamEnd { .. }) | Err(_) => return,
                Ok(_) => continue,
            }
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
        if let Ok(connection) = Self::connect_to(&paths.socket()).await {
            return Ok(connection);
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
    fn command_click_is_the_only_url_open_gesture() {
        let mouse = |kind, modifiers| MouseEvent {
            kind,
            column: 4,
            row: 7,
            modifiers,
        };

        assert!(is_open_gesture(&mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::SUPER,
        )));
        assert!(is_open_gesture(&mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::CONTROL,
        )));
        assert!(!is_open_gesture(&mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
        )));
        assert!(!is_open_gesture(&mouse(
            MouseEventKind::Down(MouseButton::Right),
            KeyModifiers::SUPER,
        )));
        assert!(!is_open_gesture(&mouse(
            MouseEventKind::Up(MouseButton::Left),
            KeyModifiers::SUPER,
        )));
    }

    #[test]
    fn opener_rejects_non_web_schemes_before_spawning() {
        assert_eq!(
            open_url("file:///tmp/report").unwrap_err(),
            "only http:// and https:// links can be opened"
        );
        assert_eq!(
            open_url("javascript:alert(1)").unwrap_err(),
            "only http:// and https:// links can be opened"
        );
    }

    #[test]
    fn restoring_the_terminal_twice_is_safe() {
        // Both the normal exit path and the panic hook call this.
        restore_terminal();
        restore_terminal();
    }
}
