//! The launch banner.
//!
//! A splash screen is not decoration here: it is the only moment Argo can state
//! what makes it different — one conversation, several CLIs — before the user
//! starts typing. It also names the detected agents, which is the first thing
//! anyone wants to know.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};

/// Extra-wide banner for terminals ≥44 columns — big, bold, unmissable.
const HERO: &[&str] = &[
    r"  █████╗  ██████╗   ██████╗   ██████╗  ",
    r" ██╔══██╗ ██╔══██╗ ██╔════╝  ██╔═══██╗ ",
    r" ███████║ ██████╔╝ ██║  ███╗ ██║   ██║ ",
    r" ██╔══██║ ██╔══██╗ ██║   ██║ ██║   ██║ ",
    r" ██║  ██║ ██║  ██║ ╚██████╔╝ ╚██████╔╝ ",
    r" ╚═╝  ╚═╝ ╚═╝  ╚═╝  ╚═════╝  ╚═════╝  ",
];

/// Standard banner, in the block style used by other coding-agent TUIs.
const WIDE: &[&str] = &[
    r" █████╗ ██████╗  ██████╗  ██████╗ ",
    r"██╔══██╗██╔══██╗██╔════╝ ██╔═══██╗",
    r"███████║██████╔╝██║  ███╗██║   ██║",
    r"██╔══██║██╔══██╗██║   ██║██║   ██║",
    r"██║  ██║██║  ██║╚██████╔╝╚██████╔╝",
    r"╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝  ╚═════╝ ",
];

/// Narrow banner for terminals too small for the block art.
const NARROW: &[&str] = &[r"┌─┐┬─┐┌─┐┌─┐", r"├─┤├┬┘│ ┬│ │", r"┴ ┴┴└─└─┘└─┘"];

/// Width the hero banner needs.
const HERO_MIN_WIDTH: u16 = 44;
/// Width the wide banner needs, including a little breathing room.
const WIDE_MIN_WIDTH: u16 = 38;

/// Gradient ramp: a warm-to-cool sweep so the logo pops.
const GRADIENT: &[Color] = &[
    Color::Rgb(130, 230, 255), // bright cyan top
    Color::Rgb(110, 210, 255),
    Color::Rgb(90, 190, 250),
    Color::Rgb(75, 170, 245),
    Color::Rgb(65, 150, 240),
    Color::Rgb(55, 130, 235), // deeper blue bottom
];

/// Accent colour for decorative lines.
const SEPARATOR_COLOR: Color = Color::Rgb(60, 70, 90);
/// Colour for the tagline.
const TAGLINE_COLOR: Color = Color::Rgb(160, 170, 190);
/// Colour for muted secondary text.
const DIM_COLOR: Color = Color::Rgb(100, 110, 130);
/// Colour for the hint commands.
const HINT_CMD_COLOR: Color = Color::Rgb(120, 200, 245);
/// Colour for the hint descriptions.
const HINT_DESC_COLOR: Color = Color::Rgb(90, 100, 120);
/// Colour for detected-agent dots.
const AGENT_DOT_COLOR: Color = Color::Rgb(100, 220, 170);
/// Colour for detected-agent names.
const AGENT_NAME_COLOR: Color = Color::Rgb(170, 180, 200);

/// Builds the splash shown in an empty conversation.
///
/// `agents` is the rendered detected-agent summary, and `version` identifies the
/// build so a bug report can name it.
pub fn splash(width: u16, version: &str, agents: &[String]) -> Vec<TextLine<'static>> {
    let (art, is_hero): (&[&str], bool) = if width >= HERO_MIN_WIDTH {
        (HERO, true)
    } else if width >= WIDE_MIN_WIDTH {
        (WIDE, false)
    } else {
        (NARROW, false)
    };

    let w = width as usize;
    let mut lines: Vec<TextLine<'static>> = Vec::new();

    // Top padding.
    lines.push(TextLine::from(""));
    if is_hero {
        lines.push(TextLine::from(""));
    }

    // ── Logo ──────────────────────────────────────────────────────────────
    for (index, row) in art.iter().enumerate() {
        let color = GRADIENT[index.min(GRADIENT.len() - 1)];
        let row_str = (*row).to_string();
        let row_width = row_str.chars().count();
        let pad = w.saturating_sub(row_width) / 2;
        let padding = " ".repeat(pad);
        lines.push(TextLine::from(vec![
            Span::raw(padding),
            Span::styled(
                row_str,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    // ── Separator + tagline ──────────────────────────────────────────────
    lines.push(TextLine::from(""));

    if width >= 46 {
        // Decorative separator centred under the logo.
        let sep_content = "─── ✦ ───";
        let sep_width = sep_content.chars().count();
        let sep_pad = " ".repeat(w.saturating_sub(sep_width) / 2);
        lines.push(TextLine::from(vec![
            Span::raw(sep_pad),
            Span::styled(
                sep_content.to_string(),
                Style::default().fg(SEPARATOR_COLOR),
            ),
        ]));
        lines.push(TextLine::from(""));
    }

    let tagline = if width >= 52 {
        format!("one conversation  ·  many coding CLIs  ·  v{version}")
    } else if width >= 40 {
        format!("one conversation · many CLIs · v{version}")
    } else if width >= 30 {
        format!("many CLIs, one chat · v{version}")
    } else {
        format!("v{version}")
    };
    let tag_width = tagline.chars().count();
    let tag_pad = " ".repeat(w.saturating_sub(tag_width) / 2);
    lines.push(TextLine::from(vec![
        Span::raw(tag_pad),
        Span::styled(tagline, Style::default().fg(TAGLINE_COLOR)),
    ]));

    // ── Detected agents ─────────────────────────────────────────────────
    if !agents.is_empty() {
        lines.push(TextLine::from(""));

        if width >= 46 {
            // Find the widest agent entry so the whole block shares one left margin.
            let heading = "── detected ──";
            let heading_w = heading.chars().count();
            let max_agent_w = agents
                .iter()
                .map(|a| format!("● {a}").chars().count())
                .max()
                .unwrap_or(0);
            let block_w = heading_w.max(max_agent_w);
            let block_pad = " ".repeat(w.saturating_sub(block_w) / 2);

            // The heading is itself centred within the block.
            let heading_inner_pad = " ".repeat(block_w.saturating_sub(heading_w) / 2);
            lines.push(TextLine::from(vec![
                Span::raw(block_pad.clone()),
                Span::raw(heading_inner_pad),
                Span::styled(
                    heading.to_string(),
                    Style::default().fg(DIM_COLOR).add_modifier(Modifier::BOLD),
                ),
            ]));
            for agent in agents {
                lines.push(TextLine::from(vec![
                    Span::raw(block_pad.clone()),
                    Span::styled("● ".to_string(), Style::default().fg(AGENT_DOT_COLOR)),
                    Span::styled(agent.clone(), Style::default().fg(AGENT_NAME_COLOR)),
                ]));
            }
        } else {
            lines.push(TextLine::from(Span::styled(
                "detected".to_string(),
                Style::default().fg(DIM_COLOR).add_modifier(Modifier::BOLD),
            )));
            for agent in agents {
                lines.push(TextLine::from(vec![
                    Span::styled("  ● ".to_string(), Style::default().fg(AGENT_DOT_COLOR)),
                    Span::styled(agent.clone(), Style::default().fg(AGENT_NAME_COLOR)),
                ]));
            }
        }
    }

    // ── Hints ────────────────────────────────────────────────────────────
    lines.push(TextLine::from(""));

    if width >= 46 {
        let hint_sep = "─── ✦ ───";
        let hint_sep_w = hint_sep.chars().count();
        let hint_sep_pad = " ".repeat(w.saturating_sub(hint_sep_w) / 2);
        lines.push(TextLine::from(vec![
            Span::raw(hint_sep_pad),
            Span::styled(hint_sep.to_string(), Style::default().fg(SEPARATOR_COLOR)),
        ]));
        lines.push(TextLine::from(""));

        // Centred prompt.
        let prompt = "type a message to begin";
        let prompt_w = prompt.chars().count();
        let prompt_pad = " ".repeat(w.saturating_sub(prompt_w) / 2);
        lines.push(TextLine::from(vec![
            Span::raw(prompt_pad),
            Span::styled(
                prompt.to_string(),
                Style::default()
                    .fg(TAGLINE_COLOR)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
        lines.push(TextLine::from(""));

        let hints: &[(&str, &str)] = &[
            ("/agent", "switch CLI mid-conversation"),
            ("/model", "choose a model"),
            ("/chats", "reopen an earlier session"),
            ("/help", "everything else"),
        ];
        // Find the widest hint row so the whole block shares one left margin.
        let max_hint_w = hints
            .iter()
            .map(|(cmd, desc)| format!("{cmd:<8}  {desc}").chars().count())
            .max()
            .unwrap_or(0);
        let hint_block_pad = " ".repeat(w.saturating_sub(max_hint_w) / 2);
        for (cmd, desc) in hints {
            lines.push(TextLine::from(vec![
                Span::raw(hint_block_pad.clone()),
                Span::styled(
                    format!("{cmd:<8}"),
                    Style::default()
                        .fg(HINT_CMD_COLOR)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {desc}"), Style::default().fg(HINT_DESC_COLOR)),
            ]));
        }
    } else {
        let commands: &[&str] = &["/agent", "/model", "/chats", "/help"];
        for cmd in commands {
            lines.push(TextLine::from(Span::styled(
                format!("  {cmd}"),
                Style::default().fg(HINT_CMD_COLOR),
            )));
        }
    }

    lines.push(TextLine::from(""));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flattens rendered lines to plain text for assertions.
    fn text(lines: &[TextLine<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_hero_banner_is_used_on_wide_terminals() {
        let lines = splash(80, "0.1.0", &[]);
        let rendered = text(&lines);
        // Hero uses spaced-out block letters.
        assert!(rendered.contains("█████╗"));
        assert!(rendered.contains("one conversation"));
    }

    #[test]
    fn the_wide_banner_is_used_when_there_is_room() {
        let lines = splash(40, "0.1.0", &[]);
        let rendered = text(&lines);
        assert!(rendered.contains("█████╗"));
    }

    #[test]
    fn a_narrow_terminal_falls_back_to_compact_art() {
        // The block art would wrap and look broken below this width.
        let lines = splash(24, "0.1.0", &[]);
        let rendered = text(&lines);
        assert!(rendered.contains("┌─┐┬─┐┌─┐┌─┐"));
        assert!(!rendered.contains("█████╗"));
    }

    #[test]
    fn every_banner_row_fits_its_terminal() {
        for width in [24u16, 38, 80, 120] {
            for line in splash(width, "0.1.0", &[]) {
                let rendered: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                assert!(
                    rendered.chars().count() <= width as usize,
                    "row {rendered:?} overflows width {width}"
                );
            }
        }
    }

    #[test]
    fn detected_agents_are_listed_when_known() {
        let agents = vec!["claude 2.1.220".to_string(), "codex 0.146.0".to_string()];
        let rendered = text(&splash(90, "0.1.0", &agents));
        assert!(rendered.contains("detected"));
        assert!(rendered.contains("claude 2.1.220"));
        assert!(rendered.contains("codex 0.146.0"));
    }

    #[test]
    fn the_agent_section_is_omitted_when_nothing_was_detected() {
        let rendered = text(&splash(90, "0.1.0", &[]));
        assert!(!rendered.contains("detected"));
    }

    #[test]
    fn the_tagline_degrades_before_it_overflows() {
        let wide = text(&splash(90, "0.1.0", &[]));
        assert!(wide.contains("many coding CLIs"));
        // A narrow terminal gets a shorter line rather than a wrapped one.
        let narrow = text(&splash(24, "0.1.0", &[]));
        assert!(narrow.contains("v0.1.0"));
    }

    #[test]
    fn the_hints_name_the_switching_commands() {
        // The splash is where a new user learns Argo's point.
        let rendered = text(&splash(90, "0.1.0", &[]));
        assert!(rendered.contains("/agent"));
        assert!(rendered.contains("switch CLI mid-conversation"));
        assert!(rendered.contains("/chats"));
    }
}
