//! Syntax-span plumbing: tree-sitter highlight spans + LSP diagnostic
//! underlines into the engine's styled-span table, plus small text helpers.

use super::*;

/// Combine the LSP diagnostics vector with tree-sitter-derived parse
/// errors into one list for the inline-underline overlay. Parse errors
/// are lifted to `ERROR` severity so they render with the same loud
/// styling as an LSP error — they're "why did my SQL not run" markers
/// either way.
pub(crate) fn merged_diagnostics(
    lsp: &[sqeel_core::lsp::Diagnostic],
    parse_errors: &[sqeel_core::highlight::ParseError],
) -> Vec<sqeel_core::lsp::Diagnostic> {
    let mut out: Vec<sqeel_core::lsp::Diagnostic> = lsp.to_vec();
    out.extend(parse_errors.iter().map(|e| sqeel_core::lsp::Diagnostic {
        line: e.start_row as u32,
        col: e.start_col as u32,
        end_line: e.end_row as u32,
        end_col: e.end_col as u32,
        message: e.message.clone(),
        severity: lsp_types::DiagnosticSeverity::ERROR,
    }));
    out
}

/// Decide whether the highlight worker needs a fresh submission.
///
/// Fires on:
/// - `content_changed` — user edited the buffer.
/// - `viewport_scrolled` — viewport moved far enough that the current
///   parse window no longer covers what's on screen.
/// - A dialect flip — the DB handshake is async, so the first parse at
///   startup runs under `Dialect::Generic`. Once the connection resolves
///   and sets the concrete dialect we need to re-parse so dialect-specific
///   keyword promotion (DESC / SHOW / PRAGMA / …) kicks in.
pub(crate) fn should_resubmit_highlight(
    content_changed: bool,
    viewport_scrolled: bool,
    current_dialect: Dialect,
    last_dialect: Dialect,
) -> bool {
    content_changed || viewport_scrolled || current_dialect != last_dialect
}

pub(crate) fn capture_style(capture: &str) -> Option<Style> {
    let u = ui();
    if sqeel_core::highlight::is_sql_keyword_capture(capture) {
        Some(
            Style::default()
                .fg(u.sql_keyword)
                .add_modifier(Modifier::BOLD),
        )
    } else if capture.starts_with("string") || capture.starts_with("literal") {
        Some(Style::default().fg(u.sql_string))
    } else if capture.starts_with("comment") {
        Some(
            Style::default()
                .fg(u.sql_comment)
                .add_modifier(Modifier::ITALIC),
        )
    } else if capture.starts_with("number")
        || capture.starts_with("integer")
        || capture.starts_with("float")
    {
        Some(Style::default().fg(u.sql_number))
    } else if capture.starts_with("operator") || capture == "punctuation.special" {
        Some(Style::default().fg(u.sql_operator))
    } else {
        None
    }
}

/// Splice a window of tree-sitter spans into the textarea's existing
/// per-row syntax span table.  Spans in the result are slice-local —
/// rows are rebased by `result.start_row` before being written.
///
/// Avoids the 700k-row `vec![Vec::new(); row_count]` allocation the old
/// `syntax_spans_by_row` paid on the main thread every time a highlight
/// result arrived: we `take` the existing outer `Vec`, mutate only the
/// rows inside the window, and put it back.
pub(crate) fn apply_window_spans<H: Host>(
    editor: &mut Editor<hjkl_buffer::Buffer, H>,
    result: &HighlightResult,
    buffer_rows: usize,
    diagnostics: &[sqeel_core::lsp::Diagnostic],
) {
    let mut by_row = std::mem::take(&mut editor.styled_spans);
    if by_row.len() < buffer_rows {
        by_row.resize_with(buffer_rows, Vec::new);
    }
    let window_start = result.start_row;
    let window_end = (window_start + result.row_count).min(buffer_rows);
    for row_spans in by_row.iter_mut().take(window_end).skip(window_start) {
        row_spans.clear();
    }
    // Materialize only the rows in the highlight window.
    let seed_start = window_start;
    let rope = editor.buffer().rope();
    let buffer_lines: Vec<String> = (seed_start..window_end.min(rope.len_lines()))
        .map(|r| hjkl_buffer::rope_line_str(&rope, r))
        .collect();
    // Lookup helper: absolute row → slice index, or `None` for rows
    // outside the materialized window.
    let line_at = |row: usize| -> Option<&str> {
        if row < seed_start {
            return None;
        }
        buffer_lines.get(row - seed_start).map(String::as_str)
    };
    let line_len_at = |row: usize| -> usize { line_at(row).map(str::len).unwrap_or(0) };
    for s in &result.spans {
        // Marker spans are handled in the dedicated overlay pass below.
        if s.capture.starts_with("comment.marker.") {
            continue;
        }
        let Some(style) = capture_style(s.capture.as_str()).map(style_from_ratatui) else {
            continue;
        };
        let sr = s.start_row + window_start;
        let er = s.end_row + window_start;
        if sr >= buffer_rows {
            continue;
        }
        if sr == er {
            if s.end_col > s.start_col {
                by_row[sr].push((s.start_col, s.end_col, style));
            }
        } else {
            by_row[sr].push((s.start_col, usize::MAX, style));
            for row_spans in by_row.iter_mut().take(er.min(buffer_rows)).skip(sr + 1) {
                row_spans.push((0, usize::MAX, style));
            }
            if er < buffer_rows && s.end_col > 0 {
                by_row[er].push((0, s.end_col, style));
            }
        }
    }
    // Apply comment marker overlays sourced from hjkl-bonsai's
    // CommentMarkerPass (injected into result.spans by sqeel-core).
    // Uses overlay_span to splice in marker styles on top of the comment
    // spans already written above.
    for s in &result.spans {
        if !s.capture.starts_with("comment.marker.") {
            continue;
        }
        let sr = s.start_row + window_start;
        let er = s.end_row + window_start;
        if sr >= buffer_rows {
            continue;
        }
        let Some(style) = marker_capture_style(s.capture.as_str()).map(style_from_ratatui) else {
            continue;
        };
        if sr == er {
            if s.end_col > s.start_col {
                overlay_span(&mut by_row[sr], s.start_col, s.end_col, style);
            }
        } else {
            overlay_span(&mut by_row[sr], s.start_col, usize::MAX, style);
            for row_spans in by_row.iter_mut().take(er.min(buffer_rows)).skip(sr + 1) {
                overlay_span(row_spans, 0, usize::MAX, style);
            }
            if er < buffer_rows && s.end_col > 0 {
                overlay_span(&mut by_row[er], 0, s.end_col, style);
            }
        }
    }
    // LSP diagnostic underlines. Applied last so the underline stacks
    // on top of the keyword / marker overlays; we preserve the existing
    // span's fg and just layer on an error-coloured underline.
    for d in diagnostics {
        apply_diagnostic_underline(&mut by_row, d, &line_len_at, buffer_rows);
    }
    // Sort each touched row so `line_spans` sees them in start-byte order.
    for row_spans in by_row.iter_mut().take(window_end).skip(window_start) {
        row_spans.sort_by_key(|&(s, _, _)| s);
    }
    editor.install_syntax_spans(by_row);
}

/// Layer an LSP diagnostic's error / warning underline onto `by_row`
/// at the diagnostic's range. Existing spans in the range are split
/// and their fg preserved — we only add the `UNDERLINED` modifier and
/// paint the underline colour with the diagnostic severity colour, so
/// keyword / marker colouring inside the range still renders.
pub(crate) fn apply_diagnostic_underline(
    by_row: &mut [Vec<(usize, usize, EngineStyle)>],
    d: &sqeel_core::lsp::Diagnostic,
    line_len: &impl Fn(usize) -> usize,
    buffer_rows: usize,
) {
    let u = ui();
    let color = match d.severity {
        lsp_types::DiagnosticSeverity::ERROR => u.status_diag_error,
        lsp_types::DiagnosticSeverity::WARNING => u.status_diag_warning,
        _ => return,
    };
    let start_row = d.line as usize;
    let end_row = d.end_line as usize;
    if start_row >= buffer_rows {
        return;
    }
    let stop = end_row.min(buffer_rows.saturating_sub(1));
    for (row, row_spans) in by_row.iter_mut().enumerate().take(stop + 1).skip(start_row) {
        let line_len = line_len(row);
        let start_col = if row == start_row { d.col as usize } else { 0 };
        let mut end_col = if row == end_row {
            d.end_col as usize
        } else {
            line_len
        };
        // Zero-width ranges (LSP sometimes emits those) need to
        // highlight *something* — fall back to `start_col..line_end`,
        // clamped to at least one cell.
        if end_col <= start_col {
            end_col = line_len.max(start_col + 1);
        }
        end_col = end_col.min(line_len.max(start_col + 1));
        if start_col >= end_col {
            continue;
        }
        merge_underline(row_spans, start_col, end_col, color);
    }
}

/// Split `row` at `[start, end)` boundaries, adding the `UNDERLINE`
/// attr to the overlap region of each existing span. Uncovered bytes
/// in `[start, end)` get a bare underline span using `color` as fg.
///
/// Engine-native styles carry no separate underline colour (hjkl 0.33
/// interns `hjkl_engine::Style`), so the diagnostic colour goes on the
/// fg — which the old ratatui path also did for visibility in terminals
/// without colored-underline support.
pub(crate) fn merge_underline(
    row: &mut Vec<(usize, usize, EngineStyle)>,
    start: usize,
    end: usize,
    color: Color,
) {
    let ecolor = style_from_ratatui(Style::default().fg(color)).fg;
    let mut out: Vec<(usize, usize, EngineStyle)> = Vec::with_capacity(row.len() + 4);
    let mut overlap_ranges: Vec<(usize, usize)> = Vec::new();
    for &(s, e, sty) in row.iter() {
        if e <= start || s >= end {
            out.push((s, e, sty));
            continue;
        }
        if s < start {
            out.push((s, start, sty));
        }
        let olap_s = s.max(start);
        let olap_e = e.min(end);
        // Replace the syntax fg with the diagnostic colour inside the
        // range so the underline reads loud against the editor bg even
        // in terminals without colored-underline support. The range is
        // small (usually one token) so losing syntax colour there is a
        // fair trade for unambiguous error visibility.
        let merged = EngineStyle {
            fg: ecolor,
            attrs: sty.attrs | hjkl_engine::types::Attrs::UNDERLINE,
            ..sty
        };
        out.push((olap_s, olap_e, merged));
        overlap_ranges.push((olap_s, olap_e));
        if e > end {
            out.push((end, e, sty));
        }
    }
    // Fill gaps in [start, end) uncovered by any existing span.
    overlap_ranges.sort_by_key(|&(s, _)| s);
    let bare = EngineStyle {
        fg: ecolor,
        attrs: hjkl_engine::types::Attrs::UNDERLINE,
        ..EngineStyle::default()
    };
    let mut cursor = start;
    for (s, e) in overlap_ranges {
        if s > cursor {
            out.push((cursor, s, bare));
        }
        cursor = cursor.max(e);
    }
    if cursor < end {
        out.push((cursor, end, bare));
    }
    out.sort_by_key(|&(s, _, _)| s);
    *row = out;
}

/// Map a `comment.marker.*` capture name to a ratatui Style.
///
/// Label captures (`comment.marker.todo` etc.) produce a bold badge with
/// `sql_marker_fg` on the marker bg — contrast-driven visibility, matching
/// hjkl-bonsai's canonical theme so the label stays readable on the cursor
/// row.
/// Tail captures (`comment.marker.tail.*`) produce an italic tint.
/// Returns `None` for unrecognised capture names.
pub(crate) fn marker_capture_style(capture: &str) -> Option<Style> {
    let u = ui();
    let is_tail = capture.contains(".tail.");
    let color = if capture.contains(".todo") {
        u.sql_marker_todo
    } else if capture.contains(".fixme") {
        u.sql_marker_fixme
    } else if capture.contains(".note") {
        u.sql_marker_note
    } else if capture.contains(".warn") {
        u.sql_marker_warn
    } else {
        return None;
    };
    if is_tail {
        Some(Style::default().fg(color).add_modifier(Modifier::ITALIC))
    } else {
        Some(
            Style::default()
                .fg(u.sql_marker_fg)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )
    }
}

/// Insert a marker span `[ms, me)` with `style` into `row`, trimming /
/// splitting any existing span that overlaps so the marker isn't masked
/// by an outer tree-sitter comment span.
pub(crate) fn overlay_span(
    row: &mut Vec<(usize, usize, EngineStyle)>,
    ms: usize,
    me: usize,
    style: EngineStyle,
) {
    let mut trimmed: Vec<(usize, usize, EngineStyle)> = Vec::with_capacity(row.len() + 2);
    for &(s, e, sty) in row.iter() {
        if e <= ms || s >= me {
            trimmed.push((s, e, sty));
        } else if s < ms && e > me {
            trimmed.push((s, ms, sty));
            trimmed.push((me, e, sty));
        } else if s < ms {
            trimmed.push((s, ms, sty));
        } else if e > me {
            trimmed.push((me, e, sty));
        }
        // else: span fully inside marker — drop it.
    }
    trimmed.push((ms, me, style));
    *row = trimmed;
}

/// Materialize the buffer's logical lines as owned `String`s.
///
/// `hjkl_buffer::Buffer` went rope-only in 0.33 (`Buffer::lines` deleted);
/// the line-slice helpers below (`word_prefix_at`, `row_col_to_byte`, …)
/// keep their `&[String]` shape and get fed through this adapter. O(n) per
/// call — fine for sqeel's SQL-scratch buffer sizes (the heavy pipeline is
/// already gated off above 2 MB).
pub(crate) fn buffer_lines(buffer: &hjkl_buffer::Buffer) -> Vec<String> {
    let rope = buffer.rope();
    (0..rope.len_lines())
        .map(|r| hjkl_buffer::rope_line_str(&rope, r))
        .collect()
}

/// `file://` URI for a sqeel tab's LSP document. One document per tab
/// (keyed by sanitized tab name) so diagnostics publishes can be matched
/// back to the document they describe. The path is virtual — nothing is
/// written there; sqls only needs a stable, distinct identity per doc.
pub(crate) fn tab_lsp_uri(name: &str) -> lsp_types::Uri {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let p = std::env::temp_dir().join(format!("sqeel-tab-{sanitized}.sql"));
    let p = p.to_string_lossy();
    let uri_str = if p.starts_with('/') {
        format!("file://{p}")
    } else {
        // Windows: C:\... → file:///C:/...
        format!("file:///{}", p.replace('\\', "/"))
    };
    uri_str
        .parse()
        .unwrap_or_else(|_| "file:///tmp/sqeel-scratch.sql".parse().unwrap())
}

/// Expand a leading `~/` (or bare `~`) to the user's home directory.
/// Non-tilde paths pass through untouched.
pub(crate) fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("~"));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    std::path::PathBuf::from(path)
}

/// Convert a `(row, col)` character position into a byte offset in the
/// joined source (`\n` between lines). Used to feed cursor position into
/// `completion_ctx::parse_context`, which operates on a single string.
pub(crate) fn row_col_to_byte(lines: &[String], row: usize, col: usize) -> usize {
    let mut offset = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if i == row {
            for (char_count, (b, _)) in line.char_indices().enumerate() {
                if char_count == col {
                    return offset + b;
                }
            }
            return offset + line.len();
        }
        offset += line.len() + 1; // `\n`
    }
    offset
}

/// Returns the word (alphanumeric + `_`) ending at `col` on `line`.
pub(crate) fn word_prefix_at(lines: &[String], row: usize, col: usize) -> String {
    let Some(line) = lines.get(row) else {
        return String::new();
    };
    let before = &line[..col.min(line.len())];
    before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// Full word spanning the cursor — extends both left and right of the
/// caret until it hits a non-identifier character. Used by `K` to
/// short-circuit the LSP hover when the word matches a table we've
/// already cached columns for.
pub(crate) fn word_at_cursor(lines: &[String], row: usize, col: usize) -> String {
    let Some(line) = lines.get(row) else {
        return String::new();
    };
    let chars: Vec<char> = line.chars().collect();
    let col = col.min(chars.len());
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    chars[start..end].iter().collect()
}
