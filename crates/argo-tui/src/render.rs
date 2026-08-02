//! Rendering.
//!
//! One frame is: a header naming the workspace and current selection, the
//! transcript (or an overlay), a composer, and a status line. Layout is computed
//! from the terminal size each frame so a resize needs no special handling.

use crate::app::{Activity, App, LineKind, Overlay, PickerAction};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

/// Most composer lines shown before it stops growing.
const MAX_COMPOSER_LINES: usize = 8;

/// Accent colour used for Argo's own chrome.
const ACCENT: Color = Color::Rgb(94, 200, 255);
/// Colour for Argo's own notices.
const NOTICE: Color = Color::Rgb(255, 196, 92);
/// Muted colour for secondary detail.
const MUTED: Color = Color::Rgb(120, 130, 145);

/// A stable colour per agent, so a multi-agent transcript is scannable.
///
/// Derived from the id rather than a fixed table, so a newly added adapter gets a
/// colour without touching this function.
fn agent_color(agent: &str) -> Color {
    match agent {
        "claude" => Color::Rgb(217, 138, 87),
        "codex" => Color::Rgb(128, 208, 160),
        "opencode" => Color::Rgb(168, 160, 255),
        "kiro" => Color::Rgb(120, 190, 255),
        "grok" => Color::Rgb(230, 130, 180),
        other => {
            // Cheap stable hash so unknown agents still differ from each other.
            let hash = other.bytes().fold(17u32, |acc, byte| {
                acc.wrapping_mul(31).wrapping_add(byte as u32)
            });
            Color::Indexed(((hash % 200) + 20) as u8)
        }
    }
}

/// Extracts the agent name from an `agent · model · mode` header line.
fn header_agent(text: &str) -> &str {
    text.split('·').next().unwrap_or(text).trim()
}

/// A bordered block in Argo's style.
fn panel(title: impl Into<String>, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
        .title(Span::styled(
            title.into(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

/// Draws one frame.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    // Size from visual rows, not just explicit newlines. A long prompt in a narrow
    // terminal must grow the composer as it wraps instead of painting each new
    // character over the only allocated content row.
    let composer_width = frame.area().width.saturating_sub(2).max(1) as usize;
    let visual_rows = wrap_composer(&app.input, app.cursor, composer_width)
        .lines
        .len();
    let composer_rows = (visual_rows.min(MAX_COMPOSER_LINES) as u16) + 2;
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            // Header, body, composer, status.
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(composer_rows),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_header(frame, areas[0], app);
    draw_body(frame, areas[1], app);
    draw_composer(frame, areas[2], app);
    draw_status(frame, areas[3], app);

    // Drawn last so it floats above the transcript, anchored to the composer.
    draw_completions(frame, areas[1], areas[2], app);
    draw_mouse_selection(frame.buffer_mut(), app);
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = app
        .conversation
        .as_ref()
        .and_then(|c| c.title.clone())
        .unwrap_or_else(|| "new conversation".to_string());

    let agent = app
        .conversation
        .as_ref()
        .and_then(|c| c.selected_agent_id.clone())
        .unwrap_or_else(|| "auto".to_string());

    let mut spans = vec![
        Span::styled(
            " ARGO ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        // The active selection is the single most consulted fact on screen.
        Span::styled(
            format!(" {} ", app.selection_label()),
            Style::default()
                .fg(Color::Black)
                .bg(agent_color(&agent))
                .add_modifier(Modifier::BOLD),
        ),
    ];

    // Which CLIs already hold a live session here, which is what makes switching
    // back cheap.
    if let Some(conversation) = &app.conversation {
        if !conversation.agents_with_sessions.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled("live: ", Style::default().fg(MUTED)));
            for (index, held) in conversation.agents_with_sessions.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" ", Style::default().fg(MUTED)));
                }
                spans.push(Span::styled(
                    held.clone(),
                    Style::default().fg(agent_color(held)),
                ));
            }
        }
    }

    let (active_children, known_children) = app.delegated_agent_counts();
    if known_children > 0 {
        spans.push(Span::raw("  "));
        let delegated = if active_children > 0 {
            format!("agents: {active_children} active/{known_children} · /children")
        } else {
            format!("agents: {known_children} · /children")
        };
        spans.push(Span::styled(delegated, Style::default().fg(NOTICE)));
    }

    if let Some(version) = &app.available_update {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("update v{version} · /update"),
            Style::default()
                .fg(Color::Black)
                .bg(NOTICE)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        shorten_path(&app.workspace, 34),
        Style::default().fg(MUTED),
    ));
    frame.render_widget(Paragraph::new(TextLine::from(spans)), area);
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, app: &App) {
    match &app.overlay {
        Overlay::None => draw_transcript(frame, area, app),
        Overlay::Picker {
            title,
            selected,
            filter,
            action,
            ..
        } => {
            let matches = app.picker_matches();
            if matches!(
                action,
                PickerAction::StartupAgent
                    | PickerAction::StartupModel
                    | PickerAction::StartupEffort
            ) {
                draw_welcome_picker(frame, area, title, app, &matches, *selected, filter);
            } else {
                draw_picker(frame, area, title, app, &matches, *selected, filter)
            }
        }
        Overlay::Text {
            title,
            lines,
            scroll,
        } => draw_text_overlay(frame, area, title, lines, *scroll),
        Overlay::Input {
            title,
            prompt,
            value,
            secret,
            ..
        } => draw_input_overlay(frame, area, title, prompt, value, *secret),
    }
}

fn draw_input_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    prompt: &str,
    value: &str,
    secret: bool,
) {
    frame.render_widget(Clear, area);
    let shown = if secret {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let lines = vec![
        TextLine::from(Span::styled(prompt, Style::default().fg(MUTED))),
        TextLine::from(""),
        TextLine::from(Span::styled(
            format!("> {shown}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        TextLine::from(""),
        TextLine::from(Span::styled(
            "Paste is supported · Enter continues · Esc cancels",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(panel(format!(" {title} "), true)),
        area,
    );
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let height = area.height.saturating_sub(2) as usize;
    // Minus the two border columns.
    let inner_width = area.width.saturating_sub(2) as usize;

    let mut rendered: Vec<TextLine<'_>> = Vec::new();
    let mut collapsed_thinking = false;
    for line in &app.lines {
        if !app.thinking_visible && line.kind == LineKind::Thinking {
            if !collapsed_thinking {
                let marker = crate::app::Line {
                    kind: LineKind::Activity,
                    text: "◌ thinking hidden · Ctrl+T or /thinking show".into(),
                };
                rendered.extend(render_line(&marker, inner_width));
            }
            collapsed_thinking = true;
            continue;
        }
        collapsed_thinking = false;
        rendered.extend(render_line(line, inner_width));
    }

    // Keep a truthful, animated activity row beside the live transcript. Actual
    // thinking text is rendered above from ThinkingDelta events; this row only
    // labels observable stream state and disappears when the run finishes.
    if let Some(indicator) = app.activity_indicator() {
        let marker = match app.activity {
            Activity::Starting | Activity::Thinking => "◌ ",
            Activity::Responding => "│ ",
            Activity::Working => "↳ ",
            Activity::Idle => "  ",
        };
        rendered.push(TextLine::from(vec![
            Span::styled(marker, Style::default().fg(ACCENT)),
            Span::styled(indicator, Style::default().fg(MUTED)),
        ]));
        rendered.push(TextLine::from(vec![
            Span::styled("? ", Style::default().fg(NOTICE)),
            Span::styled(
                format!("tip · {}", app.shortcut_tip()),
                Style::default().fg(MUTED),
            ),
        ]));
    }

    if rendered.is_empty() {
        // An empty conversation is the only place Argo can explain itself.
        let agents: Vec<String> = app
            .agents
            .iter()
            .filter(|info| info.available)
            .map(|info| match crate::app::agent_display_version(info) {
                Some(version) => format!("{:<10} {version}", info.name),
                None => info.name.clone(),
            })
            .collect();
        // `splash` returns owned lines, so they coerce into the borrowed vector.
        let default_label = app
            .default_selection
            .as_ref()
            .map(|selection| selection.label());
        rendered.extend(crate::banner::splash_with_selection(
            area.width.saturating_sub(4),
            &app.version,
            &agents,
            default_label.as_deref(),
        ));
    }

    // Keep the newest content visible unless the user has scrolled back.
    //
    // Lines were wrapped to `inner_width` as they were built, so one entry is
    // exactly one row on screen. Letting the widget wrap instead would make the
    // count disagree with what is drawn, and any under-count scrolls the newest
    // text — normally the agent's reply — off the bottom of the pane.
    let total = rendered.len();
    let max_scroll = total.saturating_sub(height);
    app.set_scroll_limit(max_scroll);
    let scroll_back = app.scroll_back.min(max_scroll);
    let offset = max_scroll.saturating_sub(scroll_back);

    let mouse_hint = if app.mouse_scroll_mode {
        "wheel · drag select+copy · F2 native"
    } else {
        "native select · F2 wheel + drag"
    };
    let title = if scroll_back > 0 {
        format!(" conversation — scrolled back {scroll_back} rows · PgDn/End · {mouse_hint} ")
    } else {
        format!(" conversation — PgUp · {mouse_hint} ")
    };
    // Slice to the visible rows instead of passing a potentially overflowing
    // usize through Paragraph's u16 scroll offset. This also avoids asking the
    // widget to walk a huge off-screen transcript on every animation frame.
    let visible = rendered
        .into_iter()
        .skip(offset)
        .take(height)
        .collect::<Vec<_>>();
    let paragraph = Paragraph::new(visible).block(panel(title, false));
    frame.render_widget(paragraph, area);
}

/// Breaks `text` into chunks no wider than `width`, preferring word boundaries.
///
/// Argo wraps the transcript itself rather than delegating to the widget so that a
/// rendered entry is exactly one screen row. That keeps the scroll offset in step
/// with what is drawn, and allows continuation rows to be indented under their
/// prefix instead of starting flush against the border.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split(' ') {
        let word_width = word.chars().count();

        // A word longer than the pane can never fit; split it rather than let it
        // overflow and be clipped.
        if word_width > width {
            if current_width > 0 {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                if chunk.chars().count() == width {
                    rows.push(std::mem::take(&mut chunk));
                }
                chunk.push(ch);
            }
            if !chunk.is_empty() {
                current = chunk;
                current_width = current.chars().count();
            }
            continue;
        }

        let needed = if current_width == 0 {
            word_width
        } else {
            current_width + 1 + word_width
        };
        if needed > width {
            rows.push(std::mem::take(&mut current));
            current.push_str(word);
            current_width = word_width;
        } else {
            if current_width > 0 {
                current.push(' ');
            }
            current.push_str(word);
            current_width = needed;
        }
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}

/// Styles one transcript line, wrapped to `inner_width`.
fn render_line(line: &crate::app::Line, inner_width: usize) -> Vec<TextLine<'static>> {
    if line.kind == LineKind::Assistant {
        let style = Style::default().fg(Color::Rgb(228, 228, 235));
        return crate::markdown::render(&line.text, "│ ", style, inner_width);
    }

    let (prefix, style) = match line.kind {
        LineKind::User => (
            "▌ ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        // A visible rail keeps a multi-line response distinct from tool activity and
        // from the next user turn. Explicit colour avoids terminal themes where
        // Color::Reset is nearly the background.
        LineKind::Assistant => ("│ ", Style::default().fg(Color::Rgb(228, 228, 235))),
        LineKind::Thinking => (
            "◌ ",
            Style::default()
                .fg(Color::Rgb(145, 150, 170))
                .add_modifier(Modifier::ITALIC),
        ),
        // Coloured by agent so a transcript spanning several CLIs is scannable.
        LineKind::AgentHeader => (
            "◆ ",
            Style::default()
                .fg(agent_color(header_agent(&line.text)))
                .add_modifier(Modifier::BOLD),
        ),
        LineKind::Activity => ("  ", Style::default().fg(MUTED)),
        LineKind::Notice => ("  ", Style::default().fg(NOTICE)),
        LineKind::Error => ("  ", Style::default().fg(Color::Rgb(255, 110, 110))),
    };

    let mut out: Vec<TextLine<'static>> = Vec::new();
    // A blank row above each turn header; without it a reply and the next prompt
    // run together into one block that is hard to read.
    if line.kind == LineKind::AgentHeader {
        out.push(TextLine::from(""));
    }

    // Continuation rows line up under the text, not under the marker.
    let indent: String = " ".repeat(prefix.chars().count());
    let text_width = inner_width.saturating_sub(prefix.chars().count());

    // Embedded newlines are hard breaks and are preserved.
    for paragraph in line.text.split('\n') {
        for (index, row) in wrap_words(paragraph, text_width).into_iter().enumerate() {
            let marker = if index == 0 {
                prefix.to_string()
            } else {
                indent.clone()
            };
            out.push(TextLine::from(vec![
                Span::styled(marker, style),
                Span::styled(row, style),
            ]));
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn draw_welcome_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    app: &App,
    matches: &[usize],
    selected: usize,
    filter: &str,
) {
    let header_height = area.height.saturating_sub(5).min(8);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_height), Constraint::Min(5)])
        .split(area);
    let header = crate::banner::welcome_header(areas[0].width, &app.version);
    frame.render_widget(Paragraph::new(header), areas[0]);
    draw_picker(frame, areas[1], title, app, matches, selected, filter);
}

/// Highlights the terminal cells selected by Argo's combined mouse mode.
fn draw_mouse_selection(buffer: &mut Buffer, app: &App) {
    if !app.mouse_scroll_mode {
        return;
    }
    let Some(selection) = app.mouse_selection else {
        return;
    };
    let (start, end) = selection.ordered();
    let area = buffer.area;
    for row in start.row..=end.row {
        if row < area.top() || row >= area.bottom() {
            continue;
        }
        let left = if row == start.row {
            start.column
        } else {
            area.left()
        };
        let right = if row == end.row {
            end.column
        } else {
            area.right().saturating_sub(1)
        };
        for column in left..=right {
            if let Some(cell) = buffer.cell_mut((column, row)) {
                cell.set_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Rgb(170, 210, 235)),
                );
            }
        }
    }
}

fn draw_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    app: &App,
    matches: &[usize],
    selected: usize,
    filter: &str,
) {
    frame.render_widget(Clear, area);

    let all_items = match &app.overlay {
        Overlay::Picker { items, .. } => items.as_slice(),
        _ => &[],
    };

    // Only the visible window is built: a provider list can be several hundred
    // rows, and rendering all of them every frame is wasted work.
    let height = area.height.saturating_sub(2) as usize;
    let first = selected.saturating_sub(height.saturating_sub(1).min(selected));

    let rows: Vec<ListItem<'_>> = matches
        .iter()
        .enumerate()
        .skip(first)
        .take(height.max(1))
        .map(|(position, item_index)| {
            let label = all_items
                .get(*item_index)
                .map(String::as_str)
                .unwrap_or_default();
            let style = if position == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(TextLine::from(Span::styled(format!(" {label} "), style)))
        })
        .collect();

    let rows = if rows.is_empty() {
        vec![ListItem::new(TextLine::from(Span::styled(
            " no match — keep typing or Esc to cancel ",
            Style::default().fg(Color::Yellow),
        )))]
    } else {
        rows
    };

    let heading = if filter.is_empty() {
        let controls = match &app.overlay {
            Overlay::Picker {
                action: PickerAction::StartupAgent,
                ..
            } => "Enter once · Space default · type to filter",
            Overlay::Picker {
                action: PickerAction::Agents,
                ..
            } => "Enter switch · Space set default · Del clear · type to filter",
            _ => "type to filter, Enter, Esc",
        };
        format!(" {title} — {} items · {controls} ", matches.len())
    } else {
        format!(
            " {title} — filter '{filter}' · {} of {} ",
            matches.len(),
            all_items.len()
        )
    };

    frame.render_widget(List::new(rows).block(panel(heading, true)), area);
}

/// Floats live command suggestions just above the composer.
fn draw_completions(frame: &mut Frame<'_>, body: Rect, composer: Rect, app: &App) {
    if app.completions.is_empty() || app.has_overlay() {
        return;
    }

    let rows = app.completions.len().min(8) as u16;
    let width = 48u16.min(body.width);
    let height = rows + 2;
    if body.height < height {
        return;
    }

    // Anchored to the composer's top edge so it reads as part of the input.
    let area = Rect {
        x: composer.x,
        y: composer.y.saturating_sub(height),
        width,
        height,
    };

    frame.render_widget(Clear, area);
    // Scroll the window so the highlighted entry stays visible in a long list.
    let visible = 8usize;
    let first = app
        .completion_index
        .saturating_sub(visible.saturating_sub(1));

    let items: Vec<ListItem<'_>> = app
        .completions
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(index, name)| {
            let selected = index == app.completion_index;
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ACCENT)
            };
            let marker = if selected { "›" } else { " " };
            ListItem::new(TextLine::from(Span::styled(
                format!("{marker} {name} "),
                style,
            )))
        })
        .collect();

    frame.render_widget(
        List::new(items).block(panel(
            " ↑↓ pick · Tab complete · Enter run · Esc dismiss ",
            true,
        )),
        area,
    );
}

fn draw_text_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    lines: &[String],
    scroll: usize,
) {
    frame.render_widget(Clear, area);
    let body: Vec<TextLine<'_>> = lines
        .iter()
        .map(|line| TextLine::from(line.as_str()))
        .collect();
    let paragraph = Paragraph::new(body)
        .block(panel(format!(" {title} — ↑↓ scroll · Esc close "), true))
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

fn draw_composer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let logical_line_count = app.input_line_count();
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let wrapped = wrap_composer(&app.input, app.cursor, inner_width);
    let visual_row_count = wrapped.lines.len();
    let hint = match app.activity_indicator() {
        Some(indicator) => format!(" {indicator} — Esc cancels "),
        None if visual_row_count > MAX_COMPOSER_LINES && app.has_multiline_paste() => {
            format!(
                " pasted text · {} lines · Enter submits all ",
                logical_line_count
            )
        }
        None if visual_row_count > MAX_COMPOSER_LINES => {
            format!(" ↑ {} rows · Enter submits all ", visual_row_count)
        }
        None => " message — / for commands ".to_string(),
    };

    // For large inputs, show only the visual rows around the cursor so a long
    // soft-wrapped line follows the caret just like an explicit multiline input.
    let inner_height = area.height.saturating_sub(2) as usize;
    let (window_start, window_end) = if visual_row_count > inner_height && inner_height > 0 {
        let half = inner_height / 2;
        let start = wrapped.cursor_row.saturating_sub(half);
        let end = (start + inner_height).min(visual_row_count);
        let start = end.saturating_sub(inner_height);
        (start, end)
    } else {
        (0, visual_row_count)
    };
    let display_text = wrapped.lines[window_start..window_end].join("\n");

    let paragraph = Paragraph::new(display_text.as_str()).block(panel(hint, !app.is_busy()));
    frame.render_widget(paragraph, area);

    // Place the real terminal caret on the same visual row produced above.
    if !app.has_overlay() {
        let display_row = wrapped.cursor_row.saturating_sub(window_start);
        let max_row = area.height.saturating_sub(3) as usize;
        frame.set_cursor_position((
            area.x + 1 + wrapped.cursor_column.min(inner_width) as u16,
            area.y + 1 + display_row.min(max_row) as u16,
        ));
    }
}

/// Soft-wraps composer text at terminal cell boundaries and records where the
/// character-indexed caret lands. Keeping text and caret geometry in one pass
/// prevents narrow terminals from rendering one while positioning the other as
/// if the input were still a single line.
#[derive(Debug, PartialEq, Eq)]
struct WrappedComposer {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_column: usize,
}

fn wrap_composer(input: &str, cursor: usize, width: usize) -> WrappedComposer {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut row = 0usize;
    let mut column = 0usize;
    let mut at_right_edge = false;
    let mut caret = None;
    let char_count = input.chars().count();
    let cursor = cursor.min(char_count);

    for (index, ch) in input.chars().enumerate() {
        if index == cursor {
            caret = Some(if at_right_edge {
                (row + 1, 0)
            } else {
                (row, column)
            });
        }

        if ch == '\n' {
            lines.push(String::new());
            row += 1;
            column = 0;
            at_right_edge = false;
            continue;
        }

        let char_width = ch.width().unwrap_or(0);
        if char_width > 0 && (at_right_edge || (column > 0 && column + char_width > width)) {
            lines.push(String::new());
            row += 1;
            column = 0;
            at_right_edge = false;
        }

        lines[row].push(ch);
        column = column.saturating_add(char_width);
        if char_width > 0 && column >= width {
            at_right_edge = true;
        }
    }

    let (cursor_row, cursor_column) = caret.unwrap_or_else(|| {
        if at_right_edge {
            lines.push(String::new());
            (row + 1, 0)
        } else {
            (row, column)
        }
    });

    WrappedComposer {
        lines,
        cursor_row,
        cursor_column,
    }
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // The warning is reserved its own cell: a long status line must never be able
    // to push a standing authority grant off screen.
    let mode = app.mode();
    let authority_label = format!(" {} mode ", mode.label());
    let warning_width = (authority_label.chars().count() as u16).min(area.width);
    let cells = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(warning_width)])
        .split(area);
    let (info_area, warning_area) = (cells[0], cells[1]);

    frame.render_widget(
        Paragraph::new(TextLine::from(Span::styled(
            authority_label,
            Style::default()
                .fg(Color::Black)
                .bg(if mode == argo_core::mode::AgentMode::Full {
                    Color::Rgb(200, 80, 80)
                } else {
                    NOTICE
                })
                .add_modifier(Modifier::BOLD),
        ))),
        warning_area,
    );
    let agent = app
        .conversation
        .as_ref()
        .and_then(|c| c.selected_agent_id.clone())
        .unwrap_or_else(|| "choose CLI".to_string());
    let model = app
        .conversation
        .as_ref()
        .and_then(|c| c.selected_model.clone())
        .unwrap_or_else(|| "choose model".to_string());

    let mut spans = vec![
        // The active target, which is the fact a user checks most often.
        Span::styled(
            format!(" {agent} "),
            Style::default()
                .fg(Color::Black)
                .bg(agent_color(&agent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {model} "), Style::default().fg(ACCENT)),
    ];

    if let Some(effort) = app.effort_label() {
        spans.push(Span::styled(
            format!("· {effort} "),
            Style::default().fg(NOTICE),
        ));
    }

    spans.push(Span::styled("│", Style::default().fg(MUTED)));
    spans.push(Span::styled(
        format!(" {} ", app.context_label()),
        Style::default().fg(MUTED),
    ));

    if let Some(usage) = &app.last_usage {
        if let (Some(input), Some(output)) = (usage.input, usage.output) {
            spans.push(Span::styled(
                format!("· last {input}→{output} "),
                Style::default().fg(MUTED),
            ));
        }
    }

    if app.queue_depth() > 0 {
        spans.push(Span::styled(
            format!("· {} queued ", app.queue_depth()),
            Style::default()
                .fg(Color::Black)
                .bg(NOTICE)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.extend([
        Span::styled("│", Style::default().fg(MUTED)),
        Span::styled(format!(" {} ", app.status), Style::default().fg(MUTED)),
    ]);
    frame.render_widget(Paragraph::new(TextLine::from(spans)), info_area);
}

/// Shortens a path from the left so the tail stays readable.
pub fn shorten_path(path: &str, max: usize) -> String {
    if path.chars().count() <= max {
        return path.to_string();
    }
    let tail: String = path
        .chars()
        .rev()
        .take(max.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// A visible terminal fragment backed by a terminal-native OSC 8 destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeHyperlink {
    pub(crate) column: u16,
    pub(crate) row: u16,
    pub(crate) text: String,
    pub(crate) url: String,
}

/// Finds visible HTTP(S) destinations after Ratatui has laid out the frame.
///
/// The URL text remains ordinary terminal text, so drag selection and copying are
/// left to the terminal. `run` repaints these exact fragments with OSC 8 metadata
/// after Ratatui flushes the frame, which gives supporting terminals native link
/// interaction without enabling mouse capture.
pub(crate) fn native_hyperlinks(buffer: &Buffer, app: &App) -> Vec<NativeHyperlink> {
    let area = buffer.area;
    if area.is_empty() {
        return Vec::new();
    }

    let mut known = app
        .lines
        .iter()
        .flat_map(|line| urls_in_text(&line.text).into_iter().map(|(_, _, url)| url))
        .collect::<Vec<_>>();
    match &app.overlay {
        Overlay::Text { lines, .. } => known.extend(
            lines
                .iter()
                .flat_map(|line| urls_in_text(line).into_iter().map(|(_, _, url)| url)),
        ),
        Overlay::Picker { items, .. } => known.extend(
            items
                .iter()
                .flat_map(|line| urls_in_text(line).into_iter().map(|(_, _, url)| url)),
        ),
        Overlay::Input { value, secret, .. } if !secret => {
            known.extend(urls_in_text(value).into_iter().map(|(_, _, url)| url));
        }
        Overlay::Input { .. } => {}
        Overlay::None => {}
    }
    known.sort();
    known.dedup();

    let mut hyperlinks = Vec::new();
    for row in area.y..area.y.saturating_add(area.height) {
        let rendered = (area.x..area.x.saturating_add(area.width))
            .map(|column| buffer[(column, row)].symbol())
            .collect::<String>();
        for (start, _, text) in urls_in_text(&rendered) {
            let url = known
                .iter()
                .find(|url| url.as_str() == text)
                .cloned()
                .or_else(|| {
                    let matches = known
                        .iter()
                        .filter(|url| url.starts_with(&text))
                        .collect::<Vec<_>>();
                    (matches.len() == 1).then(|| matches[0].clone())
                })
                .unwrap_or_else(|| text.clone());
            hyperlinks.push(NativeHyperlink {
                column: area.x.saturating_add(start as u16),
                row,
                text,
                url,
            });
        }
    }
    hyperlinks
}

fn urls_in_text(text: &str) -> Vec<(usize, usize, String)> {
    let mut urls = Vec::new();
    let mut offset = 0usize;
    while offset < text.len() {
        let rest = &text[offset..];
        let next_http = rest.find("http://");
        let next_https = rest.find("https://");
        let Some(relative) = (match (next_http, next_https) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(found), None) | (None, Some(found)) => Some(found),
            (None, None) => None,
        }) else {
            break;
        };
        let start_byte = offset + relative;
        let mut end_byte = start_byte;
        for (index, ch) in text[start_byte..].char_indices() {
            if ch.is_whitespace()
                || ch.is_control()
                || matches!(ch, ')' | ']' | '}' | '>' | '"' | '\'' | '|' | '│' | '┃')
            {
                break;
            }
            end_byte = start_byte + index + ch.len_utf8();
        }
        while end_byte > start_byte
            && text[..end_byte]
                .chars()
                .next_back()
                .is_some_and(|ch| matches!(ch, '.' | ',' | ';' | '!'))
        {
            end_byte -= text[..end_byte]
                .chars()
                .next_back()
                .map(char::len_utf8)
                .unwrap_or(0);
        }
        if end_byte > start_byte {
            let start_column = text[..start_byte].chars().count();
            let url = text[start_byte..end_byte].to_string();
            let end_column = start_column + url.chars().count();
            urls.push((start_column, end_column, url));
        }
        offset = end_byte.max(start_byte + 1);
    }
    urls
}

#[cfg(test)]
mod tests {

    #[test]
    fn wrapping_prefers_word_boundaries() {
        assert_eq!(wrap_words("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap_words("", 10), vec![""]);
        // A zero-width pane must not loop or panic.
        assert_eq!(wrap_words("text", 0), vec!["text"]);
    }

    #[test]
    fn a_word_wider_than_the_pane_is_split_rather_than_clipped() {
        // A long path or URL would otherwise vanish past the border.
        let rows = wrap_words("/very/long/path/that/exceeds/the/pane", 10);
        assert!(rows.iter().all(|r| r.chars().count() <= 10), "{rows:?}");
        assert_eq!(rows.concat(), "/very/long/path/that/exceeds/the/pane");
    }

    #[test]
    fn no_wrapped_row_exceeds_the_pane_width() {
        let rows = wrap_words(&"word ".repeat(40), 12);
        assert!(rows.iter().all(|r| r.chars().count() <= 12), "{rows:?}");
    }

    #[test]
    fn a_long_agent_reply_stays_visible_without_scrolling() {
        // The bug: the offset was computed from logical line count, so one long
        // paragraph counted as a single row and pushed itself off the bottom.
        let mut app = App::new("/repo");
        app.push(LineKind::User, "summarise the repo".to_string());
        app.push(
            LineKind::Assistant,
            "filler words to force wrapping ".repeat(12) + "END_MARKER",
        );
        let out = render(&app, 40, 14);
        assert!(
            out.contains("END_MARKER"),
            "the end of a wrapped reply must remain on screen:\n{out}"
        );
    }

    #[test]
    fn markdown_styles_reach_terminal_cells_without_changing_canonical_text() {
        let source = "# HMARK\n\n**BMARK** and *IMARK* with `CMARK`\n\n- first\n- second";
        let mut app = App::new("/repo");
        app.push(LineKind::Assistant, source);

        let backend = TestBackend::new(70, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let cells = buffer.content();
        let output = cells
            .chunks(70)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(output.contains("HMARK"), "{output}");
        assert!(output.contains("• first"), "{output}");
        assert!(!output.contains("# HMARK"), "{output}");
        assert!(!output.contains("**BMARK**"), "{output}");
        assert!(cells
            .iter()
            .any(|cell| { cell.symbol() == "B" && cell.modifier.contains(Modifier::BOLD) }));
        assert!(cells
            .iter()
            .any(|cell| { cell.symbol() == "I" && cell.modifier.contains(Modifier::ITALIC) }));
        assert!(cells
            .iter()
            .any(|cell| { cell.symbol() == "C" && cell.bg == Color::Rgb(38, 43, 52) }));
        assert_eq!(app.lines.last().expect("assistant").text, source);
    }

    #[test]
    fn a_long_markdown_response_keeps_its_formatted_tail_visible() {
        let mut app = App::new("/repo");
        app.push(
            LineKind::Assistant,
            format!(
                "# Result\n\n{}\n\n## Choices\n\n1. **Keep current design**\n2. Select `TAIL_MARKER`",
                "Markdown paragraph content that wraps safely. ".repeat(12)
            ),
        );
        let output = render(&app, 48, 16);
        assert!(output.contains("Choices"), "{output}");
        assert!(output.contains("TAIL_MARKER"), "{output}");
        assert!(!output.contains("**"), "{output}");
    }

    #[test]
    fn command_code_choice_picker_keeps_long_response_options_visible() {
        let mut app = App::new("/repo");
        app.push(LineKind::User, "available data for nifty using volrix mcp?");
        app.push(
            LineKind::Assistant,
            format!(
                "{}\n\nA few options to move forward — which do you want?\n\n1. Invoke it yourself\n2. **Re-authenticate** the MCP server\n3. Use curl directly",
                "Introductory Command Code response text that wraps. ".repeat(10)
            ),
        );
        assert!(app.open_latest_response_options());
        let out = render(&app, 62, 16);
        assert!(out.contains("choose a response"), "{out}");
        assert!(out.contains("Re-authenticate"), "{out}");
        assert!(out.contains("Use curl directly"), "{out}");
    }
    use super::*;

    #[test]
    fn resumed_structured_history_renders_like_live_activity() {
        use argo_core::message::{ContentBlock, ToolCall, ToolStatus};
        use argo_daemon::protocol::MessageView;

        let mut app = App::new("/repo");
        app.replace_transcript(vec![MessageView {
            id: "m1".into(),
            role: "assistant".into(),
            text: String::new(),
            blocks: vec![
                ContentBlock::Thinking {
                    text: "checking the report".into(),
                },
                ContentBlock::Tool {
                    call: ToolCall {
                        id: "t1".into(),
                        name: "run_backtest".into(),
                        input: Some("SENSEX".into()),
                        output: Some("runID backtest-123".into()),
                        status: ToolStatus::Completed,
                    },
                },
                ContentBlock::FileWrite {
                    path: "strategy.py".into(),
                },
                ContentBlock::text(
                    "## Result\n\n[Report Link](https://example.com/report/backtest-123)",
                ),
            ],
            agent_id: Some("antigravity".into()),
            model: Some("sonnet".into()),
            usage: None,
            created_at: 0,
        }]);

        let output = render(&app, 78, 20);
        assert!(output.contains("◌ checking the report"), "{output}");
        assert!(output.contains("calling run_backtest"), "{output}");
        assert!(output.contains("backtest-123"), "{output}");
        assert!(output.contains("wrote strategy.py"), "{output}");
        assert!(output.contains("Result"), "{output}");
        assert!(output.contains("https://example.com/report"), "{output}");
        assert!(!output.contains("## Result"), "{output}");
    }

    #[test]
    fn reasoning_tools_files_and_answer_are_visually_distinct_and_visible() {
        let mut app = App::new("/repo");
        app.push(LineKind::Thinking, "checking the repository".to_string());
        app.push(
            LineKind::Activity,
            "↳ calling shell — cargo test".to_string(),
        );
        app.push(LineKind::Activity, "✎ wrote src/main.rs".to_string());
        app.push(LineKind::Assistant, "ANSWER_MARKER finished".to_string());

        let out = render(&app, 62, 15);
        assert!(out.contains("◌ checking the repository"), "{out}");
        assert!(out.contains("calling shell"), "{out}");
        assert!(out.contains("wrote src/main.rs"), "{out}");
        assert!(out.contains("│ ANSWER_MARKER finished"), "{out}");
    }
    use crate::app::LineKind;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_hyperlinks(app: &App, width: u16, height: u16) -> Vec<NativeHyperlink> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hyperlinks = Vec::new();
        terminal
            .draw(|frame| {
                draw(frame, app);
                hyperlinks = native_hyperlinks(frame.buffer_mut(), app);
            })
            .expect("draw");
        hyperlinks
    }

    #[test]
    fn rendered_markdown_destination_becomes_a_native_hyperlink() {
        let mut app = App::new("/repo");
        app.push(
            LineKind::Assistant,
            "[Report Link](https://example.com/report/123)",
        );

        assert!(rendered_hyperlinks(&app, 72, 16).iter().any(|link| {
            link.text == "https://example.com/report/123"
                && link.url == "https://example.com/report/123"
        }));
    }

    #[test]
    fn wrapped_link_prefix_uses_the_full_canonical_destination() {
        let destination = "https://example.com/reports/very-long-backtest-identifier-123456789";
        let mut app = App::new("/repo");
        app.push(LineKind::Assistant, format!("[Report Link]({destination})"));

        assert!(rendered_hyperlinks(&app, 38, 18)
            .iter()
            .any(|link| link.text.starts_with("https://") && link.url == destination));
    }

    #[test]
    fn url_parser_accepts_web_links_and_rejects_unsafe_schemes() {
        let urls = urls_in_text(
            "https://example.com/a, http://localhost:3000/x! file://tmp/x javascript:alert(1)",
        );
        assert_eq!(
            urls.iter()
                .map(|(_, _, url)| url.as_str())
                .collect::<Vec<_>>(),
            vec!["https://example.com/a", "http://localhost:3000/x"]
        );
        assert!(urls_in_text("file:///tmp/report javascript:alert(1)").is_empty());
        assert!(
            urls_in_text("https://safe.example/\u{1b}]8;;https://evil.example")[0]
                .2
                .ends_with('/')
        );
    }

    #[test]
    fn text_overlay_destinations_become_native_hyperlinks() {
        let mut app = App::new("/repo");
        app.open_text(
            "report",
            vec!["Open https://example.com/overlay/report".into()],
        );

        assert!(rendered_hyperlinks(&app, 70, 16).iter().any(|link| {
            link.text == "https://example.com/overlay/report"
                && link.url == "https://example.com/overlay/report"
        }));
    }

    #[test]
    fn each_agent_gets_a_distinct_colour() {
        // A transcript spanning several CLIs is only scannable if they differ.
        let claude = agent_color("claude");
        let codex = agent_color("codex");
        let opencode = agent_color("opencode");
        assert_ne!(claude, codex);
        assert_ne!(codex, opencode);
        // Unknown adapters still get a stable colour rather than a default.
        assert_eq!(agent_color("future-cli"), agent_color("future-cli"));
    }

    #[test]
    fn the_agent_is_extracted_from_a_header_line() {
        assert_eq!(header_agent("codex · default · resumed session"), "codex");
        assert_eq!(header_agent("claude"), "claude");
    }

    #[test]
    fn live_sessions_are_shown_in_the_header() {
        let mut app = App::new("/repo");
        app.conversation = Some(argo_daemon::protocol::ConversationSummary {
            id: argo_core::ids::ConversationId::new("c1"),
            title: Some("demo".into()),
            description: None,
            selected_agent_id: Some("codex".into()),
            selected_model: None,
            selected_reasoning: None,
            selected_mode: None,
            message_count: 4,
            agents_with_sessions: vec!["claude".into(), "codex".into()],
            parent_conversation_id: None,
            workspace: Some("/repo".into()),
            updated_at: 0,
        });
        let output = render(&app, 100, 12);
        assert!(output.contains("live:"));
        assert!(output.contains("ARGO"));
    }

    #[test]
    fn scrollback_is_indicated_in_the_transcript_title() {
        let mut app = App::new("/repo");
        for i in 0..40 {
            app.push(LineKind::Assistant, format!("line {i}"));
        }
        // Initial draw establishes the width-aware rendered-row limit used by
        // subsequent keyboard navigation.
        let _ = render(&app, 70, 14);
        app.scroll_up(5);
        assert!(render(&app, 70, 14).contains("scrolled back 5"));
    }

    #[test]
    fn wrapped_single_response_can_scroll_to_its_beginning() {
        let mut app = App::new("/repo");
        app.push(
            LineKind::Assistant,
            format!("FIRST_MARKER {} LAST_MARKER", "wrapped content ".repeat(80)),
        );
        let bottom = render(&app, 36, 12);
        assert!(bottom.contains("LAST_MARKER"), "{bottom}");
        assert!(!bottom.contains("FIRST_MARKER"), "{bottom}");

        app.scroll_up(10_000);
        let top = render(&app, 36, 12);
        assert!(top.contains("FIRST_MARKER"), "{top}");
        assert!(top.contains("scrolled back"), "{top}");
    }

    #[test]
    fn live_activity_is_animated_without_inventing_thinking() {
        let mut app = App::new("/repo");
        app.begin_run(
            argo_core::ids::RunId::new("r1"),
            "kiro",
            Some("gpt-5.6"),
            false,
        );
        let starting = render(&app, 70, 14);
        assert!(starting.contains("waiting for CLI output"), "{starting}");
        assert!(!starting.contains("CLI-emitted reasoning"), "{starting}");

        app.apply_event(argo_core::event::RunEventKind::ThinkingDelta {
            text: "checking the repository".into(),
        });
        let thinking = render(&app, 70, 14);
        assert!(thinking.contains("checking the repository"), "{thinking}");
        assert!(
            thinking.contains("receiving CLI-emitted reasoning"),
            "{thinking}"
        );
    }

    #[test]
    fn paths_are_shortened_from_the_left() {
        assert_eq!(shorten_path("/short", 40), "/short");
        let long = "/Users/someone/very/deep/nested/project/path/here";
        let short = shorten_path(long, 20);
        assert!(short.starts_with('…'));
        assert!(short.ends_with("here"), "the tail is the useful part");
        assert_eq!(short.chars().count(), 20);
    }

    #[test]
    fn an_empty_conversation_shows_guidance() {
        let app = App::new("/repo");
        let output = render(&app, 80, 20);
        assert!(output.contains("ARGO"));
        assert!(output.contains("/help"));
        // The current authority mode is always visible.
        assert!(output.contains("full access mode"));
    }

    #[test]
    fn secret_guided_input_is_masked() {
        let mut app = App::new("/repo");
        app.open_input(
            "MCP bearer token",
            "Paste token",
            true,
            crate::app::InputAction::McpBearerToken,
        );
        app.overlay_input_push_str("super-secret-token");
        let output = render(&app, 70, 18);
        assert!(!output.contains("super-secret-token"), "{output}");
        assert!(output.contains('•'), "{output}");
    }

    #[test]
    fn transcript_lines_are_rendered() {
        let mut app = App::new("/repo");
        app.push(LineKind::User, "add a health endpoint");
        app.push(LineKind::AgentHeader, "claude · haiku · resumed session");
        app.push(LineKind::Assistant, "Added /health.");
        let output = render(&app, 80, 20);
        assert!(output.contains("add a health endpoint"));
        assert!(output.contains("claude"));
        assert!(output.contains("Added /health."));
    }

    #[test]
    fn multiline_assistant_text_is_split_into_rows() {
        let mut app = App::new("/repo");
        app.push(LineKind::Assistant, "first\nsecond\nthird");
        let output = render(&app, 40, 20);
        assert!(output.contains("first"));
        assert!(output.contains("second"));
        assert!(output.contains("third"));
    }

    #[test]
    fn a_picker_replaces_the_transcript() {
        let mut app = App::new("/repo");
        app.push(LineKind::Assistant, "hidden behind the overlay");
        app.open_picker(
            "Agent",
            vec!["claude".into(), "codex".into()],
            vec!["claude".into(), "codex".into()],
            crate::app::PickerAction::Agent,
        );
        let output = render(&app, 80, 20);
        assert!(output.contains("Agent"));
        assert!(output.contains("codex"));
        assert!(output.contains("type to filter"));
        assert!(!output.contains("hidden behind the overlay"));
    }

    #[test]
    fn startup_picker_keeps_the_argo_logo_visible() {
        let mut app = App::new("/repo");
        app.open_picker(
            "choose a coding CLI",
            vec!["claude".into(), "codex".into()],
            vec!["claude".into(), "codex".into()],
            PickerAction::StartupAgent,
        );
        let output = render(&app, 80, 24);
        assert!(output.contains("████"), "{output}");
        assert!(output.contains("choose a coding CLI"), "{output}");
        assert!(output.contains("claude"), "{output}");
        assert!(output.contains("Space default"), "{output}");
    }

    #[test]
    fn agents_picker_explains_switch_and_default_controls() {
        let mut app = App::new("/repo");
        app.open_picker(
            "coding CLIs",
            vec!["Codex".into(), "Claude".into()],
            vec!["codex".into(), "claude".into()],
            PickerAction::Agents,
        );
        let output = render(&app, 100, 18);
        assert!(output.contains("Enter switch"), "{output}");
        assert!(output.contains("Space set default"), "{output}");
        assert!(output.contains("Del clear"), "{output}");
    }

    #[test]
    fn an_available_update_is_visible_in_the_header() {
        let mut app = App::new("/repo");
        app.available_update = Some("0.2.0".into());
        let output = render(&app, 100, 14);
        assert!(output.contains("update v0.2.0"), "{output}");
        assert!(output.contains("/update"), "{output}");
    }

    #[test]
    fn typing_a_slash_floats_a_suggestion_list() {
        let mut app = App::new("/repo");
        app.insert('/');
        app.insert('m');
        let output = render(&app, 60, 16);
        assert!(output.contains("/model"));
        assert!(output.contains("Enter run"));
    }

    #[test]
    fn the_highlighted_suggestion_is_marked() {
        let mut app = App::new("/repo");
        app.insert('/');
        app.insert('a');
        app.completion_move(1);
        let output = render(&app, 70, 16);
        assert!(output.contains("↑↓ pick"));
        // The marker sits on the highlighted row.
        let marked: Vec<&str> = output
            .lines()
            .filter(|line| line.contains('›') && line.contains("/agents"))
            .collect();
        assert!(!marked.is_empty(), "highlighted entry must be marked");
    }

    #[test]
    fn a_multiline_composer_renders_every_line() {
        let mut app = App::new("/repo");
        for ch in "first line".chars() {
            app.insert(ch);
        }
        app.insert_newline();
        for ch in "second line".chars() {
            app.insert(ch);
        }
        let output = render(&app, 60, 16);
        assert!(output.contains("first line"));
        assert!(output.contains("second line"));
    }

    #[test]
    fn a_long_composer_soft_wraps_and_grows_in_a_narrow_terminal() {
        let mut app = App::new("/repo");
        for ch in "abcdefghijklmnopqrstuvwxyz".chars() {
            app.insert(ch);
        }

        let output = render(&app, 16, 14);
        let rows = output.lines().collect::<Vec<_>>();
        let first = rows
            .iter()
            .position(|row| row.contains("abcdefghijklmn"))
            .expect("first wrapped composer row");
        let second = rows
            .iter()
            .position(|row| row.contains("opqrstuvwxyz"))
            .expect("second wrapped composer row");

        assert_eq!(second, first + 1, "wrapped rows must not overlap: {output}");
    }

    #[test]
    fn composer_wrapping_tracks_the_caret_and_terminal_cell_width() {
        assert_eq!(
            wrap_composer("abcdefghijkl", 12, 5),
            WrappedComposer {
                lines: vec!["abcde".into(), "fghij".into(), "kl".into()],
                cursor_row: 2,
                cursor_column: 2,
            }
        );
        assert_eq!(
            wrap_composer("abcde", 5, 5),
            WrappedComposer {
                lines: vec!["abcde".into(), "".into()],
                cursor_row: 1,
                cursor_column: 0,
            }
        );
        assert_eq!(
            wrap_composer("界界a", 3, 4),
            WrappedComposer {
                lines: vec!["界界".into(), "a".into()],
                cursor_row: 1,
                cursor_column: 1,
            }
        );
    }

    #[test]
    fn suggestions_are_hidden_behind_an_overlay() {
        let mut app = App::new("/repo");
        app.insert('/');
        app.open_text("commands", vec!["something".into()]);
        let output = render(&app, 60, 16);
        assert!(!output.contains("Tab to accept"));
    }

    #[test]
    fn a_filtered_picker_shows_the_match_count() {
        let mut app = App::new("/repo");
        let items: Vec<String> = (0..300).map(|i| format!("provider/model-{i}")).collect();
        app.open_picker(
            "model",
            items.clone(),
            items,
            crate::app::PickerAction::Model,
        );
        for ch in "model-29".chars() {
            app.picker_filter_push(ch);
        }
        let output = render(&app, 70, 20);
        assert!(output.contains("filter 'model-29'"));
        assert!(output.contains("model-29"));
    }

    #[test]
    fn a_picker_filter_matching_nothing_says_so() {
        let mut app = App::new("/repo");
        let items: Vec<String> = vec!["alpha".into()];
        app.open_picker(
            "model",
            items.clone(),
            items,
            crate::app::PickerAction::Model,
        );
        for ch in "zzz".chars() {
            app.picker_filter_push(ch);
        }
        assert!(render(&app, 60, 12).contains("no match"));
    }

    #[test]
    fn a_large_picker_renders_without_panicking() {
        // Only the visible window should be built.
        let mut app = App::new("/repo");
        let items: Vec<String> = (0..475).map(|i| format!("provider/model-{i}")).collect();
        app.open_picker(
            "model",
            items.clone(),
            items,
            crate::app::PickerAction::Model,
        );
        app.overlay_move(400);
        assert!(!render(&app, 80, 20).is_empty());
    }

    #[test]
    fn the_composer_shows_a_running_state() {
        let mut app = App::new("/repo");
        assert!(render(&app, 60, 12).contains("message"));
        app.begin_run(
            argo_core::ids::RunId::new("r1"),
            "claude",
            Some("haiku"),
            false,
        );
        assert!(render(&app, 60, 12).contains("Esc cancels"));
        assert!(render(&app, 60, 12).contains("starting"));
    }

    #[test]
    fn rendering_survives_a_very_narrow_terminal() {
        // Layout arithmetic must not panic when there is almost no room.
        let mut app = App::new("/a/very/long/workspace/path/that/will/not/fit");
        app.push(LineKind::Assistant, "some text that must wrap somewhere");
        let output = render(&app, 20, 10);
        assert!(!output.is_empty());
    }

    #[test]
    fn rendering_survives_a_tiny_terminal() {
        let app = App::new("/repo");
        let output = render(&app, 10, 8);
        assert!(!output.is_empty());
    }

    #[test]
    fn selection_is_shown_in_the_header() {
        let mut app = App::new("/repo");
        app.conversation = Some(argo_daemon::protocol::ConversationSummary {
            id: argo_core::ids::ConversationId::new("c1"),
            title: Some("switching demo".into()),
            description: None,
            selected_agent_id: Some("codex".into()),
            selected_model: Some("gpt-5.6".into()),
            selected_reasoning: None,
            selected_mode: None,
            message_count: 2,
            agents_with_sessions: vec!["claude".into()],
            parent_conversation_id: None,
            workspace: Some("/repo".into()),
            updated_at: 0,
        });
        let output = render(&app, 90, 12);
        assert!(output.contains("switching demo"));
        assert!(output.contains("codex/gpt-5.6"));
    }

    #[test]
    fn thinking_can_be_hidden_without_mutating_the_transcript() {
        let mut app = App::new("/repo");
        app.push(LineKind::Thinking, "private-visible-reasoning");
        app.push(LineKind::Assistant, "final answer");
        app.set_thinking_visible(false);
        let output = render(&app, 70, 12);
        assert!(!output.contains("private-visible-reasoning"));
        assert!(output.contains("thinking hidden"));
        assert!(output.contains("final answer"));
        assert!(app.lines.iter().any(|line| line.kind == LineKind::Thinking));

        app.set_thinking_visible(true);
        let shown = render(&app, 70, 12);
        assert!(shown.contains("private-visible-reasoning"));
        assert!(!shown.contains("thinking hidden"));
    }
}
