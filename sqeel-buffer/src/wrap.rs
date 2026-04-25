//! Soft-wrap helpers shared between the renderer, viewport scroll,
//! and the buffer's vertical motion code.

use unicode_width::UnicodeWidthChar;

/// Soft-wrap mode controlling how doc rows wider than the text area
/// turn into multiple visual rows. Default is [`Wrap::None`] — every
/// doc row is exactly one screen row and `top_col` clips the left
/// side, mirroring vim's `set nowrap` default for sqeel today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wrap {
    /// Single screen row per doc row; clip with `top_col`.
    #[default]
    None,
    /// Break at the cell boundary regardless of word edges.
    Char,
    /// Break at the last whitespace inside the visible width when
    /// possible; falls back to a char break for runs longer than the
    /// width.
    Word,
}

/// Split `line` into char-index segments `[start, end)` such that
/// each segment's display width fits within `width` cells.
/// `Wrap::Word` rewinds to the last whitespace inside the candidate
/// segment when a break would otherwise split a word; falls through
/// to a char break for runs longer than `width`. `Wrap::None` is not
/// expected here — callers branch before calling — but is handled
/// for completeness as a single segment covering the full line.
pub fn wrap_segments(line: &str, width: u16, mode: Wrap) -> Vec<(usize, usize)> {
    let total = line.chars().count();
    if matches!(mode, Wrap::None) || width == 0 || line.is_empty() {
        return vec![(0, total)];
    }
    let chars: Vec<(char, u16)> = line
        .chars()
        .map(|c| (c, c.width().unwrap_or(1).max(1) as u16))
        .collect();
    let mut segs = Vec::new();
    let mut start = 0usize;
    while start < total {
        let mut cells: u16 = 0;
        let mut i = start;
        while i < total {
            let w = chars[i].1;
            if cells + w > width {
                break;
            }
            cells += w;
            i += 1;
        }
        if i == total {
            segs.push((start, total));
            break;
        }
        let break_at = if matches!(mode, Wrap::Word) {
            // Look for the last whitespace inside [start, i] so the
            // segment ends *after* that whitespace. Falls back to a
            // hard char break when the segment has no whitespace.
            (start..i)
                .rev()
                .find(|&k| chars[k].0.is_whitespace())
                .map(|k| k + 1)
                .filter(|&end| end > start)
                .unwrap_or(i)
        } else {
            i
        };
        segs.push((start, break_at));
        start = break_at;
    }
    if segs.is_empty() {
        segs.push((0, 0));
    }
    segs
}

/// Returns the index into `segments` whose `[start, end)` covers
/// `col`. The past-end cursor (`col == last segment's end`) maps to
/// the last segment, matching vim's "EOL on the visual row that
/// holds the line's last char" behaviour.
pub fn segment_for_col(segments: &[(usize, usize)], col: usize) -> usize {
    if segments.is_empty() {
        return 0;
    }
    if let Some(idx) = segments.iter().position(|&(s, e)| col >= s && col < e) {
        return idx;
    }
    segments.len() - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_returns_full_line_segment() {
        let segs = wrap_segments("hello world", 4, Wrap::None);
        assert_eq!(segs, vec![(0, 11)]);
    }

    #[test]
    fn segment_for_col_finds_containing_segment() {
        let segs = vec![(0, 4), (4, 8), (8, 10)];
        assert_eq!(segment_for_col(&segs, 0), 0);
        assert_eq!(segment_for_col(&segs, 3), 0);
        assert_eq!(segment_for_col(&segs, 4), 1);
        assert_eq!(segment_for_col(&segs, 7), 1);
        assert_eq!(segment_for_col(&segs, 9), 2);
        // Past-end col clamps to last segment.
        assert_eq!(segment_for_col(&segs, 10), 2);
        assert_eq!(segment_for_col(&segs, 99), 2);
    }
}
