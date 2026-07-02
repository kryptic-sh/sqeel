//! Statement dispatch: run-under-cursor / run-all, the destructive-run
//! guard's pending state, and the shared executor hand-off.

use super::*;

/// A run the destructive-statement guard intercepted, parked while the y/N
/// confirm modal is up. `y` dispatches it via [`dispatch_pending_run`];
/// anything else drops it.
pub(crate) enum PendingRun {
    /// Single statement (Ctrl+Enter / `<leader><CR>`).
    Single(String),
    /// Whole-buffer batch (Ctrl+Shift+Enter / `<leader><Tab>`).
    Batch(Vec<String>),
}

impl PendingRun {
    /// One-line description of why the guard fired, for the confirm dialog.
    pub(crate) fn warn_label(&self) -> String {
        match self {
            PendingRun::Single(stmt) => sqeel_core::safety::destructive_kind(stmt)
                .map(|k| k.label().to_string())
                .unwrap_or_else(|| "destructive statement".to_string()),
            PendingRun::Batch(stmts) => {
                let kinds: Vec<&'static str> = stmts
                    .iter()
                    .filter_map(|s| sqeel_core::safety::destructive_kind(s))
                    .map(|k| k.label())
                    .collect();
                let uniq: Vec<&'static str> = {
                    let mut seen = Vec::new();
                    for k in kinds {
                        if !seen.contains(&k) {
                            seen.push(k);
                        }
                    }
                    seen
                };
                uniq.join(", ")
            }
        }
    }
}

/// Actually dispatch a (possibly guard-confirmed) run to the executor.
/// Shared by the direct path (guard off / statement safe) and the confirm
/// modal's `y` arm.
pub(crate) fn dispatch_pending_run(state: &Arc<Mutex<AppState>>, pending: PendingRun) {
    let mut s = state.lock().unwrap();
    s.dismiss_results();
    match pending {
        PendingRun::Single(stmt) => {
            let tab_idx = s.push_loading_tab(stmt.clone());
            let sent = s.send_query(stmt.clone(), tab_idx);
            if !sent {
                s.push_history(&stmt);
                s.dismiss_results();
                s.set_error(
                    "No DB connected. Use --url / --connection or <leader>c to switch.".into(),
                );
            }
        }
        PendingRun::Batch(stmts) => {
            for stmt in &stmts {
                s.push_loading_tab(stmt.clone());
            }
            if !s.send_batch(stmts, 0) {
                s.dismiss_results();
                s.set_error(
                    "No DB connected. Use --url / --connection or <leader>c to switch.".into(),
                );
            }
        }
    }
}

/// Run the SQL statement under the cursor (or the visual selection) against
/// the active connection. Mirrors the `Ctrl+Enter` / `<leader><CR>` key
/// handlers.
///
/// With `guard` on, a destructive statement (see
/// [`sqeel_core::safety::destructive_kind`]) is NOT dispatched — it's
/// returned as a [`PendingRun`] for the caller to park behind the y/N
/// confirm modal.
pub(crate) fn run_statement_under_cursor(
    editor: &mut Editor<hjkl_buffer::Buffer, SqeelHost>,
    state: &Arc<Mutex<AppState>>,
    guard: bool,
) -> Option<PendingRun> {
    let content = editor.content();
    let stmt = if let Some(sel) = visual_selection_text(editor) {
        sel
    } else {
        let cursor_byte = cursor_byte_offset(&buffer_lines(editor.buffer()), editor.cursor());
        statement_at_byte(&content, cursor_byte)
            .map(|(s, e)| content[s..e].trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| content.trim().to_string())
    };
    // Drop visual mode after capturing the text so results render against a
    // clean cursor (the user expects Ctrl+Enter to also "commit" the
    // selection — like running it commits to it).
    if editor.vim_mode() != hjkl_engine::VimMode::Normal {
        hjkl_vim::dispatch_input(
            editor,
            hjkl_engine::Input {
                key: hjkl_engine::Key::Esc,
                ..hjkl_engine::Input::default()
            },
        );
    }
    let stmt = stmt.trim().to_string();
    {
        let mut s = state.lock().unwrap();
        s.dismiss_completions();
        let dialect = s.active_dialect;
        if strip_sql_comments(&stmt).trim().is_empty() {
            return None; // nothing to run
        }
        if !dialect.is_native_statement(&stmt)
            && let Some(err) = first_syntax_error(&stmt)
        {
            s.dismiss_results();
            s.set_error(format!(
                "Syntax error at {}:{} — {}",
                err.line, err.col, err.message
            ));
            return None;
        }
    }
    let pending = PendingRun::Single(stmt);
    if guard
        && matches!(&pending, PendingRun::Single(s) if sqeel_core::safety::destructive_kind(s).is_some())
    {
        return Some(pending);
    }
    dispatch_pending_run(state, pending);
    None
}

/// Run every non-empty statement in the editor buffer against the active
/// connection.  Mirrors the `Ctrl+Shift+Enter` / `<leader><Tab>` key handlers.
///
/// With `guard` on and any destructive statement in the batch, nothing is
/// dispatched — the whole batch comes back as a [`PendingRun`] for the
/// confirm modal (all-or-nothing keeps statement order intact).
pub(crate) fn run_all_statements(
    editor: &mut Editor<hjkl_buffer::Buffer, SqeelHost>,
    state: &Arc<Mutex<AppState>>,
    guard: bool,
) -> Option<PendingRun> {
    let content = editor.content();
    let stmts: Vec<String> = statement_ranges(&content)
        .into_iter()
        .map(|(s, e)| content[s..e].trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| !strip_sql_comments(s).trim().is_empty())
        .collect();
    {
        let mut s = state.lock().unwrap();
        s.dismiss_completions();
        let dialect = s.active_dialect;
        // Syntax pre-check only if none of the statements are engine-native
        // (DESC, SHOW, PRAGMA, …) — tree-sitter-sequel rejects those but the
        // DB runs them fine.
        let any_native = stmts.iter().any(|s| dialect.is_native_statement(s));
        let syntax_err = if any_native {
            None
        } else {
            first_syntax_error(&content)
        };
        if stmts.is_empty() {
            return None; // nothing to run
        }
        if let Some(err) = syntax_err {
            s.dismiss_results();
            s.set_error(format!(
                "Syntax error at {}:{} — {}",
                err.line, err.col, err.message
            ));
            return None;
        }
    }
    let any_destructive = stmts
        .iter()
        .any(|s| sqeel_core::safety::destructive_kind(s).is_some());
    let pending = PendingRun::Batch(stmts);
    if guard && any_destructive {
        return Some(pending);
    }
    dispatch_pending_run(state, pending);
    None
}

pub(crate) fn tmux_navigate(direction: char) {
    if std::env::var("TMUX").is_ok() {
        let _ = std::process::Command::new("tmux")
            .args(["select-pane", &format!("-{direction}")])
            .spawn();
    }
}
