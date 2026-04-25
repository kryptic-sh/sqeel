//! Ex-command parser + executor.
//!
//! Parses the text after a leading `:` in the command-line prompt and
//! returns an [`ExEffect`] describing what the caller should do. Only the
//! editor-local effects (substitute, goto-line, clear-highlight) are
//! applied in-place against `Editor`; quit / save / unknown are returned
//! to the caller so the TUI loop can run them.

use crate::editor::Editor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExEffect {
    /// Nothing happened (empty input or already-applied effect).
    None,
    /// Save the current buffer.
    Save,
    /// Quit (`:q`, `:q!`, `:wq`, `:x`).
    Quit { force: bool, save: bool },
    /// Unknown command — caller should surface as an error toast.
    Unknown(String),
    /// Substitution finished — report replacement count.
    Substituted { count: usize },
    /// A no-op response for successful commands that don't need a side
    /// effect but should not be reported as unknown (e.g. `:noh`).
    Ok,
    /// Surface an informational message.
    Info(String),
    /// Surface an error message (syntax error, bad pattern, …).
    Error(String),
}

/// Parse and execute `input` (without the leading `:`).
pub fn run(editor: &mut Editor<'_>, input: &str) -> ExEffect {
    let cmd = input.trim();
    if cmd.is_empty() {
        return ExEffect::None;
    }

    // Bare line number — jump there.
    if let Ok(line) = cmd.parse::<usize>() {
        editor.goto_line(line);
        return ExEffect::Ok;
    }

    // `:q`, `:q!`, `:w`, `:wq`, `:x`.
    match cmd {
        "q" => {
            return ExEffect::Quit {
                force: false,
                save: false,
            };
        }
        "q!" => {
            return ExEffect::Quit {
                force: true,
                save: false,
            };
        }
        "w" => return ExEffect::Save,
        "wq" | "x" => {
            return ExEffect::Quit {
                force: false,
                save: true,
            };
        }
        "noh" | "nohlsearch" => {
            // Clearing the pattern removes the highlight.
            editor.buffer_mut().set_search_pattern(None);
            return ExEffect::Ok;
        }
        "reg" | "registers" => return ExEffect::Info(format_registers(editor)),
        "marks" => return ExEffect::Info(format_marks(editor)),
        "undo" | "u" => {
            crate::vim::do_undo(editor);
            return ExEffect::Ok;
        }
        "redo" | "red" => {
            crate::vim::do_redo(editor);
            return ExEffect::Ok;
        }
        _ => {}
    }

    // `:sort[!][iun]` — sort the whole buffer. Flags follow the vim
    // convention: `!` reverses, `i` ignores case, `u` keeps the first
    // occurrence of each line (unique), `n` sorts numerically.
    if let Some(rest) = cmd.strip_prefix("sort").or_else(|| cmd.strip_prefix("sor")) {
        return apply_sort(editor, rest);
    }

    // `:g/pat/cmd` (and `:g!/pat/cmd` / `:v/pat/cmd` for the
    // inverse). Only `cmd = d` is supported today — the most useful
    // application in a SQL editor is "delete every line matching".
    if let Some((negate, rest)) = parse_global_prefix(cmd) {
        return apply_global(editor, rest, negate);
    }

    // `:s/...` or `:%s/...` substitute.
    let (scope, rest) = if let Some(rest) = cmd.strip_prefix("%s") {
        (SubScope::Whole, rest)
    } else if let Some(rest) = cmd.strip_prefix('s') {
        (SubScope::CurrentLine, rest)
    } else {
        return ExEffect::Unknown(cmd.to_string());
    };
    match parse_substitute_body(rest) {
        Ok(sub) => match apply_substitute(editor, scope, sub) {
            Ok(count) => ExEffect::Substituted { count },
            Err(e) => ExEffect::Error(e),
        },
        Err(e) => ExEffect::Error(e),
    }
}

/// Detect a `:g/pat/cmd`, `:g!/pat/cmd`, or `:v/pat/cmd` prefix.
/// Returns `(negate, body_after_prefix)` where `body_after_prefix`
/// still has the leading separator + pattern + cmd attached.
fn parse_global_prefix(cmd: &str) -> Option<(bool, &str)> {
    if let Some(rest) = cmd.strip_prefix("g!") {
        return Some((true, rest));
    }
    if let Some(rest) = cmd.strip_prefix('v') {
        return Some((true, rest));
    }
    if let Some(rest) = cmd.strip_prefix('g') {
        return Some((false, rest));
    }
    None
}

/// Run `:g/pat/d` (or its negated variants). Walks the buffer's
/// rows, collects matches, then drops them in reverse so row indices
/// stay valid through the cascade of deletes.
fn apply_global(editor: &mut Editor<'_>, body: &str, negate: bool) -> ExEffect {
    use sqeel_buffer::{Edit, MotionKind, Position};
    let mut chars = body.chars();
    let sep = match chars.next() {
        Some(c) => c,
        None => return ExEffect::Error("empty :g pattern".into()),
    };
    if sep.is_alphanumeric() || sep == '\\' {
        return ExEffect::Error("global needs a separator, e.g. :g/foo/d".into());
    }
    let rest: String = chars.collect();
    let parts = split_unescaped(&rest, sep);
    if parts.len() < 2 {
        return ExEffect::Error("global needs /pattern/cmd".into());
    }
    let pattern = unescape(&parts[0], sep);
    let cmd = parts[1].trim();
    if cmd != "d" {
        return ExEffect::Error(format!(":g supports only `d` today, got `{cmd}`"));
    }
    let regex = match regex::Regex::new(&pattern) {
        Ok(r) => r,
        Err(e) => return ExEffect::Error(format!("bad pattern: {e}")),
    };

    editor.push_undo();
    // Identify rows to drop (newest-first so multi-line drops don't
    // shift indices under us).
    let row_count = editor.buffer().row_count();
    let mut targets: Vec<usize> = Vec::new();
    for row in 0..row_count {
        let line = editor.buffer().line(row).unwrap_or("");
        let matches = regex.is_match(line);
        if matches != negate {
            targets.push(row);
        }
    }
    if targets.is_empty() {
        editor.undo_stack.pop();
        return ExEffect::Substituted { count: 0 };
    }
    let count = targets.len();
    for row in targets.iter().rev() {
        let row = *row;
        // Last row in a 1-row buffer can't be removed (Buffer keeps
        // the one-empty-row invariant); just clear it instead.
        if editor.buffer().row_count() == 1 {
            let line_chars = editor
                .buffer()
                .line(0)
                .map(|l| l.chars().count())
                .unwrap_or(0);
            if line_chars > 0 {
                editor.mutate_edit(Edit::DeleteRange {
                    start: Position::new(0, 0),
                    end: Position::new(0, line_chars),
                    kind: MotionKind::Char,
                });
            }
            continue;
        }
        editor.mutate_edit(Edit::DeleteRange {
            start: Position::new(row, 0),
            end: Position::new(row, 0),
            kind: MotionKind::Line,
        });
    }
    editor.mark_dirty_after_ex();
    ExEffect::Substituted { count }
}

/// `:sort[!][iun]` body — `flags` is whatever followed the command name
/// (e.g. `!u`, ` un`, `i`). Sorts the whole buffer in place.
fn apply_sort(editor: &mut Editor<'_>, flags: &str) -> ExEffect {
    let trimmed = flags.trim();
    let mut reverse = false;
    let mut unique = false;
    let mut numeric = false;
    let mut ignore_case = false;
    for c in trimmed.chars() {
        match c {
            '!' => reverse = true,
            'u' => unique = true,
            'n' => numeric = true,
            'i' => ignore_case = true,
            ' ' | '\t' => {}
            other => return ExEffect::Error(format!("bad :sort flag `{other}`")),
        }
    }

    let mut lines: Vec<String> = editor.buffer().lines().to_vec();
    if numeric {
        // Vim's `:sort n`: extract the first decimal integer (with
        // optional leading `-`) on each line; lines with no number sort
        // first, in original order.
        lines.sort_by_key(|l| extract_leading_number(l));
    } else if ignore_case {
        lines.sort_by_key(|s| s.to_lowercase());
    } else {
        lines.sort();
    }
    if reverse {
        lines.reverse();
    }
    if unique {
        let cmp_key = |s: &str| -> String {
            if ignore_case {
                s.to_lowercase()
            } else {
                s.to_string()
            }
        };
        let mut seen = std::collections::HashSet::new();
        lines.retain(|line| seen.insert(cmp_key(line)));
    }

    editor.push_undo();
    editor.restore(lines, (0, 0));
    editor.mark_dirty_after_ex();
    ExEffect::Ok
}

/// Parse the first signed decimal integer from `line` for `:sort n`.
/// Lines with no leading number sort as `i64::MIN` so they cluster at
/// the top, matching vim's behaviour.
fn extract_leading_number(line: &str) -> i64 {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_digit() && bytes[i] != b'-' {
        i += 1;
    }
    if i >= bytes.len() {
        return i64::MIN;
    }
    let mut j = i;
    if bytes[j] == b'-' {
        j += 1;
    }
    let start = j;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    if j == start {
        return i64::MIN;
    }
    line[i..j].parse().unwrap_or(i64::MIN)
}

/// `:reg` / `:registers` — tabular dump of every non-empty register slot.
fn format_registers(editor: &Editor<'_>) -> String {
    let r = editor.registers();
    let mut lines = vec!["--- Registers ---".to_string()];
    let mut push = |sel: &str, text: &str, linewise: bool| {
        if text.is_empty() {
            return;
        }
        let marker = if linewise { "L" } else { " " };
        lines.push(format!("{sel:<3} {marker} {}", display_register(text)));
    };
    push("\"\"", &r.unnamed.text, r.unnamed.linewise);
    push("\"0", &r.yank_zero.text, r.yank_zero.linewise);
    for (i, slot) in r.delete_ring.iter().enumerate() {
        let sel = format!("\"{}", i + 1);
        push(&sel, &slot.text, slot.linewise);
    }
    for (i, slot) in r.named.iter().enumerate() {
        let sel = format!("\"{}", (b'a' + i as u8) as char);
        push(&sel, &slot.text, slot.linewise);
    }
    if lines.len() == 1 {
        lines.push("(no registers set)".to_string());
    }
    lines.join("\n")
}

/// Escape control chars + truncate so a multi-line register fits a single row
/// of the toast table.
fn display_register(text: &str) -> String {
    let escaped: String = text
        .chars()
        .map(|c| match c {
            '\n' => "\\n".to_string(),
            '\t' => "\\t".to_string(),
            '\r' => "\\r".to_string(),
            c => c.to_string(),
        })
        .collect();
    const MAX: usize = 60;
    if escaped.chars().count() > MAX {
        let head: String = escaped.chars().take(MAX - 3).collect();
        format!("{head}...")
    } else {
        escaped
    }
}

/// `:marks` — list every set mark with `(line, col)`. Lines are 1-based to
/// match vim; cols are 0-based.
fn format_marks(editor: &Editor<'_>) -> String {
    let mut lines = vec!["--- Marks ---".to_string(), "mark  line  col".to_string()];
    let mut entries: Vec<(char, usize, usize)> = editor
        .vim
        .marks
        .iter()
        .map(|(c, (r, col))| (*c, *r, *col))
        .collect();
    entries.sort_by_key(|(c, _, _)| *c);
    for (c, r, col) in entries {
        lines.push(format!(" {c}    {:>4}  {col:>3}", r + 1));
    }
    if let Some((r, col)) = editor.vim.jump_back.last() {
        lines.push(format!(" '    {:>4}  {col:>3}", r + 1));
    }
    if let Some((r, col)) = editor.vim.last_edit_pos {
        lines.push(format!(" .    {:>4}  {col:>3}", r + 1));
    }
    if lines.len() == 2 {
        lines.push("(no marks set)".to_string());
    }
    lines.join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubScope {
    CurrentLine,
    Whole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Substitute {
    pattern: String,
    replacement: String,
    global: bool,
    case_insensitive: bool,
}

/// Parse the `/pat/repl/flags` tail of a substitute command. The leading
/// `s` or `%s` has already been stripped. The separator is the first
/// character after the optional scope (typically `/`), matching vim.
fn parse_substitute_body(body: &str) -> Result<Substitute, String> {
    let mut chars = body.chars();
    let sep = chars.next().ok_or_else(|| "empty substitute".to_string())?;
    if sep.is_alphanumeric() || sep == '\\' {
        return Err("substitute needs a separator, e.g. :s/foo/bar/".into());
    }
    let rest: String = chars.collect();
    let parts = split_unescaped(&rest, sep);
    if parts.len() < 2 {
        return Err("substitute needs /pattern/replacement/".into());
    }
    let pattern = unescape(&parts[0], sep);
    let replacement = unescape(&parts[1], sep);
    let flags = parts.get(2).cloned().unwrap_or_default();
    let mut global = false;
    let mut case_insensitive = false;
    for f in flags.chars() {
        match f {
            'g' => global = true,
            'i' => case_insensitive = true,
            'c' => {
                return Err("interactive substitution (c flag) is not supported".into());
            }
            other => return Err(format!("unknown substitute flag: {other}")),
        }
    }
    Ok(Substitute {
        pattern,
        replacement,
        global,
        case_insensitive,
    })
}

/// Split `s` by `sep`, treating `\<sep>` as a literal occurrence.
fn split_unescaped(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                // Preserve the escape so regex metachars survive; only
                // collapse an escaped separator into a literal separator.
                if next == sep {
                    cur.push(sep);
                    chars.next();
                } else {
                    cur.push('\\');
                    cur.push(next);
                    chars.next();
                }
            } else {
                cur.push('\\');
            }
        } else if c == sep {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

/// Remove our `\<sep>` → `<sep>` escape. Other `\x` sequences pass
/// through so regex escape syntax (`\b`, `\d`, …) still works.
fn unescape(s: &str, _sep: char) -> String {
    s.to_string()
}

fn apply_substitute(
    editor: &mut Editor<'_>,
    scope: SubScope,
    sub: Substitute,
) -> Result<usize, String> {
    let pattern = if sub.case_insensitive {
        format!("(?i){}", sub.pattern)
    } else {
        sub.pattern.clone()
    };
    let regex = regex::Regex::new(&pattern).map_err(|e| format!("bad pattern: {e}"))?;

    editor.push_undo();

    let (range_start, range_end) = match scope {
        SubScope::CurrentLine => {
            let r = editor.cursor().0;
            (r, r)
        }
        SubScope::Whole => (0, editor.buffer().lines().len().saturating_sub(1)),
    };

    let mut new_lines: Vec<String> = editor.buffer().lines().to_vec();
    let mut count = 0usize;
    let clamp = range_end.min(new_lines.len().saturating_sub(1));
    for line in new_lines[range_start..=clamp].iter_mut() {
        let (replaced, n) = regex_replace(&regex, line, &sub.replacement, sub.global);
        *line = replaced;
        count += n;
    }

    if count == 0 {
        // Undo the undo push so the user doesn't see an empty undo step.
        editor.undo_stack.pop();
        return Ok(0);
    }

    // Apply the new content. Yank survives across loads since it's
    // owned by Editor now (was previously held by the textarea).
    editor.buffer_mut().replace_all(&new_lines.join("\n"));
    editor
        .buffer_mut()
        .set_cursor(sqeel_buffer::Position::new(range_start, 0));
    editor.mark_dirty_after_ex();
    Ok(count)
}

/// Count-returning variant of `Regex::replace` / `replace_all`. The
/// replacement is first translated from vim's notation (`&`) to the
/// regex crate's (`$0`) so `$n` interpolation still runs.
fn regex_replace(
    regex: &regex::Regex,
    text: &str,
    replacement: &str,
    global: bool,
) -> (String, usize) {
    let matches = regex.find_iter(text).count();
    if matches == 0 {
        return (text.to_string(), 0);
    }
    let rep = expand_vim_replacement(replacement);
    let replaced = if global {
        regex.replace_all(text, rep.as_str()).into_owned()
    } else {
        regex.replace(text, rep.as_str()).into_owned()
    };
    let count = if global { matches } else { 1 };
    (replaced, count)
}

/// Translate vim-ish replacement placeholders to regex ones. For now only
/// `&` → the whole match; vim also supports `\0-\9` which the `regex`
/// crate already honours, so we leave those alone.
fn expand_vim_replacement(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                out.push('\\');
                out.push(next);
                chars.next();
            } else {
                out.push('\\');
            }
        } else if c == '&' {
            // `&` in vim replacement == whole match, same as `$0` for `regex`.
            out.push_str("$0");
        } else {
            out.push(c);
        }
    }
    out
}

impl<'a> Editor<'a> {
    /// Called by ex-command handlers after they rewrite the buffer.
    /// Ensures dirty tracking and undo bookkeeping stay consistent.
    fn mark_dirty_after_ex(&mut self) {
        self.mark_content_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeybindingMode;
    use crate::editor::Editor;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn new(content: &str) -> Editor<'static> {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(content);
        e
    }

    fn type_keys(e: &mut Editor<'_>, keys: &str) {
        for c in keys.chars() {
            let ev = match c {
                '\n' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                '\x1b' => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                ch => KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            };
            e.handle_key(ev);
        }
    }

    #[test]
    fn substitute_current_line() {
        let mut e = new("foo foo\nfoo foo");
        let effect = run(&mut e, "s/foo/bar/");
        assert_eq!(effect, ExEffect::Substituted { count: 1 });
        assert_eq!(e.buffer().lines()[0], "bar foo");
        assert_eq!(e.buffer().lines()[1], "foo foo");
    }

    #[test]
    fn substitute_current_line_global() {
        let mut e = new("foo foo\nfoo");
        run(&mut e, "s/foo/bar/g");
        assert_eq!(e.buffer().lines()[0], "bar bar");
        assert_eq!(e.buffer().lines()[1], "foo");
    }

    #[test]
    fn substitute_whole_buffer_global() {
        let mut e = new("foo\nfoo foo\nbar");
        let effect = run(&mut e, "%s/foo/xyz/g");
        assert_eq!(effect, ExEffect::Substituted { count: 3 });
        assert_eq!(e.buffer().lines()[0], "xyz");
        assert_eq!(e.buffer().lines()[1], "xyz xyz");
        assert_eq!(e.buffer().lines()[2], "bar");
    }

    #[test]
    fn substitute_zero_matches_reports_zero() {
        let mut e = new("hello");
        let effect = run(&mut e, "s/xyz/abc/");
        assert_eq!(effect, ExEffect::Substituted { count: 0 });
        assert_eq!(e.buffer().lines()[0], "hello");
    }

    #[test]
    fn substitute_respects_case_insensitive_flag() {
        let mut e = new("Foo");
        let effect = run(&mut e, "s/foo/bar/i");
        assert_eq!(effect, ExEffect::Substituted { count: 1 });
        assert_eq!(e.buffer().lines()[0], "bar");
    }

    #[test]
    fn substitute_accepts_alternate_separator() {
        let mut e = new("/usr/local/bin");
        run(&mut e, "s#/usr#/opt#");
        assert_eq!(e.buffer().lines()[0], "/opt/local/bin");
    }

    #[test]
    fn substitute_ampersand_in_replacement() {
        let mut e = new("foo");
        run(&mut e, "s/foo/[&]/");
        assert_eq!(e.buffer().lines()[0], "[foo]");
    }

    #[test]
    fn goto_line() {
        let mut e = new("a\nb\nc\nd");
        run(&mut e, "3");
        assert_eq!(e.cursor().0, 2);
    }

    #[test]
    fn quit_and_force_quit() {
        let mut e = new("");
        assert_eq!(
            run(&mut e, "q"),
            ExEffect::Quit {
                force: false,
                save: false
            }
        );
        assert_eq!(
            run(&mut e, "q!"),
            ExEffect::Quit {
                force: true,
                save: false
            }
        );
        assert_eq!(
            run(&mut e, "wq"),
            ExEffect::Quit {
                force: false,
                save: true
            }
        );
    }

    #[test]
    fn write_returns_save() {
        let mut e = new("");
        assert_eq!(run(&mut e, "w"), ExEffect::Save);
    }

    #[test]
    fn noh_is_ok() {
        let mut e = new("");
        assert_eq!(run(&mut e, "noh"), ExEffect::Ok);
    }

    #[test]
    fn registers_lists_unnamed_and_named() {
        let mut e = new("hello world");
        // `yw` populates `"` and `"0`; `"ayw` also fills `"a`.
        type_keys(&mut e, "yw");
        type_keys(&mut e, "\"ayw");
        let info = match run(&mut e, "reg") {
            ExEffect::Info(s) => s,
            other => panic!("expected Info, got {other:?}"),
        };
        assert!(info.starts_with("--- Registers ---"));
        assert!(info.contains("\"\""));
        assert!(info.contains("\"0"));
        assert!(info.contains("\"a"));
        // Alias resolves to same command.
        assert_eq!(run(&mut e, "registers"), ExEffect::Info(info));
    }

    #[test]
    fn registers_empty_state() {
        let mut e = new("hi");
        let info = match run(&mut e, "reg") {
            ExEffect::Info(s) => s,
            other => panic!("expected Info, got {other:?}"),
        };
        assert!(info.contains("(no registers set)"));
    }

    #[test]
    fn marks_lists_user_and_special() {
        let mut e = new("alpha\nbeta\ngamma");
        type_keys(&mut e, "ma");
        type_keys(&mut e, "jjmb");
        // `iX<Esc>` produces a last_edit_pos.
        type_keys(&mut e, "iX");
        let info = match run(&mut e, "marks") {
            ExEffect::Info(s) => s,
            other => panic!("expected Info, got {other:?}"),
        };
        assert!(info.starts_with("--- Marks ---"));
        assert!(info.contains(" a "));
        assert!(info.contains(" b "));
        assert!(info.contains(" . "));
    }

    #[test]
    fn undo_alias_reverses_last_change() {
        let mut e = new("hello");
        type_keys(&mut e, "Aworld\x1b");
        assert_eq!(e.buffer().lines()[0], "helloworld");
        assert_eq!(run(&mut e, "undo"), ExEffect::Ok);
        assert_eq!(e.buffer().lines()[0], "hello");
        // Short alias.
        type_keys(&mut e, "Awow\x1b");
        assert_eq!(e.buffer().lines()[0], "hellowow");
        assert_eq!(run(&mut e, "u"), ExEffect::Ok);
        assert_eq!(e.buffer().lines()[0], "hello");
    }

    #[test]
    fn redo_alias_reapplies_undone_change() {
        let mut e = new("hi");
        type_keys(&mut e, "Athere\x1b");
        assert_eq!(e.buffer().lines()[0], "hithere");
        run(&mut e, "undo");
        assert_eq!(e.buffer().lines()[0], "hi");
        assert_eq!(run(&mut e, "redo"), ExEffect::Ok);
        assert_eq!(e.buffer().lines()[0], "hithere");
        // Short alias.
        run(&mut e, "u");
        assert_eq!(run(&mut e, "red"), ExEffect::Ok);
        assert_eq!(e.buffer().lines()[0], "hithere");
    }

    #[test]
    fn marks_empty_state() {
        let mut e = new("hi");
        let info = match run(&mut e, "marks") {
            ExEffect::Info(s) => s,
            other => panic!("expected Info, got {other:?}"),
        };
        assert!(info.contains("(no marks set)"));
    }

    #[test]
    fn sort_alphabetical() {
        let mut e = new("banana\napple\ncherry");
        assert_eq!(run(&mut e, "sort"), ExEffect::Ok);
        assert_eq!(
            e.buffer().lines(),
            vec!["apple".to_string(), "banana".into(), "cherry".into()]
        );
    }

    #[test]
    fn sort_reverse_with_bang() {
        let mut e = new("apple\nbanana\ncherry");
        run(&mut e, "sort!");
        assert_eq!(
            e.buffer().lines(),
            vec!["cherry".to_string(), "banana".into(), "apple".into()]
        );
    }

    #[test]
    fn sort_unique() {
        let mut e = new("foo\nbar\nfoo\nbaz\nbar");
        run(&mut e, "sort u");
        assert_eq!(
            e.buffer().lines(),
            vec!["bar".to_string(), "baz".into(), "foo".into()]
        );
    }

    #[test]
    fn sort_numeric() {
        let mut e = new("10\n2\n100\n7");
        run(&mut e, "sort n");
        assert_eq!(
            e.buffer().lines(),
            vec!["2".to_string(), "7".into(), "10".into(), "100".into()]
        );
    }

    #[test]
    fn sort_ignore_case() {
        let mut e = new("Banana\napple\nCherry");
        run(&mut e, "sort i");
        assert_eq!(
            e.buffer().lines(),
            vec!["apple".to_string(), "Banana".into(), "Cherry".into()]
        );
    }

    #[test]
    fn sort_undo_restores_original_order() {
        let mut e = new("c\nb\na");
        run(&mut e, "sort");
        assert_eq!(e.buffer().lines()[0], "a");
        crate::vim::do_undo(&mut e);
        assert_eq!(
            e.buffer().lines(),
            vec!["c".to_string(), "b".into(), "a".into()]
        );
    }

    #[test]
    fn sort_rejects_unknown_flag() {
        let mut e = new("a\nb");
        match run(&mut e, "sortz") {
            ExEffect::Error(msg) => assert!(msg.contains("z")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_command() {
        let mut e = new("");
        match run(&mut e, "blargh") {
            ExEffect::Unknown(cmd) => assert_eq!(cmd, "blargh"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn bad_substitute_pattern() {
        let mut e = new("hi");
        match run(&mut e, "s/[unterminated/foo/") {
            ExEffect::Error(_) => {}
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn substitute_escaped_separator() {
        let mut e = new("a/b/c");
        let effect = run(&mut e, "s/\\//-/g");
        assert_eq!(effect, ExEffect::Substituted { count: 2 });
        assert_eq!(e.buffer().lines()[0], "a-b-c");
    }

    #[test]
    fn global_delete_drops_matching_rows() {
        let mut e = new("keep1\nDROP1\nkeep2\nDROP2\nkeep3");
        let effect = run(&mut e, "g/DROP/d");
        assert_eq!(effect, ExEffect::Substituted { count: 2 });
        assert_eq!(
            e.buffer().lines(),
            &[
                "keep1".to_string(),
                "keep2".to_string(),
                "keep3".to_string()
            ]
        );
    }

    #[test]
    fn global_negated_drops_non_matching_rows() {
        let mut e = new("keep1\nother\nkeep2");
        let effect = run(&mut e, "v/keep/d");
        assert_eq!(effect, ExEffect::Substituted { count: 1 });
        assert_eq!(
            e.buffer().lines(),
            &["keep1".to_string(), "keep2".to_string()]
        );
    }

    #[test]
    fn global_with_regex_pattern() {
        let mut e = new("foo bar\nbaz qux\nfoo baz\nbaz");
        // Drop lines starting with "foo".
        let effect = run(&mut e, r"g/^foo/d");
        assert_eq!(effect, ExEffect::Substituted { count: 2 });
        assert_eq!(
            e.buffer().lines(),
            &["baz qux".to_string(), "baz".to_string()]
        );
    }

    #[test]
    fn global_no_matches_reports_zero() {
        let mut e = new("hello\nworld");
        let effect = run(&mut e, "g/xyz/d");
        assert_eq!(effect, ExEffect::Substituted { count: 0 });
        assert_eq!(e.buffer().lines().len(), 2);
    }

    #[test]
    fn global_unsupported_command_errors_out() {
        let mut e = new("foo\nbar");
        let effect = run(&mut e, "g/foo/p");
        assert!(matches!(effect, ExEffect::Error(_)));
    }
}
