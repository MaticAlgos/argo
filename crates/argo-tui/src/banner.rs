//! The launch banner.
//!
//! A splash screen is not decoration here: it is the only moment Argo can state
//! what makes it different — one conversation, several CLIs — before the user
//! starts typing. It also names the detected agents, which is the first thing
//! anyone wants to know.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};

/// Wide banner, in the block style used by other coding-agent TUIs.
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

/// Width the wide banner needs, including a little breathing room.
const WIDE_MIN_WIDTH: u16 = 38;

/// Colour ramp applied down the banner rows, so it reads as a gradient.
const RAMP: &[Color] = &[
    Color::Rgb(120, 220, 255),
    Color::Rgb(100, 205, 255),
    Color::Rgb(94, 190, 250),
    Color::Rgb(88, 172, 240),
    Color::Rgb(82, 152, 230),
    Color::Rgb(76, 132, 220),
];

/// Builds the splash shown in an empty conversation.
///
/// `agents` is the rendered detected-agent summary, and `version` identifies the
/// build so a bug report can name it.
pub fn splash(width: u16, version: &str, agents: &[String]) -> Vec<TextLine<'static>> {
    let art: &[&str] = if width >= WIDE_MIN_WIDTH {
        WIDE
    } else {
        NARROW
    };

    let mut lines: Vec<TextLine<'static>> = Vec::new();
    lines.push(TextLine::from(""));

    for (index, row) in art.iter().enumerate() {
        let color = RAMP[index.min(RAMP.len() - 1)];
        lines.push(TextLine::from(Span::styled(
            (*row).to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(TextLine::from(""));
    // The full tagline overflows a narrow terminal, so it degrades rather than wraps.
    let tagline = if width >= 46 {
        format!("one conversation · many coding CLIs · v{version}")
    } else if width >= 30 {
        format!("many CLIs, one chat · v{version}")
    } else {
        format!("v{version}")
    };
    lines.push(TextLine::from(Span::styled(
        tagline,
        Style::default().fg(Color::Rgb(150, 160, 175)),
    )));

    if !agents.is_empty() {
        lines.push(TextLine::from(""));
        lines.push(TextLine::from(Span::styled(
            "detected".to_string(),
            Style::default()
                .fg(Color::Rgb(120, 130, 145))
                .add_modifier(Modifier::BOLD),
        )));
        for agent in agents {
            lines.push(TextLine::from(Span::styled(
                format!("  {agent}"),
                Style::default().fg(Color::Rgb(150, 160, 175)),
            )));
        }
    }

    lines.push(TextLine::from(""));
    let hints: &[&str] = if width >= 46 {
        &[
            "type a message to begin",
            "/agent  switch CLI mid-conversation",
            "/model  choose a model",
            "/chats  reopen an earlier session",
            "/help   everything else",
        ]
    } else {
        &["/agent", "/model", "/chats", "/help"]
    };
    for hint in hints {
        lines.push(TextLine::from(Span::styled(
            format!("  {hint}"),
            Style::default().fg(Color::Rgb(110, 190, 235)),
        )));
    }

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
    fn the_wide_banner_is_used_when_there_is_room() {
        let lines = splash(100, "0.1.0", &[]);
        let rendered = text(&lines);
        assert!(rendered.contains("█████╗"));
        assert!(rendered.contains("one conversation · many coding CLIs · v0.1.0"));
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
        for width in [24u16, 38, 80] {
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
        let wide: String = splash(90, "0.1.0", &[])[8]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(wide.contains("many coding CLIs"));
        // A narrow terminal gets a shorter line rather than a wrapped one.
        let narrow = splash(24, "0.1.0", &[]);
        let rendered: String = narrow
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("v0.1.0"));
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
