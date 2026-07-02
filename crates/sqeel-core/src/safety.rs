//! Destructive-statement classification for the run-confirm guard.
//!
//! [`destructive_kind`] flags the statements that most often ruin someone's
//! day when fired by accident: `UPDATE` / `DELETE` with no top-level `WHERE`,
//! `DROP`, and `TRUNCATE`. The TUI asks for a y/N confirm before dispatching
//! them (config: `editor.confirm_destructive`, default on).
//!
//! Classification is a token scan, not a full parse: comments and string
//! literals are stripped, then only parenthesis-depth-0 words are considered,
//! so a `WHERE` inside a subquery or a string can't mask (or fake) the
//! top-level clause. Conservative by design — statements the scanner doesn't
//! recognise are treated as safe rather than nagging on every `SELECT`.

/// What makes a statement dangerous. Ordered roughly by blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestructiveKind {
    /// `UPDATE …` with no top-level `WHERE` — rewrites every row.
    UpdateWithoutWhere,
    /// `DELETE …` with no top-level `WHERE` — deletes every row.
    DeleteWithoutWhere,
    /// `DROP TABLE` / `DROP DATABASE` / any `DROP …`.
    Drop,
    /// `TRUNCATE …` — deletes every row, usually without undo.
    Truncate,
}

impl DestructiveKind {
    /// Short human label for the confirm dialog.
    pub fn label(self) -> &'static str {
        match self {
            DestructiveKind::UpdateWithoutWhere => "UPDATE without WHERE",
            DestructiveKind::DeleteWithoutWhere => "DELETE without WHERE",
            DestructiveKind::Drop => "DROP",
            DestructiveKind::Truncate => "TRUNCATE",
        }
    }
}

/// Classify `stmt`, returning `Some(kind)` when it should be confirmed
/// before running. `None` means "safe to dispatch".
pub fn destructive_kind(stmt: &str) -> Option<DestructiveKind> {
    let words = top_level_words(stmt);
    let first = words.first()?;
    match first.as_str() {
        "DROP" => Some(DestructiveKind::Drop),
        "TRUNCATE" => Some(DestructiveKind::Truncate),
        "DELETE" => {
            if words.iter().any(|w| w == "WHERE") {
                None
            } else {
                Some(DestructiveKind::DeleteWithoutWhere)
            }
        }
        "UPDATE" => {
            if words.iter().any(|w| w == "WHERE") {
                None
            } else {
                Some(DestructiveKind::UpdateWithoutWhere)
            }
        }
        _ => None,
    }
}

/// Uppercased word tokens at parenthesis depth 0, with comments (`--` line,
/// `/* */` block) and string literals (`'…'`, `"…"`, `` `…` ``) skipped.
/// Doubled quotes inside a literal (`''`) read as two adjacent literals,
/// which is fine — the content is skipped either way.
fn top_level_words(stmt: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = stmt.chars().peekable();
    let mut depth = 0usize;

    let flush = |word: &mut String, words: &mut Vec<String>| {
        if !word.is_empty() {
            words.push(std::mem::take(word));
        }
    };

    while let Some(c) = chars.next() {
        match c {
            // Line comment.
            '-' if chars.peek() == Some(&'-') => {
                flush(&mut word, &mut words);
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            // Block comment.
            '/' if chars.peek() == Some(&'*') => {
                flush(&mut word, &mut words);
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            // String / quoted-identifier literals — skip to the closing quote.
            '\'' | '"' | '`' => {
                flush(&mut word, &mut words);
                for n in chars.by_ref() {
                    if n == c {
                        break;
                    }
                }
            }
            '(' => {
                flush(&mut word, &mut words);
                depth += 1;
            }
            ')' => {
                flush(&mut word, &mut words);
                depth = depth.saturating_sub(1);
            }
            c if c.is_alphanumeric() || c == '_' => {
                if depth == 0 {
                    word.extend(c.to_uppercase());
                }
            }
            _ => flush(&mut word, &mut words),
        }
    }
    flush(&mut word, &mut words);
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_is_safe() {
        assert_eq!(destructive_kind("SELECT * FROM users"), None);
        assert_eq!(destructive_kind("  select 1;"), None);
        assert_eq!(destructive_kind(""), None);
    }

    #[test]
    fn insert_is_safe() {
        assert_eq!(
            destructive_kind("INSERT INTO users (email) VALUES ('x@y.z')"),
            None
        );
    }

    #[test]
    fn delete_without_where_flagged() {
        assert_eq!(
            destructive_kind("DELETE FROM users"),
            Some(DestructiveKind::DeleteWithoutWhere)
        );
        assert_eq!(
            destructive_kind("delete from users;"),
            Some(DestructiveKind::DeleteWithoutWhere)
        );
    }

    #[test]
    fn delete_with_where_safe() {
        assert_eq!(destructive_kind("DELETE FROM users WHERE id = 1"), None);
    }

    #[test]
    fn update_without_where_flagged() {
        assert_eq!(
            destructive_kind("UPDATE users SET email = 'x'"),
            Some(DestructiveKind::UpdateWithoutWhere)
        );
    }

    #[test]
    fn update_with_where_safe() {
        assert_eq!(
            destructive_kind("UPDATE users SET email = 'x' WHERE id = 1"),
            None
        );
    }

    #[test]
    fn subquery_where_does_not_mask_missing_top_level_where() {
        // The WHERE lives inside the subquery — the outer UPDATE still hits
        // every row and must be flagged.
        assert_eq!(
            destructive_kind(
                "UPDATE users SET email = (SELECT email FROM backup WHERE backup.id = users.id)"
            ),
            Some(DestructiveKind::UpdateWithoutWhere)
        );
    }

    #[test]
    fn where_in_string_literal_does_not_count() {
        assert_eq!(
            destructive_kind("DELETE FROM logs -- WHERE we would filter"),
            Some(DestructiveKind::DeleteWithoutWhere)
        );
        assert_eq!(
            destructive_kind("UPDATE t SET note = 'WHERE clause goes here'"),
            Some(DestructiveKind::UpdateWithoutWhere)
        );
    }

    #[test]
    fn leading_comment_skipped() {
        assert_eq!(
            destructive_kind("-- cleanup\n/* really */ DELETE FROM users"),
            Some(DestructiveKind::DeleteWithoutWhere)
        );
    }

    #[test]
    fn drop_and_truncate_flagged() {
        assert_eq!(
            destructive_kind("DROP TABLE users"),
            Some(DestructiveKind::Drop)
        );
        assert_eq!(
            destructive_kind("drop database prod"),
            Some(DestructiveKind::Drop)
        );
        assert_eq!(
            destructive_kind("TRUNCATE TABLE audit_log"),
            Some(DestructiveKind::Truncate)
        );
    }

    #[test]
    fn delete_with_where_in_subquery_and_top_level_safe() {
        assert_eq!(
            destructive_kind("DELETE FROM t WHERE id IN (SELECT id FROM u WHERE stale = 1)"),
            None
        );
    }
}
