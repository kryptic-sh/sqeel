use std::sync::{Arc, Mutex};
use std::io;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use sqeel_core::{AppState, UiProvider, state::Focus};

pub struct TuiProvider;

impl UiProvider for TuiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let result = run_loop(&mut terminal, state);

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        result
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<Mutex<AppState>>,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|f| {
            let s = state.lock().unwrap();
            draw(f, &s);
        })?;

        if let Event::Key(key) = event::read()? {
            let mut s = state.lock().unwrap();
            match (key.modifiers, key.code) {
                // Quit
                (KeyModifiers::NONE, KeyCode::Char('q')) => break,
                (KeyModifiers::NONE, KeyCode::Char(':')) => {
                    // TODO: command mode — :q handled in M2
                    break;
                }
                // Pane focus
                (KeyModifiers::CONTROL, KeyCode::Char('h')) => s.focus = Focus::Schema,
                (KeyModifiers::CONTROL, KeyCode::Char('l')) => s.focus = Focus::Editor,
                (KeyModifiers::CONTROL, KeyCode::Char('j')) => s.focus = Focus::Results,
                (KeyModifiers::CONTROL, KeyCode::Char('k')) => s.focus = Focus::Editor,
                _ => {}
            }
        }
    }
    Ok(())
}

fn draw(f: &mut ratatui::Frame, state: &AppState) {
    let area = f.area();

    // Outer split: schema (15%) | right (85%)
    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(15), Constraint::Percentage(85)])
        .split(area);

    let schema_focused = state.focus == Focus::Schema;
    let editor_focused = state.focus == Focus::Editor;
    let results_focused = state.focus == Focus::Results;

    // Schema panel
    let schema_block = Block::default()
        .title("Schema")
        .borders(Borders::ALL)
        .border_style(if schema_focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });
    f.render_widget(schema_block, outer[0]);

    // Right side: editor + optional results
    let editor_pct = (state.editor_ratio * 100.0) as u16;
    let results_pct = 100 - editor_pct;

    let show_results = !matches!(state.results, sqeel_core::state::ResultsPane::Empty);

    let right = if show_results {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(editor_pct),
                Constraint::Percentage(results_pct),
            ])
            .split(outer[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(100)])
            .split(outer[1])
    };

    // Editor panel
    let editor_block = Block::default()
        .title("Editor")
        .borders(Borders::ALL)
        .border_style(if editor_focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });
    let editor_widget = Paragraph::new(state.editor_content.as_str()).block(editor_block);
    f.render_widget(editor_widget, right[0]);

    // Results panel
    if show_results {
        let (title, content, color) = match &state.results {
            sqeel_core::state::ResultsPane::Results(r) => {
                let text = format!(
                    "{}\n{}",
                    r.columns.join(" | "),
                    r.rows
                        .iter()
                        .map(|row| row.join(" | "))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                ("Results", text, Color::Green)
            }
            sqeel_core::state::ResultsPane::Error(e) => ("Error", e.clone(), Color::Red),
            sqeel_core::state::ResultsPane::Empty => unreachable!(),
        };

        let results_block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(if results_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(color)
            });
        let results_widget = Paragraph::new(content).block(results_block);
        f.render_widget(results_widget, right[1]);
    }
}

#[cfg(test)]
mod tests {
    use sqeel_core::{AppState, state::{Focus, QueryResult}};

    #[test]
    fn quit_key_transitions() {
        // Verify focus changes work on AppState directly
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.focus = Focus::Schema;
        assert_eq!(s.focus, Focus::Schema);
        s.focus = Focus::Editor;
        assert_eq!(s.focus, Focus::Editor);
    }

    #[test]
    fn layout_ratio_default() {
        let state = AppState::new();
        let s = state.lock().unwrap();
        assert_eq!(s.editor_ratio, 1.0);
    }

    #[test]
    fn layout_ratio_with_results() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["col".into()],
            rows: vec![vec!["val".into()]],
        });
        assert_eq!(s.editor_ratio, 0.5);
    }
}
