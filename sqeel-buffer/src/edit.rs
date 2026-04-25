//! Edit operations on [`crate::Buffer`].
//!
//! Every mutation goes through [`Buffer::apply_edit`] and returns
//! the inverse `Edit` so the host can build an undo stack without
//! snapshotting the whole buffer. Cursor follows edits the way vim
//! does: insertions land the cursor at the end of the inserted
//! text; deletions clamp the cursor to the deletion start.

use crate::{Buffer, Position};

/// Granularity of a delete; preserved through undo so a linewise
/// delete doesn't come back as a charwise one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    /// Charwise — `[start, end)` byte range, possibly wrapping rows.
    Char,
    /// Linewise — whole rows from `start.row..=end.row`. Endpoint
    /// columns are ignored.
    Line,
    /// Blockwise — rectangle `[start.row..=end.row] × [min_col..=max_col]`.
    Block,
}

/// One unit of buffer mutation. Constructed by the caller (vim
/// engine, ex command, …) and handed to [`Buffer::apply_edit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Insert one char at `at`. Cursor lands one position past it.
    InsertChar { at: Position, ch: char },
    /// Insert `text` (possibly multi-line) at `at`. Cursor lands at
    /// the end of the inserted content.
    InsertStr { at: Position, text: String },
    /// Delete `[start, end)` with the given kind.
    DeleteRange {
        start: Position,
        end: Position,
        kind: MotionKind,
    },
    /// `J` (`with_space = true`) / `gJ` (`false`) — fold `count` rows
    /// after `row` into `row`.
    JoinLines {
        row: usize,
        count: usize,
        with_space: bool,
    },
    /// Inverse of `JoinLines`. Splits `row` back at each char column
    /// in `cols`. `inserted_space` matches the original join so the
    /// inverse can drop the space before splitting.
    SplitLines {
        row: usize,
        cols: Vec<usize>,
        inserted_space: bool,
    },
    /// Replace `[start, end)` with `with` (charwise, may span rows).
    Replace {
        start: Position,
        end: Position,
        with: String,
    },
    /// Insert one chunk per row, each at `(at.row + i, at.col)`.
    /// Inverse of a blockwise delete; preserves the rectangle even
    /// when rows are ragged shorter than `at.col`.
    InsertBlock { at: Position, chunks: Vec<String> },
    /// Inverse of [`Edit::InsertBlock`]. Removes `widths[i]` chars
    /// starting at `(at.row + i, at.col)`. Carrying widths instead
    /// of recomputing means a ragged-row block delete round-trips
    /// exactly.
    DeleteBlockChunks { at: Position, widths: Vec<usize> },
}

impl Buffer {
    /// Apply `edit` and return the inverse. Pushing the inverse back
    /// through [`Buffer::apply_edit`] restores the previous state.
    pub fn apply_edit(&mut self, edit: Edit) -> Edit {
        match edit {
            Edit::InsertChar { at, ch } => self.do_insert_str(at, ch.to_string()),
            Edit::InsertStr { at, text } => self.do_insert_str(at, text),
            Edit::DeleteRange { start, end, kind } => self.do_delete_range(start, end, kind),
            Edit::JoinLines {
                row,
                count,
                with_space,
            } => self.do_join_lines(row, count, with_space),
            Edit::SplitLines {
                row,
                cols,
                inserted_space,
            } => self.do_split_lines(row, cols, inserted_space),
            Edit::Replace { start, end, with } => self.do_replace(start, end, with),
            Edit::InsertBlock { at, chunks } => self.do_insert_block(at, chunks),
            Edit::DeleteBlockChunks { at, widths } => self.do_delete_block_chunks(at, widths),
        }
    }

    fn do_insert_block(&mut self, at: Position, chunks: Vec<String>) -> Edit {
        let mut widths: Vec<usize> = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.into_iter().enumerate() {
            let row = at.row + i;
            // Pad short rows with spaces so the column position
            // exists before splicing.
            let line_chars = self.lines_mut()[row].chars().count();
            if line_chars < at.col {
                let pad = at.col - line_chars;
                self.lines_mut()[row].push_str(&" ".repeat(pad));
            }
            widths.push(chunk.chars().count());
            self.splice_at(Position::new(row, at.col), &chunk);
        }
        self.dirty_gen_bump();
        self.set_cursor(at);
        Edit::DeleteBlockChunks { at, widths }
    }

    fn do_delete_block_chunks(&mut self, at: Position, widths: Vec<usize>) -> Edit {
        let mut chunks: Vec<String> = Vec::with_capacity(widths.len());
        for (i, w) in widths.into_iter().enumerate() {
            let row = at.row + i;
            let removed =
                self.cut_chars(Position::new(row, at.col), Position::new(row, at.col + w));
            chunks.push(removed);
        }
        self.dirty_gen_bump();
        self.set_cursor(at);
        Edit::InsertBlock { at, chunks }
    }

    fn do_insert_str(&mut self, at: Position, text: String) -> Edit {
        let normalised = self.clamp_position(at);
        let inserted_chars = text.chars().count();
        let inserted_lines = text.split('\n').count();
        let end = if inserted_lines > 1 {
            let last_chars = text.rsplit('\n').next().unwrap_or("").chars().count();
            Position::new(normalised.row + inserted_lines - 1, last_chars)
        } else {
            Position::new(normalised.row, normalised.col + inserted_chars)
        };
        self.splice_at(normalised, &text);
        self.dirty_gen_bump();
        self.set_cursor(end);
        Edit::DeleteRange {
            start: normalised,
            end,
            kind: MotionKind::Char,
        }
    }

    fn do_delete_range(&mut self, start: Position, end: Position, kind: MotionKind) -> Edit {
        let (start, end) = order(start, end);
        match kind {
            MotionKind::Char => {
                let removed = self.cut_chars(start, end);
                self.dirty_gen_bump();
                self.set_cursor(start);
                Edit::InsertStr {
                    at: start,
                    text: removed,
                }
            }
            MotionKind::Line => {
                let lo = start.row;
                let hi = end.row.min(self.row_count().saturating_sub(1));
                let removed_lines: Vec<String> = self.lines_mut().drain(lo..=hi).collect();
                if self.lines_mut().is_empty() {
                    self.lines_mut().push(String::new());
                }
                self.dirty_gen_bump();
                let target_row = lo.min(self.row_count().saturating_sub(1));
                self.set_cursor(Position::new(target_row, 0));
                let mut text = removed_lines.join("\n");
                // Trailing `\n` so the inverse insert pushes the
                // surviving row(s) down rather than concatenating
                // onto whatever currently sits at `lo`.
                text.push('\n');
                Edit::InsertStr {
                    at: Position::new(lo, 0),
                    text,
                }
            }
            MotionKind::Block => {
                let (left, right) = (start.col.min(end.col), start.col.max(end.col));
                let mut chunks: Vec<String> = Vec::with_capacity(end.row - start.row + 1);
                for row in start.row..=end.row {
                    let row_left = Position::new(row, left);
                    let row_right = Position::new(row, right + 1);
                    let removed = self.cut_chars(row_left, row_right);
                    chunks.push(removed);
                }
                self.dirty_gen_bump();
                self.set_cursor(Position::new(start.row, left));
                // Inverse paired with [`Edit::InsertBlock`]: each
                // chunk lands back at its original column on its
                // row, preserving ragged-row content exactly.
                Edit::InsertBlock {
                    at: Position::new(start.row, left),
                    chunks,
                }
            }
        }
    }

    fn do_join_lines(&mut self, row: usize, count: usize, with_space: bool) -> Edit {
        let count = count.max(1);
        let row = row.min(self.row_count().saturating_sub(1));
        let mut split_cols: Vec<usize> = Vec::with_capacity(count);
        let mut joined = std::mem::take(&mut self.lines_mut()[row]);
        for _ in 0..count {
            if row + 1 >= self.row_count() {
                break;
            }
            let next = self.lines_mut().remove(row + 1);
            let join_col = joined.chars().count();
            split_cols.push(join_col);
            if with_space && !joined.is_empty() && !next.is_empty() {
                joined.push(' ');
            }
            joined.push_str(&next);
        }
        self.lines_mut()[row] = joined;
        self.dirty_gen_bump();
        self.set_cursor(Position::new(row, 0));
        Edit::SplitLines {
            row,
            cols: split_cols,
            inserted_space: with_space,
        }
    }

    fn do_split_lines(&mut self, row: usize, cols: Vec<usize>, inserted_space: bool) -> Edit {
        let row = row.min(self.row_count().saturating_sub(1));
        let mut working = std::mem::take(&mut self.lines_mut()[row]);
        // Split right-to-left so each `cols[i]` still indexes into
        // the original char positions on the surviving prefix.
        let mut tails: Vec<String> = Vec::with_capacity(cols.len());
        for &c in cols.iter().rev() {
            let byte = Position::new(0, c).byte_offset(&working);
            let mut tail = working.split_off(byte);
            if inserted_space && tail.starts_with(' ') {
                tail.remove(0);
            }
            tails.push(tail);
        }
        // Re-insert head + tails in document order.
        self.lines_mut()[row] = working;
        for (i, tail) in tails.into_iter().rev().enumerate() {
            self.lines_mut().insert(row + 1 + i, tail);
        }
        self.dirty_gen_bump();
        self.set_cursor(Position::new(row, 0));
        Edit::JoinLines {
            row,
            count: cols.len(),
            with_space: inserted_space,
        }
    }

    fn do_replace(&mut self, start: Position, end: Position, with: String) -> Edit {
        let (start, end) = order(start, end);
        let removed = self.cut_chars(start, end);
        let normalised = self.clamp_position(start);
        let inserted_chars = with.chars().count();
        let inserted_lines = with.split('\n').count();
        let new_end = if inserted_lines > 1 {
            let last_chars = with.rsplit('\n').next().unwrap_or("").chars().count();
            Position::new(normalised.row + inserted_lines - 1, last_chars)
        } else {
            Position::new(normalised.row, normalised.col + inserted_chars)
        };
        self.splice_at(normalised, &with);
        self.dirty_gen_bump();
        self.set_cursor(new_end);
        Edit::Replace {
            start: normalised,
            end: new_end,
            with: removed,
        }
    }
}

// ── Internals — char surgery ───────────────────────────────────

impl Buffer {
    /// Splice multi-line `text` at `at`. The first piece appends to
    /// the prefix of the row; intermediate pieces become new rows;
    /// the last piece prepends to the suffix.
    fn splice_at(&mut self, at: Position, text: &str) {
        let pieces: Vec<&str> = text.split('\n').collect();
        let row = at.row;
        let line = &mut self.lines_mut()[row];
        let byte = at.byte_offset(line);
        let suffix = line.split_off(byte);
        if pieces.len() == 1 {
            line.push_str(pieces[0]);
            line.push_str(&suffix);
            return;
        }
        line.push_str(pieces[0]);
        let mut new_rows: Vec<String> = pieces[1..pieces.len() - 1]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut last = pieces.last().copied().unwrap_or("").to_string();
        last.push_str(&suffix);
        new_rows.push(last);
        let insert_at = row + 1;
        for (i, l) in new_rows.into_iter().enumerate() {
            self.lines_mut().insert(insert_at + i, l);
        }
    }

    /// Remove `[start, end)` (charwise) and return what was removed
    /// with `\n` between rows.
    fn cut_chars(&mut self, start: Position, end: Position) -> String {
        let (start, end) = order(start, end);
        if start.row == end.row {
            let line = &mut self.lines_mut()[start.row];
            let lo = start.byte_offset(line).min(line.len());
            let hi = end.byte_offset(line).min(line.len());
            return line.drain(lo..hi).collect();
        }
        let mut out = String::new();
        // Suffix of start row.
        {
            let line = &mut self.lines_mut()[start.row];
            let byte = start.byte_offset(line).min(line.len());
            let suffix: String = line.drain(byte..).collect();
            out.push_str(&suffix);
        }
        out.push('\n');
        // Drain rows strictly between start.row and end.row.
        let mid_lo = start.row + 1;
        let mid_hi = end.row.saturating_sub(1);
        if mid_hi >= mid_lo {
            let drained: Vec<String> = self.lines_mut().drain(mid_lo..=mid_hi).collect();
            for l in drained {
                out.push_str(&l);
                out.push('\n');
            }
        }
        // Prefix of (now-shifted) end row.
        let end_line_idx = start.row + 1;
        {
            let line = &mut self.lines_mut()[end_line_idx];
            let byte = end.byte_offset(line).min(line.len());
            let prefix: String = line.drain(..byte).collect();
            out.push_str(&prefix);
        }
        // Glue start row + remainder of end row.
        let merged = self.lines_mut().remove(end_line_idx);
        self.lines_mut()[start.row].push_str(&merged);
        out
    }
}

fn order(a: Position, b: Position) -> (Position, Position) {
    if a <= b { (a, b) } else { (b, a) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_check(initial: &str, edit: Edit) {
        let mut b = Buffer::from_str(initial);
        let snapshot_before = b.as_string();
        let inverse = b.apply_edit(edit);
        b.apply_edit(inverse);
        assert_eq!(b.as_string(), snapshot_before);
    }

    #[test]
    fn insert_char_round_trip() {
        round_trip_check(
            "abc",
            Edit::InsertChar {
                at: Position::new(0, 1),
                ch: 'X',
            },
        );
    }

    #[test]
    fn insert_str_multiline_round_trip() {
        round_trip_check(
            "abc\ndef",
            Edit::InsertStr {
                at: Position::new(0, 2),
                text: "X\nY\nZ".into(),
            },
        );
    }

    #[test]
    fn delete_charwise_single_row_round_trip() {
        round_trip_check(
            "alpha bravo charlie",
            Edit::DeleteRange {
                start: Position::new(0, 6),
                end: Position::new(0, 11),
                kind: MotionKind::Char,
            },
        );
    }

    #[test]
    fn delete_charwise_multi_row_round_trip() {
        round_trip_check(
            "row0\nrow1\nrow2",
            Edit::DeleteRange {
                start: Position::new(0, 2),
                end: Position::new(2, 2),
                kind: MotionKind::Char,
            },
        );
    }

    #[test]
    fn delete_linewise_round_trip() {
        round_trip_check(
            "a\nb\nc\nd",
            Edit::DeleteRange {
                start: Position::new(1, 0),
                end: Position::new(2, 0),
                kind: MotionKind::Line,
            },
        );
    }

    #[test]
    fn delete_blockwise_round_trip() {
        round_trip_check(
            "abcdef\nghijkl\nmnopqr",
            Edit::DeleteRange {
                start: Position::new(0, 1),
                end: Position::new(2, 3),
                kind: MotionKind::Block,
            },
        );
    }

    #[test]
    fn join_lines_with_space_round_trip() {
        round_trip_check(
            "first\nsecond\nthird",
            Edit::JoinLines {
                row: 0,
                count: 2,
                with_space: true,
            },
        );
    }

    #[test]
    fn join_lines_no_space_round_trip() {
        round_trip_check(
            "first\nsecond",
            Edit::JoinLines {
                row: 0,
                count: 1,
                with_space: false,
            },
        );
    }

    #[test]
    fn replace_round_trip() {
        round_trip_check(
            "foo bar baz",
            Edit::Replace {
                start: Position::new(0, 4),
                end: Position::new(0, 7),
                with: "QUUX".into(),
            },
        );
    }

    #[test]
    fn delete_clearing_buffer_keeps_one_empty_row() {
        let mut b = Buffer::from_str("only");
        b.apply_edit(Edit::DeleteRange {
            start: Position::new(0, 0),
            end: Position::new(0, 0),
            kind: MotionKind::Line,
        });
        assert_eq!(b.row_count(), 1);
        assert_eq!(b.line(0), Some(""));
    }

    #[test]
    fn insert_char_lands_cursor_after() {
        let mut b = Buffer::from_str("abc");
        b.set_cursor(Position::new(0, 1));
        b.apply_edit(Edit::InsertChar {
            at: Position::new(0, 1),
            ch: 'X',
        });
        assert_eq!(b.cursor(), Position::new(0, 2));
        assert_eq!(b.line(0), Some("aXbc"));
    }

    #[test]
    fn block_delete_on_ragged_rows_handles_short_lines() {
        // Row 1 is shorter than the block right edge — only the
        // chars that exist get removed.
        let mut b = Buffer::from_str("longline\nhi\nthird row");
        let inv = b.apply_edit(Edit::DeleteRange {
            start: Position::new(0, 2),
            end: Position::new(2, 5),
            kind: MotionKind::Block,
        });
        b.apply_edit(inv);
        assert_eq!(b.as_string(), "longline\nhi\nthird row");
    }

    #[test]
    fn dirty_gen_bumps_per_edit() {
        let mut b = Buffer::from_str("abc");
        let g0 = b.dirty_gen();
        b.apply_edit(Edit::InsertChar {
            at: Position::new(0, 0),
            ch: 'X',
        });
        assert_eq!(b.dirty_gen(), g0 + 1);
        b.apply_edit(Edit::DeleteRange {
            start: Position::new(0, 0),
            end: Position::new(0, 1),
            kind: MotionKind::Char,
        });
        assert_eq!(b.dirty_gen(), g0 + 2);
    }
}
