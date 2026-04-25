use crate::Position;

/// First-class vim selection. Each variant carries the kind directly
/// rather than relying on a single char-range primitive with separate
/// "treat as line / block" overlays — that's the whole point of
/// owning the buffer model. Anchor is where the user pressed
/// `v` / `V` / `Ctrl-V`; head moves with the cursor and is updated
/// via [`Selection::extend_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// `v` — character-wise. Covers `anchor..=head` inclusive,
    /// row-major. Empty rows in the middle of a multi-row span are
    /// treated as having one virtual cell so the highlight is
    /// visible (matches vim).
    Char { anchor: Position, head: Position },
    /// `V` — line-wise. Both endpoints are pure row indices; column
    /// is irrelevant (the whole row is always covered).
    Line { anchor_row: usize, head_row: usize },
    /// `Ctrl-V` — block-wise. Covers the inclusive rectangle whose
    /// corners are anchor and head. Row range is `min..=max`; column
    /// range is `min..=max` independently of the corner diagonals.
    Block { anchor: Position, head: Position },
}

/// Bounds of a selection on a particular row, expressed as inclusive
/// char-column range. `None` means the row is outside the selection.
/// `Some((0, usize::MAX))` is the convention for "whole row" — the
/// renderer caps it at the row's actual length.
pub type RowSpan = Option<(usize, usize)>;

impl Selection {
    /// Where the cursor end of the selection lives. After
    /// [`Selection::extend_to`] this is the freshly-set value.
    pub fn head(self) -> Position {
        match self {
            Selection::Char { head, .. } => head,
            Selection::Line { head_row, .. } => Position::new(head_row, 0),
            Selection::Block { head, .. } => head,
        }
    }

    /// The opposite end of the selection — fixed when the user
    /// entered visual mode.
    pub fn anchor(self) -> Position {
        match self {
            Selection::Char { anchor, .. } => anchor,
            Selection::Line { anchor_row, .. } => Position::new(anchor_row, 0),
            Selection::Block { anchor, .. } => anchor,
        }
    }

    /// Move the cursor end of the selection to `pos`. Anchor stays
    /// put; for `Line` we drop the column since rows are all that
    /// matter.
    pub fn extend_to(&mut self, pos: Position) {
        match self {
            Selection::Char { head, .. } => *head = pos,
            Selection::Line { head_row, .. } => *head_row = pos.row,
            Selection::Block { head, .. } => *head = pos,
        }
    }

    /// What columns of `row` the selection covers. Used by the
    /// render layer to paint the selection bg without having to
    /// know each variant's quirks.
    ///
    /// - `Char` on a single row: `[min_col, max_col]`.
    /// - `Char` spanning rows: from `head/anchor.col` on the start
    ///   row to end-of-line, then full rows in between, then
    ///   `0..=end.col` on the last row.
    /// - `Line`: `(0, usize::MAX)` for every row in range.
    /// - `Block`: `[min_col, max_col]` regardless of which row.
    pub fn row_span(self, row: usize) -> RowSpan {
        match self {
            Selection::Char { anchor, head } => {
                let (start, end) = order(anchor, head);
                if row < start.row || row > end.row {
                    return None;
                }
                let lo = if row == start.row { start.col } else { 0 };
                let hi = if row == end.row { end.col } else { usize::MAX };
                Some((lo, hi))
            }
            Selection::Line {
                anchor_row,
                head_row,
            } => {
                let (lo, hi) = if anchor_row <= head_row {
                    (anchor_row, head_row)
                } else {
                    (head_row, anchor_row)
                };
                if row < lo || row > hi {
                    None
                } else {
                    Some((0, usize::MAX))
                }
            }
            Selection::Block { anchor, head } => {
                let (top, bot) = (anchor.row.min(head.row), anchor.row.max(head.row));
                if row < top || row > bot {
                    return None;
                }
                let (left, right) = (anchor.col.min(head.col), anchor.col.max(head.col));
                Some((left, right))
            }
        }
    }

    /// Inclusive `(top_row, bottom_row)` covered by the selection.
    pub fn row_bounds(self) -> (usize, usize) {
        match self {
            Selection::Char { anchor, head } => {
                let (s, e) = order(anchor, head);
                (s.row, e.row)
            }
            Selection::Line {
                anchor_row,
                head_row,
            } => (anchor_row.min(head_row), anchor_row.max(head_row)),
            Selection::Block { anchor, head } => {
                (anchor.row.min(head.row), anchor.row.max(head.row))
            }
        }
    }
}

/// Order a pair of positions row-major.
fn order(a: Position, b: Position) -> (Position, Position) {
    if a <= b { (a, b) } else { (b, a) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_single_row_inclusive() {
        let sel = Selection::Char {
            anchor: Position::new(0, 2),
            head: Position::new(0, 5),
        };
        assert_eq!(sel.row_span(0), Some((2, 5)));
        assert_eq!(sel.row_span(1), None);
    }

    #[test]
    fn char_multi_row_clips_endpoints() {
        let sel = Selection::Char {
            anchor: Position::new(1, 3),
            head: Position::new(3, 7),
        };
        assert_eq!(sel.row_span(0), None);
        assert_eq!(sel.row_span(1), Some((3, usize::MAX)));
        assert_eq!(sel.row_span(2), Some((0, usize::MAX)));
        assert_eq!(sel.row_span(3), Some((0, 7)));
        assert_eq!(sel.row_span(4), None);
    }

    #[test]
    fn char_handles_reversed_endpoints() {
        // Cursor moved up-left of anchor.
        let sel = Selection::Char {
            anchor: Position::new(3, 7),
            head: Position::new(1, 3),
        };
        assert_eq!(sel.row_span(1), Some((3, usize::MAX)));
        assert_eq!(sel.row_span(3), Some((0, 7)));
    }

    #[test]
    fn line_covers_whole_rows_only() {
        let sel = Selection::Line {
            anchor_row: 5,
            head_row: 7,
        };
        assert_eq!(sel.row_span(4), None);
        assert_eq!(sel.row_span(5), Some((0, usize::MAX)));
        assert_eq!(sel.row_span(6), Some((0, usize::MAX)));
        assert_eq!(sel.row_span(7), Some((0, usize::MAX)));
        assert_eq!(sel.row_span(8), None);
    }

    #[test]
    fn block_inclusive_rect() {
        let sel = Selection::Block {
            anchor: Position::new(2, 4),
            head: Position::new(5, 8),
        };
        for row in 2..=5 {
            assert_eq!(sel.row_span(row), Some((4, 8)));
        }
        assert_eq!(sel.row_span(1), None);
        assert_eq!(sel.row_span(6), None);
    }

    #[test]
    fn block_normalises_corners() {
        // Anchor bottom-right, head top-left.
        let sel = Selection::Block {
            anchor: Position::new(5, 8),
            head: Position::new(2, 4),
        };
        for row in 2..=5 {
            assert_eq!(sel.row_span(row), Some((4, 8)));
        }
    }

    #[test]
    fn extend_to_updates_head() {
        let mut sel = Selection::Char {
            anchor: Position::new(0, 0),
            head: Position::new(0, 3),
        };
        sel.extend_to(Position::new(2, 9));
        assert_eq!(sel.head(), Position::new(2, 9));
        assert_eq!(sel.anchor(), Position::new(0, 0));
    }

    #[test]
    fn line_extend_to_drops_column() {
        let mut sel = Selection::Line {
            anchor_row: 1,
            head_row: 1,
        };
        sel.extend_to(Position::new(4, 50));
        assert_eq!(sel.head(), Position::new(4, 0));
    }

    #[test]
    fn row_bounds_each_kind() {
        let c = Selection::Char {
            anchor: Position::new(2, 0),
            head: Position::new(5, 0),
        };
        assert_eq!(c.row_bounds(), (2, 5));
        let l = Selection::Line {
            anchor_row: 7,
            head_row: 3,
        };
        assert_eq!(l.row_bounds(), (3, 7));
        let b = Selection::Block {
            anchor: Position::new(8, 1),
            head: Position::new(2, 9),
        };
        assert_eq!(b.row_bounds(), (2, 8));
    }
}
