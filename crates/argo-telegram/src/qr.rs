//! Terminal QR codes for the linking step.
//!
//! Typing a `t.me` link into a phone is the slowest part of setting the bridge
//! up, and the step most likely to be mistyped. Scanning the terminal opens the
//! right chat directly.
//!
//! Rendered with half-block characters so two QR rows share one text row: a
//! version-3 code is 29 modules square, which at one row per module would not
//! fit in an ordinary terminal.

use qrcode::types::Color;
use qrcode::QrCode;

/// Renders `data` as scannable terminal lines.
///
/// Returns `None` when the payload is too large to encode, which the caller
/// shows as a plain link instead — a missing QR must never block setup.
pub fn render(data: &str) -> Option<Vec<String>> {
    let code = QrCode::new(data.as_bytes()).ok()?;
    let width = code.width();
    let modules = code.to_colors();
    let dark = |x: usize, y: usize| -> bool {
        // Outside the symbol is quiet zone, which must read as light.
        if x >= width || y >= width {
            return false;
        }
        modules[y * width + x] == Color::Dark
    };

    // Four modules of quiet zone on every side, as the spec requires; without it
    // most phone scanners refuse to lock on.
    const QUIET: usize = 4;
    let span = width + QUIET * 2;
    let mut lines = Vec::with_capacity(span / 2 + 1);

    // Two module rows per text row: the upper half-block is the top row, the
    // background the bottom. Inverted (dark modules drawn as the terminal's
    // background) so it scans on a dark terminal.
    for row in (0..span).step_by(2) {
        let mut line = String::with_capacity(span);
        for column in 0..span {
            let x = column.wrapping_sub(QUIET);
            let top = dark(x, row.wrapping_sub(QUIET));
            let bottom = dark(x, (row + 1).wrapping_sub(QUIET));
            line.push(match (top, bottom) {
                (true, true) => ' ',
                (true, false) => '▄',
                (false, true) => '▀',
                (false, false) => '█',
            });
        }
        lines.push(line);
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_renders_as_a_square_block_of_half_height_rows() {
        let lines = render("https://t.me/argo_test_bot").expect("encodes");
        assert!(!lines.is_empty());
        let width = lines[0].chars().count();
        // Every row is the same width, or the symbol is skewed and unscannable.
        for line in &lines {
            assert_eq!(line.chars().count(), width, "ragged row: {line}");
        }
        // Half-block packing means roughly half as many rows as columns.
        assert!(
            lines.len() * 2 >= width - 1 && lines.len() * 2 <= width + 2,
            "{} rows for {width} columns",
            lines.len()
        );
    }

    #[test]
    fn the_quiet_zone_is_present_on_every_side() {
        // Scanners need four light modules of margin; without them most phones
        // never lock on, which would look like "the QR just doesn't work".
        let lines = render("https://t.me/argo_test_bot").expect("encodes");
        assert!(
            lines[0].chars().all(|c| c == '█'),
            "top margin is not blank: {}",
            lines[0]
        );
        let last = lines.last().expect("rows");
        assert!(last.chars().all(|c| c == '█'), "bottom margin: {last}");
        for line in &lines {
            assert!(line.starts_with("████"), "left margin: {line}");
            assert!(line.ends_with("████"), "right margin: {line}");
        }
    }

    #[test]
    fn an_oversized_payload_declines_rather_than_panicking() {
        // Setup must fall back to a printed link, never fail outright.
        assert!(render(&"x".repeat(8000)).is_none());
    }
}
