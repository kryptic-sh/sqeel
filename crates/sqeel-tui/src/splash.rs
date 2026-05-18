//! Sqeel startup splash screen — mirrors `apps/hjkl/src/start_screen.rs`.

use hjkl_splash::{CellKind, Layout, Splash, default_trail_color};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

/// The ANSI-art block embedded at compile time.
pub const ART: &str = include_str!("splash_art.txt");

/// Number of visible art rows (rows 0–5 inclusive; row 6 is blank).
pub const ROWS: u16 = 6;

/// Number of columns in the art block (max line length).
pub const COLS: u16 = 41;

/// Cursor path tracing the s, q, e, e, l letter strokes.
///
/// Art layout (each column position is a char index in the UTF-8 string;
/// glyph spans derived from actual non-space char positions):
///
///   s : cols  0–6   (row 0–4)
///   q : cols  8–15  (row 0–5 with ▀▀ descender)
///   e : cols 16–24  (row 0–4, actually 17–23)
///   e : cols 24–32  (row 0–4, actually 25–31)
///   l : cols 32–40  (row 0–4, actually 33–39)
///
/// Each entry is (row, col, segment_char).
#[rustfmt::skip]
pub const PATH: &[(u8, u8, char)] = &[
    // s: top bar left→right (row 0)
    (0, 0, 's'), (0, 1, 's'), (0, 2, 's'), (0, 3, 's'), (0, 4, 's'), (0, 5, 's'), (0, 6, 's'),
    // s: left side top-half down (row 1)
    (1, 0, 's'), (1, 1, 's'),
    // s: middle bar left→right (row 2)
    (2, 0, 's'), (2, 1, 's'), (2, 2, 's'), (2, 3, 's'), (2, 4, 's'), (2, 5, 's'), (2, 6, 's'),
    // s: right side bottom-half down (row 3)
    (3, 5, 's'), (3, 6, 's'),
    // s: bottom bar right→left (row 4)
    (4, 6, 's'), (4, 5, 's'), (4, 4, 's'), (4, 3, 's'), (4, 2, 's'), (4, 1, 's'), (4, 0, 's'),

    // q: top arc left→right (row 0)
    (0, 9, 'q'), (0, 10, 'q'), (0, 11, 'q'), (0, 12, 'q'), (0, 13, 'q'), (0, 14, 'q'),
    // q: right vertical top→bottom (row 1–4)
    (1, 14, 'q'), (1, 15, 'q'),
    (2, 14, 'q'), (2, 15, 'q'),
    (3, 14, 'q'), (3, 15, 'q'),
    (4, 9, 'q'), (4, 10, 'q'), (4, 11, 'q'), (4, 12, 'q'), (4, 13, 'q'), (4, 14, 'q'),
    // q: left vertical bottom→top (row 3–1)
    (3, 8, 'q'), (3, 9, 'q'),
    (2, 8, 'q'), (2, 9, 'q'),
    (1, 8, 'q'), (1, 9, 'q'),
    // q: tail / descender (rows 3–5)
    (3, 11, 'q'), (3, 12, 'q'),
    (5, 12, 'q'), (5, 13, 'q'),

    // e (first): top bar left→right (row 0)
    (0, 17, 'e'), (0, 18, 'e'), (0, 19, 'e'), (0, 20, 'e'), (0, 21, 'e'), (0, 22, 'e'), (0, 23, 'e'),
    // e: left vertical top→bottom (row 1–4)
    (1, 17, 'e'), (1, 18, 'e'),
    (2, 17, 'e'), (2, 18, 'e'),
    // e: middle bar left→right (row 2)
    (2, 19, 'e'), (2, 20, 'e'), (2, 21, 'e'),
    (3, 17, 'e'), (3, 18, 'e'),
    (4, 17, 'e'), (4, 18, 'e'), (4, 19, 'e'), (4, 20, 'e'), (4, 21, 'e'), (4, 22, 'e'), (4, 23, 'e'),

    // e (second): top bar left→right (row 0)
    (0, 25, 'e'), (0, 26, 'e'), (0, 27, 'e'), (0, 28, 'e'), (0, 29, 'e'), (0, 30, 'e'), (0, 31, 'e'),
    // e: left vertical top→bottom (row 1–4)
    (1, 25, 'e'), (1, 26, 'e'),
    (2, 25, 'e'), (2, 26, 'e'),
    // e: middle bar left→right (row 2)
    (2, 27, 'e'), (2, 28, 'e'), (2, 29, 'e'),
    (3, 25, 'e'), (3, 26, 'e'),
    (4, 25, 'e'), (4, 26, 'e'), (4, 27, 'e'), (4, 28, 'e'), (4, 29, 'e'), (4, 30, 'e'), (4, 31, 'e'),

    // l: vertical top→bottom (row 0–4)
    (0, 33, 'l'), (0, 34, 'l'),
    (1, 33, 'l'), (1, 34, 'l'),
    (2, 33, 'l'), (2, 34, 'l'),
    (3, 33, 'l'), (3, 34, 'l'),
    // l: bottom bar left→right (row 4)
    (4, 33, 'l'), (4, 34, 'l'), (4, 35, 'l'), (4, 36, 'l'), (4, 37, 'l'), (4, 38, 'l'), (4, 39, 'l'),
];

/// Splash-screen state for the sqeel startup animation.
pub struct SqeelStartScreen {
    splash: Splash<'static>,
}

impl Default for SqeelStartScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl SqeelStartScreen {
    pub fn new() -> Self {
        Self {
            splash: Splash::new(ART, PATH),
        }
    }
}

/// Render the splash screen into `frame` within `area`.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    screen: &SqeelStartScreen,
    theme: &crate::theme::Theme,
) {
    let layout = Layout::centered(area.width, area.height, ROWS, COLS);

    let art_top = area.y + layout.origin_y;
    let art_left = area.x + layout.origin_x;

    // Translate layout-relative origins to absolute frame coordinates.
    let abs_layout = hjkl_splash::Layout {
        origin_x: art_left,
        origin_y: art_top,
        ..layout
    };

    let ui = theme.build_ui();
    let buf = frame.buffer_mut();
    for cell in screen.splash.cells(abs_layout) {
        if cell.x >= area.x + area.width || cell.y >= area.y + area.height {
            continue;
        }
        match cell.kind {
            CellKind::Art => {
                if let Some(buf_cell) = buf.cell_mut((cell.x, cell.y)) {
                    buf_cell.set_char(cell.ch);
                    buf_cell.set_style(Style::default().fg(ui.status_bar_fg));
                }
            }
            CellKind::Trail { age } => {
                let color: Color = default_trail_color(age).into();
                if let Some(buf_cell) = buf.cell_mut((cell.x, cell.y)) {
                    buf_cell.set_char(cell.ch);
                    buf_cell.set_style(Style::default().fg(color));
                }
            }
            CellKind::Cursor => {
                if let Some(buf_cell) = buf.cell_mut((cell.x, cell.y)) {
                    buf_cell.set_char(cell.ch);
                    buf_cell.set_style(
                        Style::default()
                            .fg(ui.tab_active_fg)
                            .bg(ui.editor_cursor_line_active),
                    );
                }
            }
        }
    }

    // ── sqeel-specific hint text ───────────────────────────────────────────
    let hint_style = Style::default().fg(ui.status_bar_fg);
    let cta = "press any key to start";
    let ex_hints = [
        (":e <file>", "open a file"),
        (":c <conn>", "switch connection"),
        (":q", "quit"),
    ];

    let cmd_col_width = ex_hints.iter().map(|(cmd, _)| cmd.len()).max().unwrap_or(0);
    let gap = 3;
    let block_width = ex_hints
        .iter()
        .map(|(_, desc)| cmd_col_width + gap + desc.len())
        .max()
        .unwrap_or(0) as u16;

    let cta_y = art_top + ROWS + 1;
    if cta_y < area.y + area.height {
        let cta_len = cta.len() as u16;
        let x = area.x + area.width.saturating_sub(cta_len) / 2;
        let rect = Rect {
            x,
            y: cta_y,
            width: cta_len.min(area.width),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(cta, hint_style)])),
            rect,
        );
    }

    let block_x = area.x + area.width.saturating_sub(block_width) / 2;
    for (i, (cmd, desc)) in ex_hints.iter().enumerate() {
        let y = art_top + ROWS + 3 + i as u16;
        if y >= area.y + area.height {
            break;
        }
        let line = format!("{cmd:<cmd_col_width$}{:gap$}{desc}", "");
        let rect = Rect {
            x: block_x,
            y,
            width: block_width.min(area.width),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(line, hint_style)])),
            rect,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_produces_output_at_tick_zero() {
        let splash = Splash::fixed_tick(ART, PATH, 0);
        let layout = Layout {
            origin_x: 0,
            origin_y: 0,
            rows: ROWS,
            cols: COLS,
        };
        let count = splash.cells(layout).count();
        assert!(count > 0, "expected splash cells at tick 0, got 0");
    }

    #[test]
    fn fixed_tick_cells_non_zero_at_tick_five() {
        let splash = Splash::fixed_tick(ART, PATH, 5);
        let layout = Layout {
            origin_x: 0,
            origin_y: 0,
            rows: ROWS,
            cols: COLS,
        };
        let count = splash.cells(layout).count();
        assert!(count > 0, "expected splash cells at tick 5, got 0");
    }
}
