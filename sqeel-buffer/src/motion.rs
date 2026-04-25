//! Vim-shaped cursor motions on top of [`crate::Buffer`].
//!
//! All motions clamp to the buffer's content; none of them wrap to
//! the previous / next line. `move_right_in_line` stops at the last
//! character; the operator-context variant `move_right_to_end`
//! allows one position past it so `dl` deletes the final char.
//! Vertical motions (`move_up` / `move_down`) honour `sticky_col`
//! so bouncing through a shorter row doesn't drag the cursor back
//! to col 0.

use crate::{Buffer, Position};

/// Returns the char count of `line` — the column you'd see when the
/// cursor is parked one past the end.
fn line_chars(line: &str) -> usize {
    line.chars().count()
}

/// Last valid column for normal-mode motions (`hjkl`, etc.).
/// Empty rows clamp at 0; otherwise it's `chars - 1`.
fn last_col(line: &str) -> usize {
    line_chars(line).saturating_sub(1)
}

impl Buffer {
    // ── Horizontal motions ──────────────────────────────────────

    /// `h` — clamps at column 0; never wraps to the previous line.
    pub fn move_left(&mut self, count: usize) {
        let cursor = self.cursor();
        let new_col = cursor.col.saturating_sub(count.max(1));
        self.set_cursor(Position::new(cursor.row, new_col));
        self.refresh_sticky_col_from_cursor();
    }

    /// `l` — clamps at the last char on the line. Operator
    /// callers wanting "one past end" use [`Buffer::move_right_to_end`].
    pub fn move_right_in_line(&mut self, count: usize) {
        let cursor = self.cursor();
        let line = self.line(cursor.row).unwrap_or("");
        let limit = last_col(line);
        let new_col = (cursor.col + count.max(1)).min(limit);
        self.set_cursor(Position::new(cursor.row, new_col));
        self.refresh_sticky_col_from_cursor();
    }

    /// Operator-context `l`: allowed past the last char so a range
    /// motion includes it. Clamps at `chars()` (one past end).
    pub fn move_right_to_end(&mut self, count: usize) {
        let cursor = self.cursor();
        let line = self.line(cursor.row).unwrap_or("");
        let limit = line_chars(line);
        let new_col = (cursor.col + count.max(1)).min(limit);
        self.set_cursor(Position::new(cursor.row, new_col));
        self.refresh_sticky_col_from_cursor();
    }

    /// `0` — first column of the current row.
    pub fn move_line_start(&mut self) {
        let row = self.cursor().row;
        self.set_cursor(Position::new(row, 0));
        self.refresh_sticky_col_from_cursor();
    }

    /// `^` — first non-blank column. On a blank line it lands on 0.
    pub fn move_first_non_blank(&mut self) {
        let row = self.cursor().row;
        let col = self
            .line(row)
            .unwrap_or("")
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0);
        self.set_cursor(Position::new(row, col));
        self.refresh_sticky_col_from_cursor();
    }

    /// `$` — last char on the row. Empty rows stay at column 0.
    pub fn move_line_end(&mut self) {
        let row = self.cursor().row;
        let col = last_col(self.line(row).unwrap_or(""));
        self.set_cursor(Position::new(row, col));
        self.refresh_sticky_col_from_cursor();
    }

    // ── Vertical motions ────────────────────────────────────────

    /// `k` — `count` rows up; sticky col preserved across short rows.
    pub fn move_up(&mut self, count: usize) {
        self.move_vertical(-(count.max(1) as isize));
    }

    /// `j` — `count` rows down; sticky col preserved across short rows.
    pub fn move_down(&mut self, count: usize) {
        self.move_vertical(count.max(1) as isize);
    }

    /// `gg` — first row, first non-blank.
    pub fn move_top(&mut self) {
        self.set_cursor(Position::new(0, 0));
        self.move_first_non_blank();
    }

    /// `G` — last row (or `count - 1` when `count > 0`), first non-blank.
    /// `count = 0` (the unprefixed form) jumps to the buffer's bottom.
    pub fn move_bottom(&mut self, count: usize) {
        let last = self.row_count().saturating_sub(1);
        let target = if count == 0 {
            last
        } else {
            (count - 1).min(last)
        };
        self.set_cursor(Position::new(target, 0));
        self.move_first_non_blank();
    }

    // ── Word motions ────────────────────────────────────────────

    /// `w` / `W` — start of next word. `big = true` treats every
    /// non-whitespace run as one word (vim's WORD).
    pub fn move_word_fwd(&mut self, big: bool, count: usize) {
        for _ in 0..count.max(1) {
            let from = self.cursor();
            if let Some(next) = next_word_start(self, from, big) {
                self.set_cursor(next);
            } else {
                break;
            }
        }
        self.refresh_sticky_col_from_cursor();
    }

    /// `b` / `B` — start of previous word.
    pub fn move_word_back(&mut self, big: bool, count: usize) {
        for _ in 0..count.max(1) {
            let from = self.cursor();
            if let Some(prev) = prev_word_start(self, from, big) {
                self.set_cursor(prev);
            } else {
                break;
            }
        }
        self.refresh_sticky_col_from_cursor();
    }

    /// `%` — jump to the matching bracket. Walks the buffer
    /// counting nesting depth so nested pairs resolve correctly.
    /// Returns `true` when the cursor moved.
    pub fn match_bracket(&mut self) -> bool {
        let cursor = self.cursor();
        let line = match self.line(cursor.row) {
            Some(l) => l,
            None => return false,
        };
        let ch = match line.chars().nth(cursor.col) {
            Some(c) => c,
            None => return false,
        };
        let (open, close, forward) = match ch {
            '(' => ('(', ')', true),
            ')' => ('(', ')', false),
            '[' => ('[', ']', true),
            ']' => ('[', ']', false),
            '{' => ('{', '}', true),
            '}' => ('{', '}', false),
            '<' => ('<', '>', true),
            '>' => ('<', '>', false),
            _ => return false,
        };
        let mut depth: i32 = 0;
        if forward {
            let mut r = cursor.row;
            let mut c = cursor.col;
            loop {
                let chars: Vec<char> = self.line(r).unwrap_or("").chars().collect();
                while c < chars.len() {
                    let here = chars[c];
                    if here == open {
                        depth += 1;
                    } else if here == close {
                        depth -= 1;
                        if depth == 0 {
                            self.set_cursor(Position::new(r, c));
                            self.refresh_sticky_col_from_cursor();
                            return true;
                        }
                    }
                    c += 1;
                }
                if r + 1 >= self.row_count() {
                    return false;
                }
                r += 1;
                c = 0;
            }
        } else {
            let mut r = cursor.row;
            let mut c = cursor.col as isize;
            loop {
                let chars: Vec<char> = self.line(r).unwrap_or("").chars().collect();
                while c >= 0 {
                    let here = chars[c as usize];
                    if here == close {
                        depth += 1;
                    } else if here == open {
                        depth -= 1;
                        if depth == 0 {
                            self.set_cursor(Position::new(r, c as usize));
                            self.refresh_sticky_col_from_cursor();
                            return true;
                        }
                    }
                    c -= 1;
                }
                if r == 0 {
                    return false;
                }
                r -= 1;
                c = self.line(r).unwrap_or("").chars().count() as isize - 1;
            }
        }
    }

    /// `f` / `F` / `t` / `T` — find `ch` on the current row.
    /// `forward = true` searches right of the cursor; `till = true`
    /// stops one cell short of the match (the `t`/`T` semantic).
    /// Returns `true` when the cursor moved.
    pub fn find_char_on_line(&mut self, ch: char, forward: bool, till: bool) -> bool {
        let cursor = self.cursor();
        let line = match self.line(cursor.row) {
            Some(l) => l,
            None => return false,
        };
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            return false;
        }
        let target_col = if forward {
            chars
                .iter()
                .enumerate()
                .skip(cursor.col + 1)
                .find(|(_, c)| **c == ch)
                .map(|(i, _)| if till { i.saturating_sub(1) } else { i })
        } else {
            (0..cursor.col)
                .rev()
                .find(|&i| chars[i] == ch)
                .map(|i| if till { i + 1 } else { i })
        };
        match target_col {
            Some(col) => {
                self.set_cursor(Position::new(cursor.row, col));
                self.refresh_sticky_col_from_cursor();
                true
            }
            None => false,
        }
    }

    /// `e` / `E` — end of current/next word.
    pub fn move_word_end(&mut self, big: bool, count: usize) {
        for _ in 0..count.max(1) {
            let from = self.cursor();
            if let Some(end) = next_word_end(self, from, big) {
                self.set_cursor(end);
            } else {
                break;
            }
        }
        self.refresh_sticky_col_from_cursor();
    }

    // ── Internals ──────────────────────────────────────────────

    fn move_vertical(&mut self, delta: isize) {
        let cursor = self.cursor();
        let want = self.sticky_col().unwrap_or(cursor.col);
        // Sticky col only bootstraps from the cursor on the first
        // vertical move; subsequent moves read it back so a short
        // row clamping us to col 3 doesn't lose the desired col 12.
        self.set_sticky_col(Some(want));
        let last_row = self.row_count().saturating_sub(1) as isize;
        let target_row = (cursor.row as isize + delta).clamp(0, last_row) as usize;
        let line = self.line(target_row).unwrap_or("");
        let max_col = last_col(line);
        let target_col = want.min(max_col);
        self.set_cursor(Position::new(target_row, target_col));
    }

    /// Horizontal motions resync the sticky col so the next
    /// `j` / `k` aims at the new char position.
    fn refresh_sticky_col_from_cursor(&mut self) {
        let col = self.cursor().col;
        self.set_sticky_col(Some(col));
    }
}

/// True if `c` qualifies as a word character (vim's small `w`).
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Classify a char into vim's three "word kinds" so transitions
/// between them can drive `w` / `b` / `e`. `Big = true` collapses
/// `Word` and `Punct` into one bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharKind {
    Word,
    Punct,
    Space,
}

fn char_kind(c: char, big: bool) -> CharKind {
    if c.is_whitespace() {
        CharKind::Space
    } else if big || is_word(c) {
        // `Big` collapses Word + Punct into a single non-space bucket
        // so `W` / `B` / `E` skip across punctuation runs.
        CharKind::Word
    } else {
        CharKind::Punct
    }
}

/// Step one position forward, wrapping into the next row.
fn step_forward(buf: &Buffer, pos: Position) -> Option<Position> {
    let line = buf.line(pos.row)?;
    let len = line_chars(line);
    if pos.col + 1 < len {
        return Some(Position::new(pos.row, pos.col + 1));
    }
    if pos.row + 1 < buf.row_count() {
        return Some(Position::new(pos.row + 1, 0));
    }
    None
}

/// Step one position back, wrapping into the previous row.
fn step_back(buf: &Buffer, pos: Position) -> Option<Position> {
    if pos.col > 0 {
        return Some(Position::new(pos.row, pos.col - 1));
    }
    if pos.row == 0 {
        return None;
    }
    let prev_row = pos.row - 1;
    let prev_len = line_chars(buf.line(prev_row).unwrap_or(""));
    Some(Position::new(prev_row, prev_len.saturating_sub(1)))
}

fn char_at(buf: &Buffer, pos: Position) -> Option<char> {
    buf.line(pos.row)?.chars().nth(pos.col)
}

fn next_word_start(buf: &Buffer, from: Position, big: bool) -> Option<Position> {
    let start_kind = char_at(buf, from).map(|c| char_kind(c, big));
    let mut cur = from;
    // Skip the rest of the current word kind. Vim treats line
    // breaks as whitespace separators for `w`, so a row crossing
    // implicitly ends the current word — break and let the
    // skip-space pass handle anything beyond.
    if let Some(kind) = start_kind {
        while char_at(buf, cur).map(|c| char_kind(c, big)) == Some(kind) {
            let prev_row = cur.row;
            match step_forward(buf, cur) {
                Some(next) => {
                    cur = next;
                    if next.row != prev_row {
                        break;
                    }
                }
                None => return Some(end_of_buffer(buf)),
            }
        }
    }
    // Skip whitespace runs (within row + across rows) to land on
    // the next non-space char.
    while char_at(buf, cur).map(|c| char_kind(c, big)) == Some(CharKind::Space) {
        match step_forward(buf, cur) {
            Some(next) => cur = next,
            None => return Some(end_of_buffer(buf)),
        }
    }
    Some(cur)
}

/// One past the last char of the last row — vim's "end of buffer"
/// for operator-context word motions, so `yw` at end-of-line yanks
/// up to and including the last char.
fn end_of_buffer(buf: &Buffer) -> Position {
    let last_row = buf.row_count().saturating_sub(1);
    let last_line = buf.line(last_row).unwrap_or("");
    Position::new(last_row, line_chars(last_line))
}

fn prev_word_start(buf: &Buffer, from: Position, big: bool) -> Option<Position> {
    let mut cur = step_back(buf, from)?;
    // Skip whitespace backwards.
    while char_at(buf, cur).map(|c| char_kind(c, big)) == Some(CharKind::Space) {
        cur = step_back(buf, cur)?;
    }
    let target_kind = char_at(buf, cur).map(|c| char_kind(c, big))?;
    // Walk back while the previous char is still the same kind.
    loop {
        let Some(prev) = step_back(buf, cur) else {
            return Some(cur);
        };
        if char_at(buf, prev).map(|c| char_kind(c, big)) == Some(target_kind) {
            cur = prev;
        } else {
            return Some(cur);
        }
    }
}

fn next_word_end(buf: &Buffer, from: Position, big: bool) -> Option<Position> {
    // Vim's `e` advances at least one cell, then walks forward
    // until the *next* char is a different kind (or eof).
    let mut cur = step_forward(buf, from)?;
    while char_at(buf, cur).map(|c| char_kind(c, big)) == Some(CharKind::Space) {
        cur = step_forward(buf, cur)?;
    }
    let kind = char_at(buf, cur).map(|c| char_kind(c, big))?;
    loop {
        let Some(next) = step_forward(buf, cur) else {
            return Some(cur);
        };
        if char_at(buf, next).map(|c| char_kind(c, big)) == Some(kind) {
            cur = next;
        } else {
            return Some(cur);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(b: &Buffer) -> Position {
        b.cursor()
    }

    #[test]
    fn move_left_clamps_at_zero() {
        let mut b = Buffer::from_str("abcd");
        b.move_right_in_line(3);
        assert_eq!(at(&b), Position::new(0, 3));
        b.move_left(10);
        assert_eq!(at(&b), Position::new(0, 0));
    }

    #[test]
    fn move_left_does_not_wrap_to_prev_row() {
        let mut b = Buffer::from_str("abc\ndef");
        b.move_down(1);
        assert_eq!(at(&b).row, 1);
        b.move_left(99);
        assert_eq!(at(&b), Position::new(1, 0));
    }

    #[test]
    fn move_right_in_line_stops_at_last_char() {
        let mut b = Buffer::from_str("abcd");
        b.move_right_in_line(99);
        assert_eq!(at(&b), Position::new(0, 3));
    }

    #[test]
    fn move_right_to_end_allows_one_past() {
        let mut b = Buffer::from_str("abcd");
        b.move_right_to_end(99);
        assert_eq!(at(&b), Position::new(0, 4));
    }

    #[test]
    fn move_line_start_end() {
        let mut b = Buffer::from_str("  hello");
        b.move_line_end();
        assert_eq!(at(&b), Position::new(0, 6));
        b.move_line_start();
        assert_eq!(at(&b), Position::new(0, 0));
        b.move_first_non_blank();
        assert_eq!(at(&b), Position::new(0, 2));
    }

    #[test]
    fn move_line_end_on_empty_row_stays_at_zero() {
        let mut b = Buffer::from_str("");
        b.move_line_end();
        assert_eq!(at(&b), Position::new(0, 0));
    }

    #[test]
    fn move_down_preserves_sticky_col_across_short_row() {
        let mut b = Buffer::from_str("hello world\nhi\nlong line again");
        b.move_right_in_line(7);
        assert_eq!(at(&b), Position::new(0, 7));
        b.move_down(1);
        assert_eq!(at(&b).row, 1);
        // Short row clamps to col 1 (last char of "hi").
        assert_eq!(at(&b).col, 1);
        b.move_down(1);
        // Sticky col 7 restored on the longer row.
        assert_eq!(at(&b), Position::new(2, 7));
    }

    #[test]
    fn move_top_lands_on_first_non_blank() {
        let mut b = Buffer::from_str("    indented\nrow2");
        b.move_down(1);
        b.move_top();
        assert_eq!(at(&b), Position::new(0, 4));
    }

    #[test]
    fn move_bottom_with_count_jumps_to_line() {
        let mut b = Buffer::from_str("a\n  b\nc\nd");
        b.move_bottom(2);
        assert_eq!(at(&b), Position::new(1, 2));
    }

    #[test]
    fn move_bottom_zero_jumps_to_last_row() {
        let mut b = Buffer::from_str("a\nb\nc");
        b.move_bottom(0);
        assert_eq!(at(&b), Position::new(2, 0));
    }

    #[test]
    fn move_word_fwd_skips_whitespace_runs() {
        let mut b = Buffer::from_str("foo bar  baz");
        b.move_word_fwd(false, 1);
        assert_eq!(at(&b), Position::new(0, 4));
        b.move_word_fwd(false, 1);
        assert_eq!(at(&b), Position::new(0, 9));
    }

    #[test]
    fn move_word_fwd_separates_word_from_punct_in_small_w() {
        let mut b = Buffer::from_str("foo.bar");
        b.move_word_fwd(false, 1);
        assert_eq!(at(&b), Position::new(0, 3));
        b.move_word_fwd(false, 1);
        assert_eq!(at(&b), Position::new(0, 4));
    }

    #[test]
    fn move_word_fwd_big_collapses_word_and_punct() {
        let mut b = Buffer::from_str("foo.bar baz");
        b.move_word_fwd(true, 1);
        assert_eq!(at(&b), Position::new(0, 8));
    }

    #[test]
    fn move_word_back_lands_on_word_start() {
        let mut b = Buffer::from_str("foo bar baz");
        b.move_line_end();
        assert_eq!(at(&b), Position::new(0, 10));
        b.move_word_back(false, 1);
        assert_eq!(at(&b), Position::new(0, 8));
        b.move_word_back(false, 2);
        assert_eq!(at(&b), Position::new(0, 0));
    }

    #[test]
    fn move_word_end_lands_on_last_char() {
        let mut b = Buffer::from_str("foo bar");
        b.move_word_end(false, 1);
        assert_eq!(at(&b), Position::new(0, 2));
        b.move_word_end(false, 1);
        assert_eq!(at(&b), Position::new(0, 6));
    }

    #[test]
    fn find_char_forward_lands_on_match() {
        let mut b = Buffer::from_str("foo,bar,baz");
        assert!(b.find_char_on_line(',', true, false));
        assert_eq!(at(&b), Position::new(0, 3));
        assert!(b.find_char_on_line(',', true, false));
        assert_eq!(at(&b), Position::new(0, 7));
    }

    #[test]
    fn find_char_till_stops_one_short() {
        let mut b = Buffer::from_str("foo,bar");
        assert!(b.find_char_on_line(',', true, true));
        assert_eq!(at(&b), Position::new(0, 2));
    }

    #[test]
    fn find_char_backward_lands_on_match() {
        let mut b = Buffer::from_str("foo,bar,baz");
        b.set_cursor(Position::new(0, 10));
        assert!(b.find_char_on_line(',', false, false));
        assert_eq!(at(&b), Position::new(0, 7));
    }

    #[test]
    fn find_char_no_match_returns_false() {
        let mut b = Buffer::from_str("hello");
        assert!(!b.find_char_on_line('z', true, false));
        assert_eq!(at(&b), Position::new(0, 0));
    }

    #[test]
    fn match_bracket_pairs_within_line() {
        let mut b = Buffer::from_str("if (x + y) {");
        b.set_cursor(Position::new(0, 3));
        assert!(b.match_bracket());
        assert_eq!(at(&b), Position::new(0, 9));
        assert!(b.match_bracket());
        assert_eq!(at(&b), Position::new(0, 3));
    }

    #[test]
    fn match_bracket_handles_nesting() {
        let mut b = Buffer::from_str("((x))");
        b.set_cursor(Position::new(0, 0));
        assert!(b.match_bracket());
        assert_eq!(at(&b), Position::new(0, 4));
    }

    #[test]
    fn match_bracket_crosses_lines() {
        let mut b = Buffer::from_str("{\n  x\n}");
        b.set_cursor(Position::new(0, 0));
        assert!(b.match_bracket());
        assert_eq!(at(&b), Position::new(2, 0));
    }

    #[test]
    fn match_bracket_returns_false_off_bracket() {
        let mut b = Buffer::from_str("hello");
        assert!(!b.match_bracket());
    }

    #[test]
    fn motion_count_zero_treated_as_one() {
        let mut b = Buffer::from_str("abcd");
        b.move_right_in_line(0);
        assert_eq!(at(&b), Position::new(0, 1));
    }
}
