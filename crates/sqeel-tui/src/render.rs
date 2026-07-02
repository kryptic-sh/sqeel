//! Draw layer: the frame composer (`draw`) plus every pane, popup,
//! dialog, and status-line renderer, and screen-geometry helpers.

use super::*;

pub(crate) fn mode_label(state: &AppState) -> Span<'static> {
    let u = ui();
    match state.vim_mode {
        VimMode::Normal => Span::styled(" NORMAL ", Style::default().fg(u.status_mode_normal)),
        VimMode::Insert => Span::styled(" INSERT ", Style::default().fg(u.status_mode_insert)),
        VimMode::Visual => Span::styled(" VISUAL ", Style::default().fg(u.status_mode_visual)),
        VimMode::VisualLine => Span::styled(" V-LINE ", Style::default().fg(u.status_mode_visual)),
        VimMode::VisualBlock => {
            Span::styled(" V-BLOCK ", Style::default().fg(u.status_mode_visual))
        }
    }
}

pub(crate) fn diag_label(state: &AppState) -> Option<Span<'static>> {
    let errors = state
        .lsp_diagnostics
        .iter()
        .filter(|d| d.severity == lsp_types::DiagnosticSeverity::ERROR)
        .count();
    let warnings = state
        .lsp_diagnostics
        .iter()
        .filter(|d| d.severity == lsp_types::DiagnosticSeverity::WARNING)
        .count();
    if errors > 0 {
        Some(Span::styled(
            format!(" ✖ {errors}E "),
            Style::default().fg(ui().status_diag_error),
        ))
    } else if warnings > 0 {
        Some(Span::styled(
            format!(" ⚠ {warnings}W "),
            Style::default().fg(ui().status_diag_warning),
        ))
    } else {
        None
    }
}

/// Width of the dedicated diagnostic-sign column rendered to the left of
/// the number gutter (vim `signcolumn=yes`). Shared by the renderer, the
/// terminal-cursor placement, and the mouse→doc translation so all three
/// agree on where the text column starts.
pub(crate) const EDITOR_SIGN_COL_WIDTH: u16 = 1;

/// Translate a terminal-cell mouse position inside the editor pane into
/// document `(row, col)` coordinates. Mirrors the geometry `draw_editor`
/// renders with: one tab-bar row on top, a 1-col horizontal margin, then
/// `[sign][number]` gutter before the text. Replaces the engine's removed
/// `mouse_click_in_rect` (the mouse API takes doc coordinates since
/// hjkl-engine 0.8).
///
/// Wrap-aware: with `:set wrap` / `:set linebreak` a doc row occupies
/// several screen rows, so the screen row is resolved by walking doc rows
/// through [`hjkl_buffer::wrap::wrap_segments`] with the same
/// `(text_width, mode)` the renderer wraps with, and the column maps into
/// the clicked segment's char range.
pub(crate) fn editor_cell_to_doc<H: Host>(
    editor: &Editor<hjkl_buffer::Buffer, H>,
    area: Rect,
    col: u16,
    row: u16,
) -> (usize, usize) {
    let rope = editor.buffer().rope();
    let inner_top = area.y.saturating_add(1); // tab bar row
    let content_x = area
        .x
        .saturating_add(1) // horizontal margin
        .saturating_add(EDITOR_SIGN_COL_WIDTH)
        .saturating_add(editor.lnum_width());
    let v = editor.host().viewport();
    let rel_row = row.saturating_sub(inner_top) as usize;
    let rel_col = col.saturating_sub(content_x) as usize;
    let last_row = rope.len_lines().saturating_sub(1);
    let tab_width = editor.settings().tabstop;

    if matches!(v.wrap, hjkl_buffer::Wrap::None) {
        // One doc row per screen row; `top_col` clips the left edge.
        let doc_row = (v.top_row + rel_row).min(last_row);
        let line = hjkl_buffer::rope_line_str(&rope, doc_row);
        let char_col = hjkl_buffer::visual_col_to_char_col(&line, rel_col + v.top_col, tab_width);
        return (
            doc_row,
            char_col.min(line.chars().count().saturating_sub(1)),
        );
    }

    // Soft wrap: walk doc rows from the viewport top, expanding each into
    // its wrap segments, until the clicked screen row falls inside one.
    let width = v.text_width.max(1);
    let mut remaining = rel_row;
    let mut doc_row = v.top_row.min(last_row);
    loop {
        let line = hjkl_buffer::rope_line_str(&rope, doc_row);
        let segs = hjkl_buffer::wrap::wrap_segments(&line, width, v.wrap);
        if remaining < segs.len() {
            let (s, e) = segs[remaining];
            let seg: String = line.chars().skip(s).take(e.saturating_sub(s)).collect();
            let col_in_seg = hjkl_buffer::visual_col_to_char_col(&seg, rel_col, tab_width);
            let char_col = s + col_in_seg.min(e.saturating_sub(s).saturating_sub(1));
            return (
                doc_row,
                char_col.min(line.chars().count().saturating_sub(1)),
            );
        }
        if doc_row >= last_row {
            // Past EOF — clamp to the last cell of the last row.
            let chars = line.chars().count();
            return (last_row, chars.saturating_sub(1));
        }
        remaining -= segs.len();
        doc_row += 1;
    }
}

/// Status-bar block showing `/<pat> <i>/<n>` when an editor search is active.
/// `i` is the 1-based index of the match at-or-after the cursor; 0 means no
/// match has been navigated to yet (cursor is past the last match).
pub(crate) fn search_label<H: Host>(
    editor: &Editor<hjkl_buffer::Buffer, H>,
) -> Option<Span<'static>> {
    let re = editor.search_state().pattern.as_ref()?;
    let pat = re.as_str().to_string();
    let lines = buffer_lines(editor.buffer());
    let (cur_row, cur_col) = editor.cursor();
    let mut total = 0usize;
    let mut current = 0usize;
    for (row, line) in lines.iter().enumerate() {
        for m in re.find_iter(line) {
            total += 1;
            if current == 0 {
                let on_or_after_cursor = row > cur_row
                    || (row == cur_row && byte_to_char_col(line, m.start()) >= cur_col);
                if on_or_after_cursor {
                    current = total;
                }
            }
        }
    }
    if total == 0 {
        return Some(Span::raw(format!(" /{pat} 0/0 ")));
    }
    if current == 0 {
        current = total;
    }
    Some(Span::raw(format!(" /{pat} {current}/{total} ")))
}

pub(crate) fn byte_to_char_col(line: &str, byte_idx: usize) -> usize {
    line[..byte_idx.min(line.len())].chars().count()
}

/// Extract the first `L:C` (1-based line:column) location from a message like
/// `"Syntax error at 3:7 — unexpected `foo`"`. Returns `None` if no match.
pub(crate) fn parse_error_position(msg: &str) -> Option<(usize, usize)> {
    let bytes = msg.as_bytes();
    for i in 0..bytes.len() {
        if !bytes[i].is_ascii_digit() {
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            continue;
        }
        let mut k = j + 1;
        let col_start = k;
        while k < bytes.len() && bytes[k].is_ascii_digit() {
            k += 1;
        }
        if k == col_start {
            continue;
        }
        let line: usize = msg[i..j].parse().ok()?;
        let col: usize = msg[col_start..k].parse().ok()?;
        return Some((line, col));
    }
    None
}

/// Pull the active visual-mode selection out as a runnable string.
/// Returns `None` when the editor isn't in any Visual mode.
///
/// - **Visual** (charwise): exact span between anchor and cursor.
///   First and last lines may be partial; intermediate lines are
///   joined with `\n`.
/// - **VisualLine**: every line in `[top, bot]`, joined with `\n`.
/// - **VisualBlock**: collapsed to the full lines the block spans —
///   running a column-narrow rectangle as SQL doesn't make sense, but
///   "the lines I marked" matches user intent and matches what
///   VisualLine would have produced.
pub(crate) fn visual_selection_text<H: Host>(
    editor: &Editor<hjkl_buffer::Buffer, H>,
) -> Option<String> {
    let lines = buffer_lines(editor.buffer());
    match editor.vim_mode() {
        hjkl_engine::VimMode::Visual => {
            let ((sr, sc), (er, ec)) = editor.char_highlight()?;
            if sr == er {
                let line = lines.get(sr)?;
                Some(
                    line.chars()
                        .skip(sc)
                        .take(ec.saturating_sub(sc) + 1)
                        .collect(),
                )
            } else {
                let mut out = String::new();
                let first = lines.get(sr)?;
                out.push_str(&first.chars().skip(sc).collect::<String>());
                for r in (sr + 1)..er {
                    out.push('\n');
                    if let Some(l) = lines.get(r) {
                        out.push_str(l);
                    }
                }
                out.push('\n');
                let last = lines.get(er)?;
                out.push_str(&last.chars().take(ec + 1).collect::<String>());
                Some(out)
            }
        }
        hjkl_engine::VimMode::VisualLine => {
            let (top, bot) = editor.line_highlight()?;
            let bot = bot.min(lines.len().saturating_sub(1));
            Some(lines[top..=bot].join("\n"))
        }
        hjkl_engine::VimMode::VisualBlock => {
            let (top, bot, _, _) = editor.block_highlight()?;
            let bot = bot.min(lines.len().saturating_sub(1));
            Some(lines[top..=bot].join("\n"))
        }
        _ => None,
    }
}

/// Convert a (row, char-col) cursor into a byte offset into `lines.join("\n")`.
pub(crate) fn cursor_byte_offset(lines: &[String], cursor: (usize, usize)) -> usize {
    let mut byte = 0;
    for (i, line) in lines.iter().enumerate() {
        if i < cursor.0 {
            byte += line.len() + 1; // +1 for '\n'
        } else if i == cursor.0 {
            byte += line
                .chars()
                .take(cursor.1)
                .map(|c| c.len_utf8())
                .sum::<usize>();
            break;
        }
    }
    byte
}

/// Desired terminal cursor shape after a draw. The TUI uses a thin vertical bar
/// for any text-input context (insert mode, dialogs, schema search) and a thick
/// block for editor normal mode, so cursors look consistent across the app.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    Error,
    Info,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CursorShape {
    #[default]
    Hidden,
    Bar,
    Block,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct DrawAreas {
    pub(crate) schema_list_area: Rect,
    pub(crate) schema_list_offset: usize,
    pub(crate) schema_list_count: usize,
    pub(crate) schema_list_filtered: bool,
    pub(crate) editor: Rect,
    pub(crate) tab_bar: Rect,
    pub(crate) results: Option<Rect>,
    pub(crate) results_tab_bar: Option<Rect>,
    pub(crate) cursor_shape: CursorShape,
    /// Upper bound for `help_scroll`: beyond this the bottom of the
    /// help overlay is already visible. Recomputed each frame from the
    /// current terminal size so `j` / `Down` / wheel-down saturate at
    /// the last meaningful scroll offset.
    pub(crate) help_max_scroll: u16,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw<H: Host>(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &mut Editor<hjkl_buffer::Buffer, H>,
    command_input: Option<&TextInput>,
    rename_input: Option<&TextInput>,
    file_picker: Option<&mut hjkl_picker::Picker>,
    delete_confirm: Option<&str>,
    destructive_confirm: Option<&str>,
    quit_prompt_dirty: Option<&[String]>,
    sqls_prompt_open: bool,
    schema_search: &SchemaSearch,
    editor_search_text: Option<&str>,
    last_editor_search: Option<&str>,
    results_search_text: Option<&str>,
    hover_search_text: Option<&str>,
    sig_help_text: Option<&str>,
    toasts: &[(String, ToastKind)],
    cursorline: bool,
    cursorcolumn: bool,
) -> DrawAreas {
    let area = f.area();

    let lsp_warn = !state.lsp_available;

    // Always reserve 1 row for the status bar; optionally 1 more for LSP warning above it.
    let (main_area, lsp_warn_area, status_area) = if lsp_warn {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        (chunks[0], Some(chunks[1]), chunks[2])
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (chunks[0], None, chunks[1])
    };

    let outer_raw = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![
            Constraint::Min(30),
            Constraint::Length(1),
            Constraint::Percentage(85),
        ])
        .split(main_area);
    let outer: Vec<Rect> = {
        let sep = outer_raw[1];
        f.render_widget(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(ui().pane_sep).bg(ui().schema_pane_bg)),
            sep,
        );
        vec![outer_raw[0], outer_raw[2]]
    };

    let schema_focused = state.focus == Focus::Schema;
    let editor_focused = state.focus == Focus::Editor;
    let results_focused = state.focus == Focus::Results;

    // Schema panel
    let (
        schema_list_area,
        schema_list_offset,
        schema_list_count,
        schema_list_filtered,
        schema_search_cursor,
    ) = draw_schema(f, state, outer[0], schema_focused, schema_search);

    let show_results = state.has_results();
    let editor_pct = (state.editor_ratio * 100.0) as u16;
    let results_pct = 100 - editor_pct;

    let right_chunks = if show_results {
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

    // Tab bar is the top row of the editor pane, flush with no padding.
    let tab_bar = Rect {
        x: right_chunks[0].x,
        y: right_chunks[0].y,
        width: right_chunks[0].width,
        height: 1,
    };
    let results_tab_bar = if show_results {
        let results_area = right_chunks[1];
        if state.result_tabs.len() > 1 && results_area.height > 2 {
            // Tab bar now sits beneath a 1-row separator at the top of the
            // results pane — shift its y accordingly for click hit-testing.
            Some(Rect {
                x: results_area.x + 1,
                y: results_area.y + 1,
                width: results_area.width.saturating_sub(2),
                height: 1,
            })
        } else {
            None
        }
    } else {
        None
    };
    let mut areas = DrawAreas {
        schema_list_area,
        schema_list_offset,
        schema_list_count,
        schema_list_filtered,
        editor: right_chunks[0],
        tab_bar,
        results: if show_results {
            Some(right_chunks[1])
        } else {
            None
        },
        results_tab_bar,
        cursor_shape: CursorShape::Hidden,
        help_max_scroll: 0,
    };

    draw_editor(
        f,
        state,
        editor,
        right_chunks[0],
        editor_focused,
        (editor_search_text, last_editor_search),
        (cursorline, cursorcolumn),
    );

    if show_results {
        draw_results(
            f,
            state,
            right_chunks[1],
            results_focused,
            results_search_text,
        );
    }

    // Completion popup (overlay).  Use viewport-relative coordinates so
    // the popup stays inside the editor even when the cursor lives deep
    // in a long file.
    if state.show_completions && !state.completions.is_empty() {
        let (cur_row, cur_col) = editor.cursor();
        let top_row = editor.host().viewport().top_row;
        let top_col = editor.host().viewport().top_col;
        let screen_row = cur_row.saturating_sub(top_row);
        let screen_col = cur_col.saturating_sub(top_col);
        draw_completions(f, state, right_chunks[0], screen_row, screen_col);
    }

    // Signature help bar: one-line overlay above the cursor, shown while
    // Insert mode is active after `(` or `,`. Suppresses hover popup
    // (signature wins over hover when both would otherwise be visible).
    if let Some(text) = sig_help_text {
        let (cur_row, cur_col) = editor.cursor();
        let top_row = editor.host().viewport().top_row;
        let top_col = editor.host().viewport().top_col;
        let screen_row = cur_row.saturating_sub(top_row);
        let screen_col = cur_col.saturating_sub(top_col);
        draw_sig_help_bar(f, right_chunks[0], screen_row, screen_col, text);
    } else {
        // Hover popup (LSP `K`). Centered borderless dialog matching the
        // command / rename / delete / file-picker styling — dialog_bg as
        // the only chrome, padded inside, `Clear` under it to punch
        // through whatever the editor drew.
        if let Some(ref table) = state.hover_table {
            draw_hover_table(f, area, state, table, hover_search_text);
        } else if let Some(ref text) = state.hover_text {
            draw_hover_popup(f, area, state.hover_scroll, text);
        } else if state.hover_loading {
            draw_hover_loading(f, area);
        }
    }

    // sqls install prompt modal (shown on startup when sqls is missing)
    if sqls_prompt_open {
        draw_sqls_prompt_modal(f, area);
    }

    // Connection switcher modal (top-level overlay)
    if state.show_connection_switcher {
        draw_connection_switcher(f, state, area);
    }

    // pgpass picker (sits above connection switcher, below add-connection)
    if state.show_pgpass_picker {
        draw_pgpass_picker(f, state, area);
    }

    // Add connection dialog (above switcher and pgpass picker)
    let mut add_connection_cursor: Option<(u16, u16)> = None;
    if state.show_add_connection {
        add_connection_cursor = Some(draw_add_connection(f, state, area));
    }

    // Help overlay (topmost)
    if state.show_help {
        areas.help_max_scroll = draw_help(f, area, state.help_scroll);
    }

    // Connection-error details popup. Sits above help / switcher /
    // add — when a connect fails the popup is the only useful next
    // step, so it wins z-order against everything except toasts.
    if state.show_connect_error_popup
        && let Some(err) = state.schema_connect_error.as_deref()
    {
        let name = state.active_connection.as_deref();
        let headline = state
            .schema_connect_error_kind
            .map(|k| k.headline())
            .unwrap_or("Connection failed");
        draw_connect_error_popup(f, area, headline, err, name);
    }

    // LSP warning bar (above status bar)
    if let Some(warn_area) = lsp_warn_area {
        let msg = Paragraph::new(Span::styled(
            format!(" ⚠ LSP not available ({})", state.lsp_binary),
            Style::default().fg(ui().lsp_warn_fg).bg(ui().lsp_warn_bg),
        ));
        f.render_widget(msg, warn_area);
    }

    // Status bar (always at bottom)
    draw_status_bar(f, state, editor, status_area);

    // Command palette: small centered dialog, no borders, 2-col + 1-row padding.
    let mut dialog_cursor: Option<(u16, u16)> = None;
    if let Some(cmd) = command_input {
        dialog_cursor = Some(draw_input_dialog(f, area, ": ", cmd));
    }

    // Rename prompt: same shape as command palette.
    if let Some(name) = rename_input {
        dialog_cursor = Some(draw_input_dialog(f, area, "> ", name));
    }

    // Editor `/` / `?` search: same shape as command palette. The
    // editor owns the prompt state; we read it for render via
    // `editor.search_prompt()`.
    if let Some(prompt) = editor.search_prompt() {
        let prefix = if prompt.forward { "/ " } else { "? " };
        let input = TextInput {
            text: prompt.text.clone(),
            cursor: prompt.cursor,
        };
        dialog_cursor = Some(draw_input_dialog(f, area, prefix, &input));
    }

    // Delete confirmation: centered borderless dialog.
    if let Some(name) = delete_confirm {
        draw_confirm_dialog(f, area, &format!("Delete '{name}'?  (y / n)"));
    }

    // Destructive-run guard: centered borderless dialog.
    if let Some(label) = destructive_confirm {
        draw_confirm_dialog(f, area, &format!("Run {label}?  (y / n)"));
    }

    // Quit confirmation when there are unsaved buffers.
    if let Some(names) = quit_prompt_dirty {
        let list = if names.len() <= 3 {
            names.join(", ")
        } else {
            format!("{} + {} more", names[..3].join(", "), names.len() - 3)
        };
        draw_confirm_dialog(
            f,
            area,
            &format!("Save unsaved buffers [{list}]?  (y=save / n=discard / c=cancel)"),
        );
    }

    // File picker (leader+space): centered dialog with input + scrollable list.
    if let Some(picker) = file_picker {
        let active_name = state.tabs.get(state.active_tab).map(|t| t.name.as_str());
        dialog_cursor = Some(draw_file_picker(f, area, picker, active_name));
    }

    // Toast notifications (top-right corner, stacked vertically).
    // Each toast pads 1 row top + bottom around the message. Multi-line
    // messages (e.g. `:reg`, `:marks`) expand the box vertically and widen
    // it to the longest line.
    let mut y_off: u16 = 0;
    for (msg, kind) in toasts {
        let style = match kind {
            ToastKind::Error => Style::default()
                .fg(ui().toast_error_fg)
                .bg(ui().toast_error_bg),
            ToastKind::Info => Style::default()
                .fg(ui().toast_info_fg)
                .bg(ui().toast_info_bg),
        };
        let msg_lines: Vec<&str> = msg.lines().collect();
        let line_count = msg_lines.len().max(1) as u16;
        let max_line = msg_lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let width = (max_line + 4).min(area.width);
        let height = (line_count + 2).min(area.height.saturating_sub(y_off));
        if height == 0 {
            break;
        }
        let toast_area = Rect {
            x: area.width.saturating_sub(width),
            y: y_off,
            width,
            height,
        };
        f.render_widget(Clear, toast_area);
        f.render_widget(Block::default().style(style), toast_area);
        if height >= 2 {
            let msg_area = Rect {
                x: toast_area.x + 2,
                y: toast_area.y + 1,
                width: toast_area.width.saturating_sub(4),
                height: height.saturating_sub(2),
            };
            f.render_widget(Paragraph::new(msg.as_str()).style(style), msg_area);
        }
        y_off = y_off.saturating_add(height).saturating_add(1);
    }

    // Pick the active cursor target: dialogs > add-connection > schema search >
    // editor (when focused). Bar shape for any text-input context, Block for
    // editor normal mode.
    let (cursor_pos, shape) = if let Some(p) = dialog_cursor {
        (Some(p), CursorShape::Bar)
    } else if let Some(p) = add_connection_cursor {
        (Some(p), CursorShape::Bar)
    } else if let Some(p) = schema_search_cursor {
        (Some(p), CursorShape::Bar)
    } else if state.focus == Focus::Editor
        && !state.show_help
        && !state.show_connection_switcher
        && !state.show_pgpass_picker
    {
        // Reconstruct the textarea rect that draw_editor uses:
        // top row is the tab bar, then a 1-col horizontal margin around the body.
        let pane = right_chunks[0];
        let textarea_rect = Rect {
            x: pane.x.saturating_add(1),
            y: pane.y.saturating_add(1),
            width: pane.width.saturating_sub(2),
            height: pane.height.saturating_sub(1),
        };
        let pos = editor.cursor_screen_pos(
            textarea_rect.x,
            textarea_rect.y,
            textarea_rect.width,
            textarea_rect.height,
            EDITOR_SIGN_COL_WIDTH,
        );
        let shape = if state.vim_mode == VimMode::Insert {
            CursorShape::Bar
        } else {
            CursorShape::Block
        };
        (pos, shape)
    } else {
        (None, CursorShape::Hidden)
    };
    if let Some((x, y)) = cursor_pos {
        f.set_cursor_position((x, y));
    }
    areas.cursor_shape = shape;

    areas
}

pub(crate) fn extract_results_left_click(
    x: u16,
    y: u16,
    areas: &DrawAreas,
    state: &AppState,
) -> Option<(String, &'static str, ResultsCursor)> {
    let results_area = areas.results?;
    use ratatui::layout::Position;
    if !results_area.contains(Position { x, y }) {
        return None;
    }
    let tab_bar_rows: u16 = if state.result_tabs.len() > 1 { 2 } else { 0 };
    // Shared query-row hit-test: row 3 below the tab bar is the query line
    // for every pane that shows it (Results/Error/Cancelled when a query is
    // attached). Clicking it copies the query verbatim.
    let query_text = state
        .active_result()
        .map(|t| t.query.clone())
        .unwrap_or_default();
    let has_query = !query_text.trim().is_empty();
    let pane_has_query_row = matches!(
        state.results(),
        sqeel_core::state::ResultsPane::Results(_)
            | sqeel_core::state::ResultsPane::Cancelled
            | sqeel_core::state::ResultsPane::Skipped
            | sqeel_core::state::ResultsPane::Error(_)
            | sqeel_core::state::ResultsPane::NonQuery { .. }
    ) && has_query;
    if pane_has_query_row
        && y == results_area.y + tab_bar_rows + 3
        && x >= results_area.x
        && x < results_area.x + results_area.width
    {
        return Some((query_text, "Query", ResultsCursor::Query));
    }
    match state.results() {
        sqeel_core::state::ResultsPane::Results(r) => {
            let header_y = results_area.y + tab_bar_rows + 5;
            let body_y = results_area.y + tab_bar_rows + 7;
            let body_x = results_area.x + 1;
            if y < header_y || y == header_y + 1 {
                return None;
            }
            let char_offset: usize = r
                .col_widths
                .iter()
                .take(state.results_col_scroll())
                .map(|&w| w as usize + 1)
                .sum();
            let rel = (x.saturating_sub(body_x) as usize).saturating_add(char_offset);
            let mut cursor_x = 0usize;
            let mut col_idx: Option<usize> = None;
            for (i, &w) in r.col_widths.iter().enumerate() {
                let col_w = w as usize;
                if rel < cursor_x + col_w {
                    col_idx = Some(i);
                    break;
                }
                cursor_x += col_w;
                if i + 1 < r.col_widths.len() {
                    if rel == cursor_x {
                        return None;
                    }
                    cursor_x += 1;
                }
            }
            let col_idx = col_idx?;
            if y == header_y {
                let name = r.columns.get(col_idx)?.clone();
                return Some((name, "Column", ResultsCursor::Header(col_idx)));
            }
            if y < body_y {
                return None;
            }
            let row_idx = (y - body_y) as usize + state.results_scroll();
            let value = r.rows.get(row_idx)?.get(col_idx)?.trim().to_string();
            Some((
                value,
                "Value",
                ResultsCursor::Cell {
                    row: row_idx,
                    col: col_idx,
                },
            ))
        }
        sqeel_core::state::ResultsPane::Error(e) => {
            let content_y = results_area.y + tab_bar_rows;
            if y < content_y {
                return None;
            }
            let rel_y = (y - content_y) as usize;
            let query = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();
            let (body_start, has_q) = if !query.trim().is_empty() {
                (5usize, true)
            } else {
                (3usize, false)
            };
            if has_q && rel_y == 3 {
                return Some((query.clone(), "Query", ResultsCursor::Query));
            }
            if rel_y >= body_start {
                let line_idx = rel_y - body_start + state.results_scroll();
                let line = e.lines().nth(line_idx)?.to_string();
                return Some((line, "Line", ResultsCursor::MessageLine(line_idx)));
            }
            None
        }
        pane @ (sqeel_core::state::ResultsPane::Cancelled
        | sqeel_core::state::ResultsPane::Skipped) => {
            let content_y = results_area.y + tab_bar_rows;
            if y < content_y {
                return None;
            }
            let rel_y = (y - content_y) as usize;
            let query = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();
            let has_q = !query.trim().is_empty();
            let body_start = if has_q { 5 } else { 3 };
            if has_q && rel_y == 3 {
                return Some((query, "Query", ResultsCursor::Query));
            }
            if rel_y >= body_start {
                let msg = if matches!(pane, sqeel_core::state::ResultsPane::Cancelled) {
                    "Query cancelled"
                } else {
                    "Skipped after earlier error"
                };
                return Some((msg.to_string(), "Line", ResultsCursor::MessageLine(0)));
            }
            None
        }
        sqeel_core::state::ResultsPane::NonQuery {
            verb,
            rows_affected,
        } => {
            let content_y = results_area.y + tab_bar_rows;
            if y < content_y {
                return None;
            }
            let rel_y = (y - content_y) as usize;
            let query = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();
            let has_q = !query.trim().is_empty();
            let body_start = if has_q { 5 } else { 3 };
            if has_q && rel_y == 3 {
                return Some((query, "Query", ResultsCursor::Query));
            }
            if rel_y >= body_start {
                return Some((
                    non_query_summary(verb, *rows_affected),
                    "Line",
                    ResultsCursor::MessageLine(0),
                ));
            }
            None
        }
        _ => None,
    }
}

/// One-line summary for the NonQuery results pane. DML verbs report
/// the affected row count; DDL / transaction control collapse to
/// "OK · {VERB}" since the row count is meaningless there.
pub(crate) fn non_query_summary(verb: &str, rows_affected: u64) -> String {
    let is_dml = matches!(
        verb,
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "MERGE" | "UPSERT"
    );
    if is_dml {
        let noun = if rows_affected == 1 { "row" } else { "rows" };
        format!("{verb} OK · {rows_affected} {noun} affected")
    } else {
        format!("{verb} OK")
    }
}

pub(crate) fn extract_results_row(
    x: u16,
    y: u16,
    areas: &DrawAreas,
    state: &AppState,
) -> Option<String> {
    let results_area = areas.results?;
    use ratatui::layout::Position;
    if !results_area.contains(Position { x, y }) {
        return None;
    }
    let r = match state.results() {
        sqeel_core::state::ResultsPane::Results(r) => r,
        _ => return None,
    };
    let tab_bar_rows: u16 = if state.result_tabs.len() > 1 { 2 } else { 0 };
    let body_y = results_area.y + tab_bar_rows + 7;
    if y < body_y {
        return None;
    }
    let row_idx = (y - body_y) as usize + state.results_scroll();
    r.rows.get(row_idx).map(|row| row.join("\t"))
}

pub(crate) fn draw_status_bar<H: Host>(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &Editor<hjkl_buffer::Buffer, H>,
    area: Rect,
) {
    let mode = mode_label(state);
    let mode_width = mode.content.len() as u16;

    let conn = state
        .active_connection
        .as_deref()
        .unwrap_or("no connection");
    let tab_name = state
        .tabs
        .get(state.active_tab)
        .map(|t| t.name.as_str())
        .unwrap_or("");
    let center_text = format!(" {conn} › {tab_name} ");

    let (row, col) = editor.cursor();
    let cursor_str = format!(" {}:{} ", row + 1, col + 1);
    let cursor_width = cursor_str.len() as u16;

    let diag = diag_label(state);
    let diag_width = diag.as_ref().map(|s| s.content.len() as u16).unwrap_or(0);

    let search = search_label(editor);
    let search_width = search.as_ref().map(|s| s.content.len() as u16).unwrap_or(0);

    let right_width = cursor_width + diag_width + search_width;
    let center_width = area.width.saturating_sub(mode_width + right_width);

    // Mode block (left)
    let mode_area = Rect {
        x: area.x,
        y: area.y,
        width: mode_width.min(area.width),
        height: 1,
    };
    // Center info
    let center_area = Rect {
        x: area.x + mode_width,
        y: area.y,
        width: center_width.min(area.width.saturating_sub(mode_width)),
        height: 1,
    };
    // Right side (search + diag + cursor)
    let right_x = area.x + mode_width + center_width;
    let search_area = Rect {
        x: right_x,
        y: area.y,
        width: search_width,
        height: 1,
    };
    let diag_area = Rect {
        x: right_x + search_width,
        y: area.y,
        width: diag_width,
        height: 1,
    };
    let cursor_area = Rect {
        x: right_x + search_width + diag_width,
        y: area.y,
        width: cursor_width.min(
            area.width
                .saturating_sub(mode_width + center_width + search_width + diag_width),
        ),
        height: 1,
    };

    let bar_bg = Style::default()
        .bg(ui().status_bar_bg)
        .fg(ui().status_bar_fg);

    // Mode label (colored fg, same bg as status bar)
    let mode_style = Style::default()
        .bg(mode.style.fg.unwrap_or(ui().status_mode_normal))
        .fg(ui().status_mode_fg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(Span::styled(mode.content.to_string(), mode_style)),
        mode_area,
    );

    // Center: connection > tab
    f.render_widget(Paragraph::new(center_text).style(bar_bg), center_area);

    // Search match counter
    if let Some(s) = search {
        let style = Style::default()
            .bg(ui().status_search_bg)
            .fg(ui().status_search_fg)
            .add_modifier(Modifier::BOLD);
        f.render_widget(
            Paragraph::new(Span::styled(s.content.to_string(), style)),
            search_area,
        );
    }

    // Diagnostics
    if let Some(d) = diag {
        let diag_style = Style::default()
            .bg(d.style.fg.unwrap_or(ui().status_diag_warning))
            .fg(ui().status_mode_fg)
            .add_modifier(Modifier::BOLD);
        f.render_widget(
            Paragraph::new(Span::styled(d.content.to_string(), diag_style)),
            diag_area,
        );
    }

    // Cursor position (right-aligned, highlighted)
    let cursor_style = Style::default()
        .bg(ui().status_hint_bg)
        .fg(ui().status_hint_fg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(Span::styled(cursor_str, cursor_style)),
        cursor_area,
    );
}

pub(crate) fn schema_item_line(item: &SchemaTreeItem, u: &theme::UiColors) -> Line<'static> {
    let indent = " ".repeat(1 + item.depth * 2);
    if let SchemaItemKind::Placeholder { loading } = item.kind {
        // Greyed-out hint row; for loading rows also render the shared
        // spinner frame so the user knows something is still in flight.
        let style = Style::default()
            .fg(u.schema_placeholder_fg)
            .add_modifier(Modifier::ITALIC);
        let mut spans = vec![Span::raw(indent)];
        if loading {
            spans.push(Span::styled(format!("{} ", spinner_frame()), style));
        }
        spans.push(Span::styled(item.name.clone(), style));
        return Line::from(spans);
    }
    // Group header rows (IndexGroup / ForeignKeyGroup) — DIM, no icon.
    if let SchemaItemKind::IndexGroup { .. } | SchemaItemKind::ForeignKeyGroup { .. } = &item.kind {
        let style = Style::default()
            .fg(u.schema_placeholder_fg)
            .add_modifier(Modifier::DIM);
        return Line::from(vec![
            Span::raw(indent),
            Span::styled(item.name.clone(), style),
        ]);
    }
    let (icon, icon_color) = match &item.kind {
        SchemaItemKind::Database => ("󰆼", u.schema_icon_db),
        SchemaItemKind::Table => ("󰓫", u.schema_icon_table),
        SchemaItemKind::Column { is_pk: true, .. } => ("󰌆", u.schema_icon_pk),
        SchemaItemKind::Column { .. } => ("󱘚", u.schema_icon_column),
        SchemaItemKind::Index { .. } => ("󰠞", u.schema_icon_column),
        SchemaItemKind::ForeignKey { .. } => ("󰈩", u.schema_icon_pk),
        SchemaItemKind::Placeholder { .. } => unreachable!("handled above"),
        SchemaItemKind::IndexGroup { .. } | SchemaItemKind::ForeignKeyGroup { .. } => {
            unreachable!("handled above")
        }
    };
    let mut spans = vec![
        Span::raw(indent),
        Span::styled(icon.to_string(), Style::default().fg(icon_color)),
        Span::raw(format!(" {}", item.name)),
    ];
    if let SchemaItemKind::Column { type_name, .. } = &item.kind
        && !type_name.is_empty()
    {
        spans.push(Span::raw(": "));
        spans.push(Span::styled(
            type_name.clone(),
            Style::default().fg(u.schema_type_fg),
        ));
    }
    Line::from(spans)
}

pub(crate) fn draw_schema(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    area: Rect,
    focused: bool,
    search: &SchemaSearch,
) -> (Rect, usize, usize, bool, Option<(u16, u16)>) {
    let searching = search.focused;
    let search_cursor = search.cursor;
    let title = if state.schema_loading {
        format!("Explorer {}", spinner_frame())
    } else if state.schema_nodes.is_empty() {
        "Explorer".to_string()
    } else {
        let count = state.schema_nodes.len();
        format!("Explorer ✓ ({count})")
    };

    let border_style = if focused {
        Style::default().fg(ui().schema_border_focus)
    } else {
        Style::default()
    };

    // Fill pane background (full area), then inset content by 1 on all sides.
    f.render_widget(
        Block::default().style(Style::default().bg(ui().schema_pane_bg)),
        area,
    );

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });

    // Search box is always visible (3 rows: border+input+border), list below
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);

    let query = search.query().unwrap_or("");
    let has_filter = !query.is_empty();
    // Magnifier glyph + space prefix marks this as the search input.
    let prefix = "🔍 ";
    let input_text = format!("{prefix}{query}");
    let text_cursor = search.query.as_ref().map(|q| q.cursor).unwrap_or(0);
    // The magnifier emoji is 2 cells wide; total prefix width = 3 cells.
    let prefix_cells: u16 = 3;
    let search_cursor_pos = if searching {
        Some((
            chunks[0].x + 1 + prefix_cells + text_cursor as u16,
            chunks[0].y + 1,
        ))
    } else {
        None
    };
    let search_block = Block::default()
        .title(title.clone())
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(if searching {
            Style::default().fg(ui().schema_border_focus)
        } else if has_filter {
            Style::default().fg(ui().schema_border_filter)
        } else {
            border_style
        });
    f.render_widget(Paragraph::new(input_text).block(search_block), chunks[0]);

    // Inset 1 char left+right to align with search box inner content
    let list_area = Rect {
        x: chunks[1].x + 1,
        y: chunks[1].y,
        width: chunks[1].width.saturating_sub(2),
        height: chunks[1].height,
    };

    let items: Vec<&SchemaTreeItem> = if has_filter {
        schema::filter_items(state.all_schema_items(), query)
    } else {
        state.visible_schema_items().iter().collect()
    };

    let item_count = items.len();

    if items.is_empty() {
        let placeholder: String = if has_filter {
            "No matches".into()
        } else if state.schema_connect_error.is_some() {
            let name = state.active_connection.as_deref();
            let retry_hint = match name {
                Some(n) => format!("\n[r] retry {n}"),
                None => "\n[r] retry".into(),
            };
            let headline = state
                .schema_connect_error_kind
                .map(|k| k.headline())
                .unwrap_or("Connection failed");
            format!("{headline}\n[Enter] details{retry_hint}")
        } else if state.schema_connecting {
            match state.active_connection.as_deref() {
                Some(name) => format!("Connecting to {name}..."),
                None => "Connecting...".into(),
            }
        } else if state.active_connection.is_some() {
            "Loading...".into()
        } else {
            "No connection".into()
        };
        f.render_widget(Paragraph::new(placeholder), list_area);
        return (list_area, 0, 0, has_filter, search_cursor_pos);
    }

    let u = ui();
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|item| ListItem::new(schema_item_line(item, &u)))
        .collect();

    // When search box is actively focused, don't highlight the list.
    // In filter-nav mode (filter active, box not focused) use search_cursor.
    // Normal mode uses state.schema_cursor.
    let cursor = if has_filter {
        search_cursor
    } else {
        state.schema_cursor
    };
    let (highlight_style, selected) = if searching {
        (Style::default(), None)
    } else if focused {
        (Style::default().bg(ui().schema_sel_active_bg), Some(cursor))
    } else {
        (
            Style::default().bg(ui().schema_sel_inactive_bg),
            Some(cursor),
        )
    };

    // Publish viewport height so cursor-nav helpers on AppState can keep the
    // selection visible without needing the draw metrics plumbed through.
    state
        .schema_viewport_rows
        .store(list_area.height, std::sync::atomic::Ordering::Relaxed);

    let height = list_area.height as usize;
    let max_offset = item_count.saturating_sub(height.max(1));
    let offset = state.schema_scroll_offset.min(max_offset);
    // Only mark the row as "selected" when it's actually inside the viewport;
    // otherwise ratatui's List would fight our offset and snap back to the
    // cursor every frame.
    let selected_visible = selected.and_then(|c| {
        if height > 0 && c >= offset && c < offset + height {
            Some(c)
        } else {
            None
        }
    });

    let list = List::new(list_items).highlight_style(highlight_style);
    let mut list_state = ListState::default()
        .with_offset(offset)
        .with_selected(selected_visible);
    f.render_stateful_widget(list, list_area, &mut list_state);
    (
        list_area,
        list_state.offset(),
        item_count,
        has_filter,
        search_cursor_pos,
    )
}

pub(crate) fn draw_editor<H: Host>(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &mut Editor<hjkl_buffer::Buffer, H>,
    area: Rect,
    focused: bool,
    // (active_search, last_search) — bundled to stay under clippy's arg-count limit.
    search: (Option<&str>, Option<&str>),
    // (cursorline, cursorcolumn) — bundled to stay under clippy's arg-count limit.
    cursor_opts: (bool, bool),
) {
    let (editor_search, last_editor_search) = search;
    // Fill pane background
    f.render_widget(
        Block::default().style(Style::default().bg(ui().editor_pane_bg)),
        area,
    );

    // Tab bar sits flush at the top (full-width, no padding); the remaining
    // content below is inset by 1 on all sides.
    let tab_bar_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let body_outer = Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    let inner = body_outer.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });

    // Show first diagnostic message if any
    let diag_line = state
        .lsp_diagnostics
        .first()
        .map(|d| format!(" {}:{} {}", d.line + 1, d.col + 1, d.message));

    // Split inner: textarea + optional diag (1)
    let mut constraints = vec![Constraint::Min(1)];
    if diag_line.is_some() {
        constraints.push(Constraint::Length(1));
    }
    let body_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    // Build a `chunks`-like slice: [tab_bar, textarea, ...extras] so the rest of
    // this function (which references chunks[0..]) keeps working unchanged.
    let mut chunks: Vec<Rect> = vec![tab_bar_area];
    chunks.extend(body_chunks.iter().copied());

    f.render_widget(
        Paragraph::new(build_tab_title(state)).style(Style::default().bg(ui().editor_tab_bar_bg)),
        chunks[0],
    );

    let cursor_line_bg = if focused {
        ui().editor_cursor_line_active
    } else {
        ui().editor_cursor_line_inactive
    };

    // Phase 7d-ii-c: render through `BufferView` instead of the
    // textarea widget. The textarea field stays as the input /
    // edit authority for now (Phase 7f rips it); the buffer mirrors
    // its content + cursor + viewport + spans on every step, so the
    // render is byte-for-byte derived from the buffer state.
    editor.set_viewport_height(chunks[1].height);
    // Gutter width must mirror `Editor::cursor_screen_pos`'s reserved
    // number column or the terminal cursor lands off by one column.
    // `lnum_width()` is the engine's own calculation (hjkl#96), so the
    // renderer, the cursor placement, and the mouse translation all
    // agree — including `:set nonumber`, where it collapses to 0.
    let gutter_width = editor.lnum_width();
    let wrap_mode = editor.settings().wrap;
    {
        let v = editor.host_mut().viewport_mut();
        v.width = chunks[1].width;
        v.height = chunks[1].height;
        v.text_width = chunks[1]
            .width
            .saturating_sub(gutter_width + EDITOR_SIGN_COL_WIDTH);
        v.wrap = wrap_mode;
    }

    // Plumb the host's `/` search regex into the buffer so
    // `BufferView` can paint search bg from the cached regex.
    // Visual mode suppresses the highlight so selection isn't
    // out-shouted by it (matches the previous textarea wiring).
    let search_query = if state.vim_mode == VimMode::Visual || state.vim_mode == VimMode::VisualLine
    {
        None
    } else {
        editor_search.or(last_editor_search)
    };
    let search_pattern = search_query
        .filter(|q| !q.is_empty())
        .and_then(|q| regex::Regex::new(q).ok());
    editor.set_search_pattern(search_pattern);

    // Gutter width matches what tui-textarea reserved: digit count
    // for the largest line number, plus a leading + trailing space.
    let gutter = hjkl_buffer_tui::Gutter {
        width: gutter_width,
        style: Style::default().fg(ui().editor_line_num),
        line_offset: 0,
        numbers: hjkl_buffer_tui::GutterNumbers::Absolute,
        sign_column_width: EDITOR_SIGN_COL_WIDTH,
        fold_column_width: 0,
    };
    // Gutter diagnostic signs: highest severity per row wins
    // (error > warning). Painted by `BufferView` as part of its
    // gutter pass — no post-render overlay.
    let signs: Vec<hjkl_buffer_tui::Sign> = state
        .lsp_diagnostics
        .iter()
        .filter_map(|d| match d.severity {
            lsp_types::DiagnosticSeverity::ERROR => Some(hjkl_buffer_tui::Sign {
                row: d.line as usize,
                ch: '●',
                style: Style::default().fg(ui().status_diag_error),
                priority: 2,
            }),
            lsp_types::DiagnosticSeverity::WARNING => Some(hjkl_buffer_tui::Sign {
                row: d.line as usize,
                ch: '⚠',
                style: Style::default().fg(ui().status_diag_warning),
                priority: 1,
            }),
            _ => None,
        })
        .collect();

    let style_table: Vec<Style> = editor.ratatui_style_table();
    let resolver = move |id: u32| style_table.get(id as usize).copied().unwrap_or_default();
    let selection = editor.buffer_selection();
    let (cursorline, cursorcolumn) = cursor_opts;
    let cursorline_style = if cursorline {
        Style::default().bg(cursor_line_bg)
    } else {
        Style::default()
    };
    let cursorcolumn_style = if cursorcolumn {
        Style::default().bg(ui().sql_cursor_column_bg)
    } else {
        Style::default()
    };
    let view = hjkl_buffer_tui::BufferView {
        buffer: editor.buffer(),
        viewport: editor.host().viewport(),
        selection,
        resolver: &resolver,
        cursor_line_bg: cursorline_style,
        cursor_line_row: None,
        fold_line_bg: Style::default(),
        folds_override: None,
        cursor_column_bg: cursorcolumn_style,
        selection_bg: Style::default().add_modifier(Modifier::REVERSED),
        cursor_style: Style::default().bg(cursor_line_bg),
        gutter: Some(gutter),
        search_bg: Style::default()
            .bg(ui().editor_search_bg)
            .fg(ui().editor_search_fg),
        signs: &signs,
        conceals: &[],
        spans: editor.buffer_spans(),
        search_pattern: editor.search_state().pattern.as_ref(),
        non_text_style: Style::default(),
        show_eob: false,
        diag_overlays: &[],
        colorcolumn_cols: &[],
        colorcolumn_style: Style::default(),
        listchars: None,
        indent_guides_enabled: false,
        indent_guide_char: '│',
        indent_guide_shiftwidth: 4,
        indent_guide_fg: Color::DarkGray,
        indent_guide_active_fg: Color::DarkGray,
        indent_guide_active_col: None,
        eol_hints: &[],
        blame_plan: None,
        diff_filler: None,
    };
    f.render_widget(view, chunks[1]);

    if let Some(msg) = diag_line {
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(ui().editor_error_fg)),
            chunks[2],
        );
    }
}

pub(crate) fn build_tab_title(state: &AppState) -> Line<'_> {
    let mut spans: Vec<Span> = vec![];
    for (i, tab) in state.tabs.iter().enumerate() {
        let active = i == state.active_tab;
        let style = if active {
            Style::default()
                .fg(ui().tab_active_fg)
                .bg(ui().tab_active_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(ui().tab_inactive_fg)
        };
        spans.push(Span::styled(format!(" {} ", tab.name), style));
        if i + 1 < state.tabs.len() {
            spans.push(Span::styled("│", Style::default().fg(ui().tab_sep_fg)));
        }
    }
    Line::from(spans)
}

pub(crate) fn draw_results(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    area: Rect,
    focused: bool,
    search_text: Option<&str>,
) {
    // Fill pane background (full area), then inset content by 1 on all sides.
    f.render_widget(
        Block::default().style(Style::default().bg(ui().results_pane_bg)),
        area,
    );
    let area = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    // When the `/` prompt is live, carve one row off the bottom and
    // render the search input there; the rest of the pane lays out as
    // usual in the shrunk area.
    let (area, prompt_area) = if search_text.is_some() && area.height > 1 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    // Split off a separator + 1-row tab bar at the top when there are multiple
    // result tabs. The separator sits above the tab strip.
    let sep_style = Style::default().fg(ui().results_sep);
    let (tab_bar_area, content_area) = if state.result_tabs.len() > 1 && area.height > 2 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(area);
        let hr: String = "─".repeat(area.width as usize);
        f.render_widget(Paragraph::new(hr).style(sep_style), chunks[0]);
        (Some(chunks[1]), chunks[2])
    } else {
        (None, area)
    };

    if let Some(tab_area) = tab_bar_area {
        f.render_widget(results_tab_bar(state), tab_area);
    }

    match state.results() {
        ResultsPane::Results(r) => {
            let title_style = if focused {
                Style::default().fg(ui().results_title_active)
            } else {
                Style::default().fg(ui().results_title_inactive)
            };

            let query_text = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();

            // `SHOW CREATE TABLE/VIEW/...` returns a single row whose last
            // column holds the DDL. Render that as a syntax-highlighted block
            // instead of a 1x2 table, which is unreadable.
            if is_show_create(&query_text)
                && r.rows.len() == 1
                && r.columns.len() >= 2
                && let Some(ddl) = r.rows[0].last()
            {
                let sep_style = Style::default().fg(ui().results_sep);
                let title = if state.result_tabs.len() > 1 {
                    format!(
                        " Results ({}/{} • DDL)",
                        state.active_result_tab + 1,
                        state.result_tabs.len()
                    )
                } else {
                    " Results (DDL)".to_string()
                };
                let query_line = highlight_query_line(&query_text, state.active_dialect);
                let body_lines = highlight_sql_lines(ddl, state.active_dialect);
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Min(0),
                    ])
                    .split(content_area);
                let hr: String = "─".repeat(content_area.width as usize);
                f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[0]);
                f.render_widget(Paragraph::new(title).style(title_style), chunks[1]);
                f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[2]);
                f.render_widget(Paragraph::new(query_line), chunks[3]);
                f.render_widget(Paragraph::new(hr).style(sep_style), chunks[4]);
                let body_area = chunks[5];
                state
                    .results_body_rows
                    .store(body_area.height, std::sync::atomic::Ordering::Relaxed);
                state
                    .results_body_width
                    .store(body_area.width, std::sync::atomic::Ordering::Relaxed);
                let v_scroll = state.results_scroll().min(body_lines.len()) as u16;
                let h_scroll = state.results_col_scroll() as u16;
                f.render_widget(
                    Paragraph::new(body_lines).scroll((v_scroll, h_scroll)),
                    body_area,
                );
                return;
            }

            let title = if state.result_tabs.len() > 1 {
                format!(
                    " Results ({}/{} • {} rows)",
                    state.active_result_tab + 1,
                    state.result_tabs.len(),
                    r.rows.len()
                )
            } else {
                format!(" Results ({} rows)", r.rows.len())
            };
            let col_start = state.results_col_scroll();
            let sep_style = Style::default().fg(ui().results_sep);
            let header_style = Style::default()
                .fg(ui().results_header_active)
                .add_modifier(Modifier::BOLD);

            // Char offset into the full-width row string, derived from col_scroll.
            // Each rendered column is padded to col_widths[i], separated by `│`.
            let char_offset: u16 = r
                .col_widths
                .iter()
                .take(col_start)
                .map(|&w| w as u32 + 1)
                .sum::<u32>() as u16;

            let cursor = state.active_result().map(|t| t.cursor);
            let col_bg = results_cursor_bg(focused);
            let cursor_bg = results_cursor_bg_strong(focused);
            // Highlighted column (Header or Cell cursor) — whole column gets muted bg.
            let active_col: Option<usize> = match cursor {
                Some(ResultsCursor::Header(c)) | Some(ResultsCursor::Cell { col: c, .. }) => {
                    Some(c)
                }
                _ => None,
            };
            let cursor_row: Option<usize> = match cursor {
                Some(ResultsCursor::Cell { row, .. }) => Some(row),
                _ => None,
            };

            let selection_bounds = state.results_selection_bounds();
            let header_cursor_col = if matches!(cursor, Some(ResultsCursor::Header(_))) {
                active_col
            } else {
                None
            };
            let (header_line, body_lines) = render_grid_lines(
                &r.columns,
                &r.rows,
                &r.col_widths,
                cursor_row,
                active_col,
                header_cursor_col,
                selection_bounds,
                state.results_scroll(),
                header_style,
                sep_style,
                Style::default().bg(cursor_bg),
                Style::default().bg(col_bg),
            );

            let mut query_line = highlight_query_line(&query_text, state.active_dialect);
            if cursor == Some(ResultsCursor::Query) {
                let qbg = results_cursor_bg(focused);
                query_line = Line::from(
                    query_line
                        .spans
                        .into_iter()
                        .map(|s| {
                            let st = s.style.bg(qbg);
                            Span::styled(s.content, st)
                        })
                        .collect::<Vec<_>>(),
                );
            }

            // Split content_area: hr (1) + title (1) + hr (1) + query (1) + hr (1) + header (1) + hr (1) + body (rest).
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .split(content_area);

            let hr: String = "─".repeat(content_area.width as usize);
            f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[0]);
            f.render_widget(Paragraph::new(title).style(title_style), chunks[1]);
            f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[2]);
            f.render_widget(Paragraph::new(query_line), chunks[3]);
            f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[4]);
            f.render_widget(
                Paragraph::new(header_line).scroll((0, char_offset)),
                chunks[5],
            );
            f.render_widget(Paragraph::new(hr).style(sep_style), chunks[6]);
            let body_area = chunks[7];
            use std::sync::atomic::Ordering;
            state.results_body_x.store(body_area.x, Ordering::Relaxed);
            state.results_body_y.store(body_area.y, Ordering::Relaxed);
            state
                .results_body_rows
                .store(body_area.height, Ordering::Relaxed);
            state
                .results_body_width
                .store(body_area.width, Ordering::Relaxed);
            f.render_widget(
                Paragraph::new(body_lines).scroll((0, char_offset)),
                body_area,
            );
        }
        ResultsPane::Error(e) => {
            let title_text = render_pos_title(state, "Result");
            let cursor = state.active_result().map(|t| t.cursor);
            let cursor_bg = results_cursor_bg(focused);
            let body: Vec<Line<'static>> = e
                .lines()
                .enumerate()
                .map(|(i, el)| {
                    let mut st = Style::default().fg(ui().results_error);
                    if cursor == Some(ResultsCursor::MessageLine(i)) {
                        st = st.bg(cursor_bg);
                    }
                    Line::from(Span::styled(format!(" {}", el), st))
                })
                .collect();
            let has_query = state
                .active_result()
                .map(|t| !t.query.trim().is_empty())
                .unwrap_or(false);
            render_framed_pane(
                f,
                content_area,
                &title_text,
                Style::default().fg(ui().results_error),
                state,
                body,
                has_query,
            );
        }
        ResultsPane::Loading => {
            let frame = spinner_frame();
            let title_text = render_pos_title(state, "Result");
            let body = vec![Line::from(Span::styled(
                format!(" {} Running query…", frame),
                Style::default().fg(ui().results_loading),
            ))];
            render_framed_pane(
                f,
                content_area,
                &title_text,
                Style::default().fg(ui().results_loading),
                state,
                body,
                false,
            );
        }
        pane @ (ResultsPane::Cancelled | ResultsPane::Skipped) => {
            let title_text = render_pos_title(state, "Result");
            let cursor = state.active_result().map(|t| t.cursor);
            let mut st = Style::default().fg(ui().results_cancelled);
            if matches!(cursor, Some(ResultsCursor::MessageLine(_))) {
                st = st.bg(results_cursor_bg(focused));
            }
            let msg = if matches!(pane, ResultsPane::Cancelled) {
                " Query cancelled (Ctrl-C)"
            } else {
                " Skipped (previous query failed)"
            };
            let body = vec![Line::from(Span::styled(msg, st))];
            let has_query = state
                .active_result()
                .map(|t| !t.query.trim().is_empty())
                .unwrap_or(false);
            render_framed_pane(
                f,
                content_area,
                &title_text,
                Style::default().fg(ui().results_cancelled),
                state,
                body,
                has_query,
            );
        }
        ResultsPane::NonQuery {
            verb,
            rows_affected,
        } => {
            let title_text = render_pos_title(state, "Result");
            let cursor = state.active_result().map(|t| t.cursor);
            let body_text = non_query_summary(verb, *rows_affected);
            let title_style = Style::default().fg(ui().results_title_active);
            let mut st = Style::default().fg(ui().results_loading);
            if matches!(cursor, Some(ResultsCursor::MessageLine(_))) {
                st = st.bg(results_cursor_bg(focused));
            }
            let body = vec![Line::from(Span::styled(format!(" {body_text}"), st))];
            let has_query = state
                .active_result()
                .map(|t| !t.query.trim().is_empty())
                .unwrap_or(false);
            render_framed_pane(
                f,
                content_area,
                &title_text,
                title_style,
                state,
                body,
                has_query,
            );
        }
        ResultsPane::Empty => unreachable!(),
        _ => {}
    }

    // Paint the `/` prompt row last so it sits on top of whatever the
    // content renderer drew above.
    if let (Some(rect), Some(text)) = (prompt_area, search_text) {
        let style = Style::default().fg(ui().status_mode_normal);
        f.render_widget(Paragraph::new(format!("/{text}")).style(style), rect);
    }
}

/// Muted background for the currently-highlighted column in the results pane —
/// mirrors the editor's `cursor_line_bg` so focus feels consistent.
pub(crate) fn results_cursor_bg(focused: bool) -> Color {
    if focused {
        ui().results_col_active_bg
    } else {
        ui().results_col_inactive_bg
    }
}

/// Build the header + body `Line`s for a tabular grid. Used by both
/// the results pane and the LSP hover popup so their styling stays in
/// lock-step (cursor cell, column-wide bg, selection rectangle).
///
/// - `cursor_row` / `active_col` drive the cursor-cell + column-wide
///   backgrounds on body rows. Pass `None` for each if the cursor isn't
///   on the grid.
/// - `header_cursor_col` flips the header cell at that column to the
///   strong cursor bg (used when the results cursor is on the header
///   row; hover grids don't have a header cursor so pass `None`).
/// - `selection_bounds` is `(top, bot, left, right)` when a visual
///   selection is active. Rows inside it take the muted column bg.
/// - `body_skip` drops N leading body rows (row-scroll offset).
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_grid_lines(
    columns: &[String],
    rows: &[Vec<String>],
    col_widths: &[u16],
    cursor_row: Option<usize>,
    active_col: Option<usize>,
    header_cursor_col: Option<usize>,
    selection_bounds: Option<(usize, usize, usize, usize)>,
    body_skip: usize,
    header_style: Style,
    sep_style: Style,
    cursor_style: Style,
    selection_style: Style,
) -> (Line<'static>, Vec<Line<'static>>) {
    let col_count = columns.len();
    let mut header_spans: Vec<Span<'static>> = Vec::with_capacity(col_count * 2);
    for (i, c) in columns.iter().enumerate() {
        let w = col_widths.get(i).copied().unwrap_or(0) as usize;
        let inner = w.saturating_sub(1);
        let mut st = header_style;
        if header_cursor_col == Some(i) {
            st = st.patch(cursor_style);
        }
        header_spans.push(Span::styled(format!(" {:<inner$}", c, inner = inner), st));
        if i + 1 < col_count {
            header_spans.push(Span::styled("│".to_string(), sep_style));
        }
    }
    let header_line = Line::from(header_spans);

    let body_lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(body_skip)
        .map(|(row_idx, row)| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(col_count * 2);
            for i in 0..col_count {
                let w = col_widths.get(i).copied().unwrap_or(0) as usize;
                let inner = w.saturating_sub(1);
                let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                let text = format!(" {:<inner$}", cell, inner = inner);
                let is_cursor = cursor_row == Some(row_idx) && active_col == Some(i);
                let is_selected = selection_bounds
                    .is_some_and(|(t, b, l, rr)| row_idx >= t && row_idx <= b && i >= l && i <= rr);
                let style = if is_cursor {
                    Some(cursor_style)
                } else if is_selected {
                    Some(selection_style)
                } else {
                    None
                };
                if let Some(st) = style {
                    spans.push(Span::styled(text, st));
                } else {
                    spans.push(Span::raw(text));
                }
                if i + 1 < col_count {
                    spans.push(Span::styled("│".to_string(), sep_style));
                }
            }
            Line::from(spans)
        })
        .collect();

    (header_line, body_lines)
}

/// Slightly brighter bg used for the single cell (or header) the cursor
/// actually points at, sitting on top of the column-wide muted bg.
pub(crate) fn results_cursor_bg_strong(focused: bool) -> Color {
    if focused {
        ui().results_cursor_active_bg
    } else {
        ui().results_cursor_inactive_bg
    }
}

pub(crate) fn render_pos_title(state: &AppState, label: &str) -> String {
    if state.result_tabs.len() > 1 {
        format!(
            " {label} ({}/{})",
            state.active_result_tab + 1,
            state.result_tabs.len(),
        )
    } else {
        format!(" {label}")
    }
}

/// Draw the hr/title/hr/query/hr chrome shared by Error, Loading, and
/// Cancelled panes, then the caller-supplied body below. When `show_query`
/// is false the query row + its trailing separator are omitted.
pub(crate) fn render_framed_pane(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    title_style: Style,
    state: &AppState,
    body: Vec<Line<'static>>,
    show_query: bool,
) {
    let sep_style = Style::default().fg(ui().results_sep);
    let hr: String = "─".repeat(area.width as usize);
    let query_text = state
        .active_result()
        .map(|t| t.query.clone())
        .unwrap_or_default();

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // hr
        Constraint::Length(1), // title
        Constraint::Length(1), // hr
    ];
    if show_query {
        constraints.push(Constraint::Length(1)); // query
        constraints.push(Constraint::Length(1)); // hr
    }
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[0]);
    f.render_widget(
        Paragraph::new(title.to_string()).style(title_style),
        chunks[1],
    );
    f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[2]);
    let body_chunk = if show_query {
        let mut query_line = highlight_query_line(&query_text, state.active_dialect);
        let cursor = state.active_result().map(|t| t.cursor);
        if state.focus == Focus::Results && cursor == Some(ResultsCursor::Query) {
            let qbg = results_cursor_bg(state.focus == Focus::Results);
            query_line = Line::from(
                query_line
                    .spans
                    .into_iter()
                    .map(|s| {
                        let st = s.style.bg(qbg);
                        Span::styled(s.content, st)
                    })
                    .collect::<Vec<_>>(),
            );
        }
        f.render_widget(Paragraph::new(query_line), chunks[3]);
        f.render_widget(Paragraph::new(hr).style(sep_style), chunks[4]);
        chunks[5]
    } else {
        chunks[3]
    };
    state
        .results_body_rows
        .store(body_chunk.height, std::sync::atomic::Ordering::Relaxed);
    state
        .results_body_width
        .store(body_chunk.width, std::sync::atomic::Ordering::Relaxed);
    let scroll_y = state.active_result().map(|t| t.scroll as u16).unwrap_or(0);
    f.render_widget(
        Paragraph::new(body)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .scroll((scroll_y, 0)),
        body_chunk,
    );
}

/// Render a 1-row tab bar above the results pane: numbered tabs with the active
/// one highlighted in cyan.
pub(crate) fn results_tab_bar(state: &AppState) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(state.result_tabs.len() * 2);
    for (i, tab) in state.result_tabs.iter().enumerate() {
        let is_err = matches!(tab.kind, ResultsPane::Error(_));
        let is_loading = matches!(tab.kind, ResultsPane::Loading);
        let is_cancelled = matches!(tab.kind, ResultsPane::Cancelled | ResultsPane::Skipped);
        let label = format!(" {} ", i + 1);
        let u = ui();
        let style = if i == state.active_result_tab {
            Style::default()
                .fg(u.tab_active_fg)
                .bg(if is_err {
                    u.tab_err_bg
                } else if is_loading {
                    u.tab_loading_bg
                } else if is_cancelled {
                    u.tab_cancel_bg
                } else {
                    u.tab_active_bg
                })
                .add_modifier(Modifier::BOLD)
        } else if is_err {
            Style::default().fg(u.tab_err_fg)
        } else if is_loading {
            Style::default().fg(u.tab_loading_fg)
        } else if is_cancelled {
            Style::default().fg(u.tab_cancel_fg)
        } else {
            Style::default().fg(u.results_header_active)
        };
        spans.push(Span::styled(label, style));
        if i + 1 < state.result_tabs.len() {
            spans.push(Span::styled("│", Style::default().fg(u.tab_sep_fg)));
        }
    }
    Line::from(spans)
}

/// Build a syntax-highlighted single-line Line for the results-pane query row.
/// Newlines in the source are collapsed to spaces. Byte offsets from the
/// highlighter refer to the original (multiline) source — we remap them onto
/// the flattened string so spans stay aligned.
/// Render `source` as syntax-highlighted lines. Spans crossing line breaks
/// are split per row. Shared tree-sitter parser kept in TLS (same pattern as
/// `highlight_query_line`).
pub(crate) fn highlight_sql_lines(source: &str, dialect: Dialect) -> Vec<Line<'static>> {
    use std::cell::RefCell;
    thread_local! {
        static HL: RefCell<Option<Highlighter>> = const { RefCell::new(None) };
    }

    let spans = HL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Highlighter::new_async());
        }
        if let Some(h) = slot.as_mut() {
            h.try_upgrade();
            h.highlight(source, dialect)
        } else {
            vec![]
        }
    });

    let bytes = source.as_bytes();
    let plain = Style::default().fg(ui().sql_plain);

    // Byte range of each line (without the trailing newline).
    let mut line_ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            line_ranges.push((start, i));
            start = i + 1;
        }
    }
    line_ranges.push((start, bytes.len()));

    line_ranges
        .iter()
        .map(|&(ls, le)| {
            let mut out: Vec<Span<'static>> = Vec::new();
            let mut cursor = ls;
            for s in &spans {
                let sb = s.start_byte.max(ls);
                let eb = s.end_byte.min(le);
                if sb >= eb {
                    continue;
                }
                if sb > cursor
                    && let Ok(raw) = std::str::from_utf8(&bytes[cursor..sb])
                {
                    out.push(Span::styled(raw.to_string(), plain));
                }
                if let Ok(raw) = std::str::from_utf8(&bytes[sb..eb]) {
                    let style = capture_style(s.capture.as_str()).unwrap_or(plain);
                    out.push(Span::styled(raw.to_string(), style));
                }
                cursor = eb;
            }
            if cursor < le
                && let Ok(raw) = std::str::from_utf8(&bytes[cursor..le])
            {
                out.push(Span::styled(raw.to_string(), plain));
            }
            if out.is_empty() {
                Line::from(Span::raw(""))
            } else {
                Line::from(out)
            }
        })
        .collect()
}

pub(crate) fn highlight_query_line(query: &str, dialect: Dialect) -> Line<'static> {
    use std::cell::RefCell;
    thread_local! {
        static HL: RefCell<Option<Highlighter>> = const { RefCell::new(None) };
    }

    if query.is_empty() {
        return Line::from(vec![Span::raw(" ")]);
    }

    let spans = HL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Highlighter::new_async());
        }
        if let Some(h) = slot.as_mut() {
            h.try_upgrade();
            h.highlight(query, dialect)
        } else {
            vec![]
        }
    });

    let bytes = query.as_bytes();
    let mut out: Vec<Span<'static>> = vec![Span::raw(" ")];
    let plain = Style::default().fg(ui().sql_plain);
    let mut cursor = 0usize;
    let flatten = |b: &[u8]| -> String {
        std::str::from_utf8(b)
            .unwrap_or("")
            .replace(['\n', '\r'], " ")
    };

    for s in &spans {
        if s.start_byte >= bytes.len() || s.end_byte > bytes.len() || s.start_byte >= s.end_byte {
            continue;
        }
        if s.start_byte > cursor {
            out.push(Span::styled(flatten(&bytes[cursor..s.start_byte]), plain));
        }
        let slice = flatten(&bytes[s.start_byte..s.end_byte]);
        let style = capture_style(s.capture.as_str()).unwrap_or(plain);
        out.push(Span::styled(slice, style));
        cursor = s.end_byte;
    }
    if cursor < bytes.len() {
        out.push(Span::styled(flatten(&bytes[cursor..]), plain));
    }
    Line::from(out)
}

/// Render a tabular hover payload as a centred, borderless, focus-
/// stealing dialog. Chrome matches the command palette — 2-col / 1-row
/// padding on `dialog_bg`, no border rule.
pub(crate) fn draw_hover_table(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &AppState,
    table: &sqeel_core::state::QueryResult,
    search_text: Option<&str>,
) {
    if area.width < 20 || area.height < 5 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);

    // Natural width: sum of col_widths + separator per column gap.
    let natural_w: u32 = table
        .col_widths
        .iter()
        .map(|&w| w as u32 + 1)
        .sum::<u32>()
        .saturating_sub(1);
    let popup_w = (natural_w as u16)
        .saturating_add(4) // 2-col pad each side
        .clamp(40, area.width.saturating_sub(4).min(100));
    // Body max = terminal height - vertical padding (2) - borders
    // space (none) - header/hr (2). Popup height = header + hr + body
    // + 1 top + 1 bottom pad.
    // Reserve one extra row when the `/` prompt is live.
    let prompt_rows: u16 = if search_text.is_some() { 1 } else { 0 };
    let max_body = area.height.saturating_sub(4 + prompt_rows);
    let body_h = (table.rows.len() as u16).min(max_body.max(1));
    let popup_h = (body_h + 4 + prompt_rows).min(area.height.saturating_sub(2));

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    // No title row — the popup is chromeless apart from `dialog_bg`.
    // Layout: header (1) + separator (1) + body (rest) + optional
    // `/` prompt (1) pinned to the bottom.
    let mut constraints = vec![
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ];
    if prompt_rows > 0 {
        constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let sep_style = Style::default().fg(u.results_sep).bg(u.dialog_bg);
    let header_style = bg.add_modifier(Modifier::BOLD).fg(u.results_header_active);
    // Highlights for the hover popup: results-pane bg colors read as
    // identical to `dialog_bg` (both are dim neutral shades), so
    // rolling `REVERSED` in inverts bg/fg of whatever cell the cursor
    // or selection covers. Unconditionally visible regardless of the
    // active theme.
    let cursor_style = bg.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let selection_style = Style::default()
        .fg(u.editor_search_fg)
        .bg(u.editor_search_bg);

    let cursor_row = match state.hover_cursor {
        ResultsCursor::Cell { row, .. } => Some(row),
        _ => None,
    };
    let active_col = match state.hover_cursor {
        ResultsCursor::Cell { col, .. } => Some(col),
        _ => None,
    };
    let selection_bounds: Option<(usize, usize, usize, usize)> =
        state.hover_selection.and_then(|sel| {
            let (ar, ac) = sel.anchor;
            let (cr, cc) = match state.hover_cursor {
                ResultsCursor::Cell { row, col } => (row, col),
                _ => return None,
            };
            let top = ar.min(cr);
            let bot = ar.max(cr);
            let (left, right) = match sel.mode {
                sqeel_core::state::ResultsSelectionMode::Line => {
                    (0, table.columns.len().saturating_sub(1))
                }
                sqeel_core::state::ResultsSelectionMode::Block => (ac.min(cc), ac.max(cc)),
            };
            Some((top, bot, left, right))
        });

    let (header_line, body_lines) = render_grid_lines(
        &table.columns,
        &table.rows,
        &table.col_widths,
        cursor_row,
        active_col,
        None,
        selection_bounds,
        state.hover_scroll,
        header_style,
        sep_style,
        cursor_style,
        selection_style,
    );

    let char_offset: u16 = table
        .col_widths
        .iter()
        .take(state.hover_col_scroll)
        .map(|&w| w as u32 + 1)
        .sum::<u32>() as u16;

    f.render_widget(
        Paragraph::new(header_line)
            .style(bg)
            .scroll((0, char_offset)),
        chunks[0],
    );
    let hr: String = "─".repeat(inner.width as usize);
    f.render_widget(Paragraph::new(hr).style(sep_style), chunks[1]);
    let body_rect = chunks[2];
    // Publish the body rect so nav helpers can clamp the scroll
    // offsets and the mouse-click handler can translate terminal-
    // space coordinates into grid (row, col). Without this `l` past
    // the viewport leaves the cursor off-screen, and clicks inside
    // the popup can't hit their cell.
    use std::sync::atomic::Ordering;
    state.hover_body_x.store(body_rect.x, Ordering::Relaxed);
    state.hover_body_y.store(body_rect.y, Ordering::Relaxed);
    state
        .hover_body_height
        .store(body_rect.height, Ordering::Relaxed);
    state
        .hover_body_width
        .store(body_rect.width, Ordering::Relaxed);
    f.render_widget(
        Paragraph::new(body_lines)
            .style(bg)
            .scroll((0, char_offset)),
        body_rect,
    );

    // `/` prompt pinned to the bottom when active.
    if let (Some(text), Some(rect)) = (search_text, chunks.get(3).copied()) {
        let prompt_style = Style::default().fg(u.status_mode_normal).bg(u.dialog_bg);
        f.render_widget(Paragraph::new(format!("/{text}")).style(prompt_style), rect);
    }
}

/// Loading placeholder rendered while the LSP hover response is in
/// flight. Uses the same borderless `dialog_bg` chrome so the
/// popup's footprint is consistent once content arrives.
pub(crate) fn draw_hover_loading(f: &mut ratatui::Frame<'_>, area: Rect) {
    if area.width < 20 || area.height < 5 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    // Size from actual content. The hourglass glyph is wide in most
    // fonts, so use `UnicodeWidthStr` to measure display cells rather
    // than chars; char-count under-sized the popup and clipped the
    // trailing "(Esc to cancel)" hint.
    use unicode_width::UnicodeWidthStr;
    let label = format!("{} Loading hover…", spinner_frame());
    let hint = "  (Esc to cancel)";
    let content_w = (label.width() + hint.width()) as u16;
    let popup_w = (content_w + 4).min(area.width.saturating_sub(4));
    let popup_h = 3u16.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(label, bg.add_modifier(Modifier::BOLD)),
            Span::styled(hint, bg),
        ]))
        .style(bg),
        inner,
    );
}

/// One-line signature-help bar rendered above the cursor in the editor
/// pane. Uses `dialog_bg` chrome so it's visually distinct from the
/// text; truncated to the available editor width.
pub(crate) fn draw_sig_help_bar(
    f: &mut ratatui::Frame<'_>,
    editor_area: Rect,
    screen_row: usize,
    screen_col: usize,
    text: &str,
) {
    if editor_area.height < 3 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    // Position: the row above the cursor; fall back to the row below
    // when the cursor is on the first visible line (no room above).
    let bar_y = if screen_row > 0 {
        editor_area.y + screen_row as u16 - 1
    } else {
        editor_area.y + screen_row as u16 + 1
    };
    if bar_y >= editor_area.y + editor_area.height {
        return;
    }
    // Anchor x at the cursor column but keep it inside the editor area.
    let bar_x = (editor_area.x + screen_col as u16).min(
        editor_area
            .x
            .saturating_add(editor_area.width.saturating_sub(1)),
    );
    let max_w = editor_area
        .width
        .saturating_sub(bar_x.saturating_sub(editor_area.x));
    if max_w == 0 {
        return;
    }
    let label = format!(" {text} ");
    let display: String = label.chars().take(max_w as usize).collect();
    let bar = Rect {
        x: bar_x,
        y: bar_y,
        width: display.chars().count() as u16,
        height: 1,
    };
    f.render_widget(Clear, bar);
    f.render_widget(
        Paragraph::new(Span::styled(display, bg.add_modifier(Modifier::BOLD))),
        bar,
    );
}

/// Plain-text (or lightly-markdown) hover payload. Same borderless
/// dialog_bg chrome as the table form; supports j/k scroll via the
/// passed-in `scroll` offset.
pub(crate) fn draw_hover_popup(f: &mut ratatui::Frame<'_>, area: Rect, scroll: usize, text: &str) {
    if area.width < 10 || area.height < 5 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    let lines = format_hover_lines(text);
    let longest = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>() as u16
        })
        .max()
        .unwrap_or(0);
    let popup_w = (longest + 4).clamp(30, area.width.saturating_sub(4).min(80));
    let content_h = (lines.len() as u16).min(area.height.saturating_sub(4));
    // popup = content + 2 rows of vertical padding.
    let popup_h = (content_h + 2).min(area.height.saturating_sub(2));

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    // Chromeless — no title row, content takes the whole inner area.
    f.render_widget(
        Paragraph::new(lines)
            .style(bg)
            .scroll((scroll as u16, 0))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

/// Render an LSP hover payload (markdown or plain text) as styled
/// ratatui lines via pulldown-cmark. Handles headers, fenced code
/// blocks, inline code, bold, italic, and blockquotes; everything else
/// flattens to plain spans so the popup stays readable for arbitrary
/// server output.
pub(crate) fn format_hover_lines(text: &str) -> Vec<Line<'static>> {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
    let u = ui();
    let header1 = Style::default()
        .fg(u.dialog_border)
        .add_modifier(Modifier::BOLD);
    let header2 = Style::default()
        .fg(u.sql_keyword)
        .add_modifier(Modifier::BOLD);
    let code_style = Style::default().fg(u.sql_ident);
    let bold_style = Style::default().add_modifier(Modifier::BOLD);
    let italic_style = Style::default().add_modifier(Modifier::ITALIC);
    let quote_style = Style::default()
        .fg(u.sql_comment)
        .add_modifier(Modifier::ITALIC);

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(text, opts);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    // Active style overlay stack. Headings / blockquotes push a base
    // style that applies to every text span emitted until the matching
    // End event pops it; `bold` / `emph` / `code` layer on top.
    let mut base_stack: Vec<Style> = Vec::new();
    let mut span_stack: Vec<Style> = Vec::new();
    let mut in_code_block = false;

    let flush_line = |current: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>| {
        out.push(Line::from(std::mem::take(current)));
    };
    let active_style = |base_stack: &[Style], span_stack: &[Style]| -> Style {
        // Right-most wins so inner emphasis overrides outer heading
        // base. Patch each layer so partial styles (fg only, modifier
        // only) compose instead of overwriting.
        let mut acc = Style::default();
        for s in base_stack {
            acc = acc.patch(*s);
        }
        for s in span_stack {
            acc = acc.patch(*s);
        }
        acc
    };
    let push_text = |text: &str,
                     style: Style,
                     current: &mut Vec<Span<'static>>,
                     out: &mut Vec<Line<'static>>| {
        // Newlines inside a text event (rare) still need to split the
        // current line so wrapping works.
        let mut parts = text.split('\n').peekable();
        while let Some(chunk) = parts.next() {
            if !chunk.is_empty() {
                current.push(Span::styled(chunk.to_string(), style));
            }
            if parts.peek().is_some() {
                flush_line(current, out);
            }
        }
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                base_stack.push(match level {
                    HeadingLevel::H1 => header1,
                    _ => header2,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                base_stack.pop();
                flush_line(&mut current, &mut out);
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush_line(&mut current, &mut out);
                out.push(Line::from(""));
            }
            Event::Start(Tag::BlockQuote(_)) => base_stack.push(quote_style),
            Event::End(TagEnd::BlockQuote) => {
                base_stack.pop();
                flush_line(&mut current, &mut out);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                base_stack.push(code_style);
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                base_stack.pop();
            }
            Event::Start(Tag::Emphasis) => span_stack.push(italic_style),
            Event::End(TagEnd::Emphasis) => {
                span_stack.pop();
            }
            Event::Start(Tag::Strong) => span_stack.push(bold_style),
            Event::End(TagEnd::Strong) => {
                span_stack.pop();
            }
            Event::Text(t) => {
                let st = active_style(&base_stack, &span_stack);
                if in_code_block {
                    // Code blocks emit a single Text with embedded
                    // newlines; split them so each source line becomes
                    // its own visual row.
                    for (i, chunk) in t.split('\n').enumerate() {
                        if i > 0 {
                            flush_line(&mut current, &mut out);
                        }
                        if !chunk.is_empty() {
                            current.push(Span::styled(chunk.to_string(), st));
                        }
                    }
                } else {
                    push_text(&t, st, &mut current, &mut out);
                }
            }
            Event::Code(t) => {
                current.push(Span::styled(t.into_string(), code_style));
            }
            Event::SoftBreak => current.push(Span::raw(" ")),
            Event::HardBreak => flush_line(&mut current, &mut out),
            Event::Rule => {
                flush_line(&mut current, &mut out);
                out.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(u.results_sep),
                )));
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        flush_line(&mut current, &mut out);
    }
    // Strip trailing blank lines so the popup sizes tightly.
    while out
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.is_empty()))
    {
        out.pop();
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

pub(crate) fn draw_completions(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor_area: Rect,
    cur_row: usize,
    cur_col: usize,
) {
    let cursor = state.completion_cursor;
    let items: Vec<ListItem> = state
        .completions
        .iter()
        .map(|s| ListItem::new(s.as_str()))
        .collect();

    let longest = state
        .completions
        .iter()
        .map(|s| s.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let popup_w = (longest + 2)
        .clamp(20, 60)
        .min(editor_area.width.saturating_sub(2));
    let popup_h = (items.len() as u16 + 2).min(12);

    // inner editor area starts 1 cell in from the block border
    let inner_x = editor_area.x + 1;
    let inner_y = editor_area.y + 1;
    let inner_w = editor_area.width.saturating_sub(2);
    let inner_h = editor_area.height.saturating_sub(2);

    // cursor position in screen coords (row 0 = first visible line)
    let cx = inner_x.saturating_add(cur_col as u16);
    let cy = inner_y.saturating_add(cur_row as u16);

    // place popup one row below cursor; flip up if it would overflow bottom
    let popup_y = if cy + 2 + popup_h <= inner_y + inner_h {
        cy + 2
    } else {
        cy.saturating_sub(popup_h)
    };
    // clamp x so popup stays inside the editor
    let popup_x = cx.min((inner_x + inner_w).saturating_sub(popup_w));

    let popup = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h.min(inner_h),
    };

    // Borderless — match command palette / hover / help styling. A bg
    // fill + 1-col horizontal padding inside replaces the old border
    // frame so the overlay reads as one unified chrome.
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let list = List::new(items)
        .style(bg)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y,
        width: popup.width.saturating_sub(2),
        height: popup.height,
    };
    let mut list_state = ListState::default().with_selected(Some(cursor));
    f.render_stateful_widget(list, inner, &mut list_state);
}

/// Small borderless centered dialog: 2-col horizontal padding, 1-row vertical
/// padding, single line of input. Used by the command palette and rename.
/// Borderless centered single-line input dialog. The caller supplies the
/// prompt prefix (e.g. `> `, `: `) so cursor placement stays exact regardless
/// of glyph width. Returns the terminal-space cursor position so the caller
/// can place the real cursor.
pub(crate) fn draw_input_dialog(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    prefix: &str,
    input: &TextInput,
) -> (u16, u16) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let content = format!("{prefix}{}", input.text);
    let inner_w = (content.chars().count() as u16 + 1).max(20);
    let width = (inner_w + 4).min(area.width.saturating_sub(4));
    let height = 3u16.min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let line = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(Paragraph::new(content).style(bg), line);
    (
        line.x + prefix.chars().count() as u16 + input.cursor as u16,
        line.y,
    )
}

/// Borderless centered confirmation dialog with a single message line.
pub(crate) fn draw_confirm_dialog(f: &mut ratatui::Frame<'_>, area: Rect, message: &str) {
    let bg = Style::default()
        .fg(ui().dialog_error_fg)
        .bg(ui().dialog_error_bg);
    let inner_w = (message.chars().count() as u16).max(20);
    let width = (inner_w + 4).min(area.width.saturating_sub(4));
    let height = 3u16.min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let line = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(Paragraph::new(message.to_string()).style(bg), line);
}

/// Modal y/N prompt shown at startup when `sqls` is missing and
/// `lsp_auto_install = true`. Keyboard-only: `y`/`Y`/`Enter` → install,
/// `n`/`N`/`Esc` → dismiss. Mirrors the `draw_confirm_dialog` shape but
/// taller to accommodate the explanatory body text.
pub(crate) fn draw_sqls_prompt_modal(f: &mut ratatui::Frame<'_>, area: Rect) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    // Fixed body lines — longest is 53 chars.
    let body_lines = [
        "SQL LSP not found",
        "",
        "sqeel uses 'sqls' for completion and diagnostics.",
        "Install via hjkl-anvil now? (~30s, ~15MB)",
        "",
        "[y] yes, install   [n] no, skip this session",
    ];
    let inner_w: u16 = body_lines
        .iter()
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap_or(20);
    let width = (inner_w + 4).min(area.width.saturating_sub(4));
    // 1 top pad + N body rows + 1 bottom pad
    let height = (body_lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);
    for (i, line) in body_lines.iter().enumerate() {
        let row = Rect {
            x: inner_x,
            y: popup.y + 1 + i as u16,
            width: inner_w,
            height: 1,
        };
        let style = if i == 0 {
            // Title row: bold
            bg.add_modifier(Modifier::BOLD)
        } else if i == body_lines.len() - 1 {
            // Key hint row: dim
            bg.add_modifier(Modifier::DIM)
        } else {
            bg
        };
        f.render_widget(Paragraph::new(line.to_string()).style(style), row);
    }
}

/// File picker dialog: borderless, padded, with input row + matching tab names.
///
/// Drives `hjkl_picker::Picker` — refresh runs here so the visible list
/// stays in sync with the streaming `FileSource` background scan. The
/// `active_name` overlay marks the currently-loaded tab with `* `.
pub(crate) fn draw_file_picker(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    picker: &mut hjkl_picker::Picker,
    active_name: Option<&str>,
) -> (u16, u16) {
    picker.tick(Instant::now());
    picker.refresh();

    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let width = 60u16.min(area.width.saturating_sub(4));
    let max_rows = 12u16;
    let entries = picker.visible_entries();
    let list_rows = (entries.len() as u16).min(max_rows).max(1);
    // 1 row top pad + 1 row input + 1 row separator + N rows + 1 row bottom pad
    let height = (list_rows + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);

    // Input row
    let query_text = picker.query.text();
    let display: String = query_text.lines().next().unwrap_or("").to_string();
    let input_area = Rect {
        x: inner_x,
        y: popup.y + 1,
        width: inner_w,
        height: 1,
    };
    f.render_widget(Paragraph::new(format!("> {display}")).style(bg), input_area);
    let (_, ccol) = picker.query.cursor();
    let cursor_pos = (input_area.x + 2 + ccol as u16, input_area.y);

    // Results list
    let list_y = popup.y + 3;
    let match_style = bg.fg(ui().editor_search_bg).add_modifier(Modifier::BOLD);
    let cursor = picker.selected.min(entries.len().saturating_sub(1));
    for (i, (label, matches)) in entries.iter().take(list_rows as usize).enumerate() {
        let row = Rect {
            x: inner_x,
            y: list_y + i as u16,
            width: inner_w,
            height: 1,
        };
        let is_cursor = i == cursor;
        // FileSource labels are `"  <relpath>"` (two-cell prefix). Strip
        // it so the active-tab marker `"* "` slots in cleanly without
        // doubling the indent.
        let bare = label.strip_prefix("  ").unwrap_or(label.as_str());
        let is_active = active_name == Some(bare);
        let marker = if is_active { "* " } else { "  " };
        let row_style = if is_cursor {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let mut spans: Vec<Span> = vec![Span::styled(marker.to_string(), row_style)];
        // matches positions index into the original label (with its 2-space
        // prefix). Adjust by -2 so highlights land on `bare`.
        for (ci, ch) in bare.chars().enumerate() {
            let orig_idx = ci + 2;
            if matches.contains(&orig_idx) {
                spans.push(Span::styled(ch.to_string(), match_style));
            } else {
                spans.push(Span::styled(ch.to_string(), row_style));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(row_style), row);
    }
    if entries.is_empty() {
        let row = Rect {
            x: inner_x,
            y: list_y,
            width: inner_w,
            height: 1,
        };
        f.render_widget(
            Paragraph::new("(no matches)").style(bg.add_modifier(Modifier::DIM)),
            row,
        );
    }
    cursor_pos
}

pub(crate) fn draw_connection_switcher(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let conns = &state.available_connections;
    let cursor = state.connection_switcher_cursor;
    let active_name = state.active_connection.as_deref();

    let width = 60u16.min(area.width.saturating_sub(4));
    let max_rows = 12u16;
    let list_rows = (conns.len() as u16).min(max_rows).max(1);
    // 1 row top pad + 1 row header + 1 row separator + N rows + 1 row bottom pad
    let height = (list_rows + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);

    // Header row
    let header = Rect {
        x: inner_x,
        y: popup.y + 1,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new("Connections  (Enter connect · n new · e edit · d delete)")
            .style(bg.add_modifier(Modifier::DIM)),
        header,
    );

    // List
    let list_y = popup.y + 3;
    if conns.is_empty() {
        let row = Rect {
            x: inner_x,
            y: list_y,
            width: inner_w,
            height: 1,
        };
        f.render_widget(
            Paragraph::new("(no connections configured)").style(bg.add_modifier(Modifier::DIM)),
            row,
        );
        return;
    }
    let cur = cursor.min(conns.len().saturating_sub(1));
    for (i, c) in conns.iter().take(list_rows as usize).enumerate() {
        let row = Rect {
            x: inner_x,
            y: list_y + i as u16,
            width: inner_w,
            height: 1,
        };
        let is_cursor = i == cur;
        let is_active = active_name == Some(c.name.as_str());
        let mut row_style = bg;
        if is_cursor {
            row_style = row_style.add_modifier(Modifier::REVERSED);
        }

        // Badge glyph + color reflects the live state of the active connection.
        // Non-active connections have no badge (they were never attempted this
        // session, or were superseded by a later switch).
        let (badge_glyph, badge_color) = if is_active {
            if state.schema_connecting {
                // Handshake in flight.
                ("◌ ", Color::Yellow)
            } else if state.schema_connect_error.is_some() {
                // Last attempt failed.
                ("✗ ", Color::Red)
            } else {
                // Live connection.
                ("● ", Color::Green)
            }
        } else {
            ("  ", ui().dialog_fg)
        };

        // Active entry: bold the name so it stands out beyond the cursor
        // highlight (which is REVERSED and thus lost on non-focused rows).
        let name_style = if is_active {
            row_style.add_modifier(Modifier::BOLD)
        } else {
            row_style
        };

        let line = Line::from(vec![
            Span::styled(badge_glyph, row_style.fg(badge_color)),
            Span::styled(c.name.clone(), name_style),
            Span::styled(format!(" — {}", c.url), row_style),
        ]);
        f.render_widget(Paragraph::new(line).style(row_style), row);
    }
}

pub(crate) fn draw_pgpass_picker(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let entries = &state.pgpass_entries;
    let cursor = state.pgpass_picker_cursor;

    let width = 72u16.min(area.width.saturating_sub(4));
    let max_rows = 10u16;
    let list_rows = (entries.len() as u16).min(max_rows).max(1);
    // 1 row top pad + 1 row title + 1 row separator + N rows + 1 row hint + 1 row bottom pad
    let height = (list_rows + 5).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);

    // Title row
    let title_area = Rect {
        x: inner_x,
        y: popup.y + 1,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new("Use credential from ~/.pgpass?").style(bg.add_modifier(Modifier::DIM)),
        title_area,
    );

    // Entry list
    let list_y = popup.y + 3;
    let cur = cursor.min(entries.len().saturating_sub(1));
    for (i, e) in entries.iter().take(list_rows as usize).enumerate() {
        let row = Rect {
            x: inner_x,
            y: list_y + i as u16,
            width: inner_w,
            height: 1,
        };
        let is_cursor = i == cur;
        let row_style = if is_cursor {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let label = format!("{}:{}/{}  ({})", e.host, e.port, e.database, e.user);
        f.render_widget(Paragraph::new(label).style(row_style), row);
    }

    // Hint row at the bottom
    let hint_y = popup.y + height.saturating_sub(2);
    let hint_area = Rect {
        x: inner_x,
        y: hint_y,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new("j/k  ↵ select  Esc cancel").style(bg.add_modifier(Modifier::DIM)),
        hint_area,
    );
}

pub(crate) fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect, scroll: u16) -> u16 {
    const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        (
            "Global",
            &[
                ("?", "Open this help (normal mode)"),
                ("Ctrl+Enter", "Run statement under cursor"),
                ("Ctrl+Shift+Enter", "Run all statements in file"),
                (":q", "Quit"),
                ("Esc Esc", "Dismiss all toasts"),
            ],
        ),
        (
            "Leader (default Space — config: editor.leader_key)",
            &[
                ("<leader> c", "Connection switcher"),
                ("<leader> n", "New scratch tab"),
                ("<leader> r", "Rename current tab"),
                ("<leader> R", "Refresh schema cache"),
                ("<leader> d", "Delete current tab (confirm)"),
                ("<leader> <leader>", "Fuzzy file picker"),
                (
                    "<leader> <CR>",
                    "Run statement under cursor (tmux/SSH-friendly alt)",
                ),
                (
                    "<leader> <Tab>",
                    "Run all statements in file (tmux/SSH-friendly alt)",
                ),
            ],
        ),
        (
            "Pane Focus",
            &[
                ("Ctrl+H / click", "Focus schema"),
                ("Ctrl+L / click", "Focus editor"),
                ("Ctrl+J / click", "Focus results"),
                ("Ctrl+K / click", "Focus editor"),
            ],
        ),
        (
            "Explorer Pane",
            &[
                ("j / k", "Navigate up / down"),
                ("Enter / l", "Expand / collapse node"),
                ("<leader> R / :refreshschema", "Refresh schema cache"),
                (
                    ":describe <table>  /  :desc <table>",
                    "Show column schema for table",
                ),
            ],
        ),
        (
            "Results Pane",
            &[
                ("j / k", "Down / up (count prefix)"),
                ("h / l", "Left / right (count prefix)"),
                ("gg / G", "First / last row"),
                ("0 / $", "First / last column"),
                ("/", "Search cells"),
                ("n / N", "Next / previous match"),
                ("H / L", "Prev / next result tab"),
                ("V", "Visual-line select rows"),
                ("v / Ctrl+V", "Visual-block select rectangle"),
                ("y", "Yank selection / row"),
                ("Esc", "Clear selection"),
                ("Left click", "Copy column value"),
                ("Right click", "Copy full row"),
                ("Left click (error)", "Copy query or error text"),
            ],
        ),
        (
            "Tabs",
            &[
                ("<leader>n", "New scratch tab"),
                ("Shift+L", "Next tab"),
                ("Shift+H", "Prev tab"),
                ("<leader>r", "Rename current tab"),
                ("<leader>d", "Delete current tab"),
                ("<leader><leader>", "Fuzzy switch tab"),
                ("Click tab name", "Switch to tab"),
            ],
        ),
        (
            "Editor — Vim",
            &[
                ("i", "Insert mode"),
                ("Esc", "Normal mode"),
                ("v", "Visual mode"),
                ("K", "LSP hover under cursor"),
                ("Ctrl+P / Ctrl+N", "History prev / next"),
            ],
        ),
        (
            "Connection Switcher",
            &[
                ("j / k", "Navigate"),
                ("Enter", "Connect"),
                ("n", "New connection"),
                ("e", "Edit connection"),
                ("d", "Delete connection"),
                ("Esc", "Close"),
            ],
        ),
        (
            "Add / Edit Connection",
            &[
                ("Tab", "Switch Name / URL field"),
                ("Enter", "Save"),
                ("Esc", "Cancel"),
            ],
        ),
    ];

    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    // Borderless dialog styling: same 2-col / 1-row padding idiom as
    // the command palette and hover popups. Two extra rows reserved
    // for the title + trailing pad, so content height = total_rows.
    let width = 62.min(area.width.saturating_sub(4));
    let total_rows: u16 = SECTIONS
        .iter()
        .map(|(_, items)| items.len() as u16 + 2)
        .sum::<u16>()
        + 1;
    let height = (total_rows + 4).min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let title = Paragraph::new(Line::from(Span::styled(
        " Help",
        bg.add_modifier(Modifier::BOLD),
    )))
    .style(bg);
    f.render_widget(title, chunks[0]);

    let max_scroll = total_rows.saturating_sub(chunks[1].height);
    let scroll = scroll.min(max_scroll);

    let mut lines: Vec<ratatui::text::Line<'static>> = vec![];
    for (section, items) in SECTIONS {
        lines.push(ratatui::text::Line::from(Span::styled(
            format!(" {section}"),
            bg.add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in *items {
            let pad = 20usize.saturating_sub(key.len());
            lines.push(ratatui::text::Line::from(vec![
                Span::styled(
                    format!("  {key}"),
                    Style::default().fg(u.completion_key).bg(u.dialog_bg),
                ),
                Span::styled(" ".repeat(pad), bg),
                Span::styled((*desc).to_string(), bg),
            ]));
        }
        lines.push(ratatui::text::Line::raw(""));
    }

    f.render_widget(
        Paragraph::new(lines).style(bg).scroll((scroll, 0)),
        chunks[1],
    );
    max_scroll
}

pub(crate) fn draw_connect_error_popup(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    headline: &str,
    err: &str,
    name: Option<&str>,
) {
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    let title = match name {
        Some(n) => format!(" {headline} — {n}"),
        None => format!(" {headline}"),
    };
    let footer = "[r] retry  [Esc/Enter] close";

    // Wrap the body to ~75% of the screen width minus padding.
    let width = (area.width * 3 / 4)
        .clamp(40, 100)
        .min(area.width.saturating_sub(4));
    let inner_w = width.saturating_sub(4) as usize;
    let body_lines: Vec<String> = err
        .lines()
        .flat_map(|line| {
            if line.is_empty() {
                vec![String::new()]
            } else {
                line.chars()
                    .collect::<Vec<_>>()
                    .chunks(inner_w.max(1))
                    .map(|c| c.iter().collect::<String>())
                    .collect()
            }
        })
        .collect();
    let body_h = (body_lines.len() as u16).max(1);
    // 1 title + 1 blank + body + 1 blank + 1 footer + 2 vertical padding
    let height = (body_h + 6).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(body_lines.len() + 4);
    lines.push(Line::from(Span::styled(
        title,
        bg.add_modifier(Modifier::BOLD).fg(u.dialog_error_fg),
    )));
    lines.push(Line::raw(""));
    for l in body_lines {
        lines.push(Line::from(Span::styled(l, bg)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        footer,
        bg.add_modifier(Modifier::DIM),
    )));

    f.render_widget(Paragraph::new(lines).style(bg), inner);
}

/// Returns true when `url`'s scheme supports TLS (mysql/mariadb/postgres).
/// Mirrors the identical helper in sqeel-core so the TUI can branch on it
/// without taking a dependency on core internals.
pub(crate) fn tui_url_supports_tls(url: &str) -> bool {
    let scheme = url.split(':').next().unwrap_or("");
    matches!(scheme, "mysql" | "mariadb" | "postgres" | "postgresql")
}

pub(crate) fn draw_add_connection(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    area: Rect,
) -> (u16, u16) {
    // Match the connection switcher / file picker style: no border,
    // bg-filled block, padded inner content. Header + footer rows use
    // DIM; the focused field is REVERSED so it reads at a glance
    // without competing with the muted hint.
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let width = 64u16.min(area.width.saturating_sub(4));
    let has_error = state.add_connection_error.is_some();
    let show_tls = tui_url_supports_tls(&state.add_connection_url);
    // Layout (no TLS):
    //   pad + header + blank + name + url + password + blank +
    //   [error + blank when present] + hint + pad  → base height 9.
    // Layout (TLS):
    //   pad + header + blank + name + url + password +
    //   ca_cert + client_cert + client_key + verify_mode + blank +
    //   [error + blank when present] + hint + pad  → base height 13.
    // Error rows grow the popup by 2 (the row itself + a leading blank)
    // so the hint stays put when the error is absent.
    let extra = if has_error { 2 } else { 0 };
    let base_height: u16 = if show_tls { 13 } else { 9 };
    let height = (base_height + extra).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);
    let header_text = if state.edit_connection_original_name.is_some() {
        "Edit Connection  (Tab switch · Enter save · Esc cancel)"
    } else {
        "Add Connection  (Tab switch · Enter save · Esc cancel)"
    };
    f.render_widget(
        Paragraph::new(header_text).style(bg.add_modifier(Modifier::DIM)),
        Rect {
            x: inner_x,
            y: popup.y + 1,
            width: inner_w,
            height: 1,
        },
    );

    let name_label = "Name      ";
    let url_label = "URL       ";
    let pw_label = "Password  ";
    let label_w = name_label.chars().count() as u16;

    let focused = state.add_connection_field;
    let name_style = if focused == AddConnectionField::Name {
        bg.add_modifier(Modifier::REVERSED)
    } else {
        bg
    };
    let url_style = if focused == AddConnectionField::Url {
        bg.add_modifier(Modifier::REVERSED)
    } else {
        bg
    };
    let pw_style = if focused == AddConnectionField::Password {
        bg.add_modifier(Modifier::REVERSED)
    } else {
        bg
    };

    let name_row = Rect {
        x: inner_x,
        y: popup.y + 3,
        width: inner_w,
        height: 1,
    };
    let url_row = Rect {
        x: inner_x,
        y: popup.y + 4,
        width: inner_w,
        height: 1,
    };
    let pw_row = Rect {
        x: inner_x,
        y: popup.y + 5,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(format!("{name_label}{}", state.add_connection_name)).style(name_style),
        name_row,
    );
    f.render_widget(
        Paragraph::new(format!("{url_label}{}", state.add_connection_url)).style(url_style),
        url_row,
    );
    // Mask the password with `*` characters.
    let pw_masked: String = "*".repeat(state.add_connection_password.chars().count());
    f.render_widget(
        Paragraph::new(format!("{pw_label}{pw_masked}")).style(pw_style),
        pw_row,
    );

    // TLS rows: only shown when the URL scheme supports TLS.
    let (ca_cert_row, client_cert_row, client_key_row, verify_row) = if show_tls {
        let ca_cert_label = "CA Cert   ";
        let client_cert_label = "Clt Cert  ";
        let client_key_label = "Clt Key   ";
        let verify_label = "Verify    ";

        let ca_cert_row = Rect {
            x: inner_x,
            y: popup.y + 6,
            width: inner_w,
            height: 1,
        };
        let client_cert_row = Rect {
            x: inner_x,
            y: popup.y + 7,
            width: inner_w,
            height: 1,
        };
        let client_key_row = Rect {
            x: inner_x,
            y: popup.y + 8,
            width: inner_w,
            height: 1,
        };
        let verify_row = Rect {
            x: inner_x,
            y: popup.y + 9,
            width: inner_w,
            height: 1,
        };

        let ca_cert_style = if focused == AddConnectionField::CaCert {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let client_cert_style = if focused == AddConnectionField::ClientCert {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let client_key_style = if focused == AddConnectionField::ClientKey {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };

        f.render_widget(
            Paragraph::new(format!("{ca_cert_label}{}", state.add_connection_ca_cert))
                .style(ca_cert_style),
            ca_cert_row,
        );
        f.render_widget(
            Paragraph::new(format!(
                "{client_cert_label}{}",
                state.add_connection_client_cert
            ))
            .style(client_cert_style),
            client_cert_row,
        );
        f.render_widget(
            Paragraph::new(format!(
                "{client_key_label}{}",
                state.add_connection_client_key
            ))
            .style(client_key_style),
            client_key_row,
        );

        // VerifyMode row: two toggle chips — active one is REVERSED, inactive is DIM.
        let verify_focused = focused == AddConnectionField::VerifyMode;
        let is_full = state.add_connection_verify_mode == TlsVerifyMode::Full;
        let full_style = if is_full {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg.add_modifier(Modifier::DIM)
        };
        let skip_style = if !is_full {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg.add_modifier(Modifier::DIM)
        };
        let verify_label_style = if verify_focused {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let verify_line = Line::from(vec![
            Span::styled(verify_label, verify_label_style),
            Span::styled(" Full ", full_style),
            Span::styled("  ", bg),
            Span::styled(" Skip ", skip_style),
        ]);
        f.render_widget(Paragraph::new(verify_line), verify_row);

        (
            Some(ca_cert_row),
            Some(client_cert_row),
            Some(client_key_row),
            Some(verify_row),
        )
    } else {
        (None, None, None, None)
    };

    // Error row: offset by 4 when TLS rows are visible (rows 6-9 are TLS).
    let tls_offset: u16 = if show_tls { 4 } else { 0 };
    if let Some(err) = state.add_connection_error.as_deref() {
        let err_style = Style::default()
            .fg(ui().dialog_error_fg)
            .bg(ui().dialog_error_bg);
        // Truncate the message to one row — the popup already has
        // a fixed height. Long sqlx errors stay readable in the
        // status bar / popup details flow elsewhere.
        let max = inner_w as usize;
        let mut shown = err.replace('\n', " ");
        if shown.chars().count() > max {
            shown = shown
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
                + "…";
        }
        f.render_widget(
            Paragraph::new(shown).style(err_style),
            Rect {
                x: inner_x,
                y: popup.y + 7 + tls_offset,
                width: inner_w,
                height: 1,
            },
        );
    }
    let hint_y = popup.y + 7 + tls_offset + extra;
    let hint_text = if focused == AddConnectionField::VerifyMode {
        "Space/Enter toggle · Tab next field · Esc cancel"
    } else if focused == AddConnectionField::Url {
        "URL: mysql:// postgres:// sqlite: duckdb::memory: duckdb:/path"
    } else {
        "Name: letters/digits/-/_  ·  Password stored in OS keyring (blank = URL inline)"
    };
    f.render_widget(
        Paragraph::new(hint_text).style(bg.add_modifier(Modifier::DIM)),
        Rect {
            x: inner_x,
            y: hint_y,
            width: inner_w,
            height: 1,
        },
    );

    match focused {
        AddConnectionField::Name => (
            name_row.x + label_w + state.add_connection_name_cursor as u16,
            name_row.y,
        ),
        AddConnectionField::Url => (
            url_row.x + label_w + state.add_connection_url_cursor as u16,
            url_row.y,
        ),
        AddConnectionField::Password => (
            pw_row.x + label_w + state.add_connection_password_cursor as u16,
            pw_row.y,
        ),
        AddConnectionField::CaCert => {
            let row = ca_cert_row.unwrap_or(pw_row);
            (
                row.x + label_w + state.add_connection_ca_cert_cursor as u16,
                row.y,
            )
        }
        AddConnectionField::ClientCert => {
            let row = client_cert_row.unwrap_or(pw_row);
            (
                row.x + label_w + state.add_connection_client_cert_cursor as u16,
                row.y,
            )
        }
        AddConnectionField::ClientKey => {
            let row = client_key_row.unwrap_or(pw_row);
            (
                row.x + label_w + state.add_connection_client_key_cursor as u16,
                row.y,
            )
        }
        AddConnectionField::VerifyMode => {
            // Position cursor on the active chip: "Full" starts at label_w+1,
            // "Skip" starts at label_w+9 (label_w + " Full " (6) + "  " (2) + " "(1)).
            let row = verify_row.unwrap_or(pw_row);
            let chip_offset: u16 = if state.add_connection_verify_mode == TlsVerifyMode::Full {
                label_w + 1
            } else {
                label_w + 9
            };
            (row.x + chip_offset, row.y)
        }
    }
}

/// Pre-process a raw `:` command string, stripping out cursorline /
/// cursorcolumn tokens from any `:set …` call and updating the caller's
/// booleans in place.  Returns the residual command (possibly shortened).
///
/// hjkl-engine 0.3 does not expose cursorline / cursorcolumn in its
/// `Settings` struct, so sqeel-tui owns these two booleans and intercepts
/// them before forwarding the rest of the `:set` line to the engine.
///
/// Supported token forms (mirrors vim):
///   `cursorline` / `cul`        → enable
///   `nocursorline` / `nocul`    → disable
///   `cursorcolumn` / `cuc`      → enable
///   `nocursorcolumn` / `nocuc`  → disable
///
/// Query-only tokens (`cursorline?` / `cul?`) and bare `:set` (no args)
/// are left in place so the engine's Info handler can report them.
/// Result of pre-processing a `:set` command for cursorline / cursorcolumn.
///
/// `forward` is what the engine's ex dispatcher receives (tokens we
/// consumed are stripped). `info` is an optional status string surfaced
/// as an Info toast — used for the `?` query form.
pub(crate) struct CursorOptsResult<'a> {
    pub forward: std::borrow::Cow<'a, str>,
    pub info: Option<String>,
}
