//! Fuzzy-picker sources: saved-query files and query history.

use super::*;

/// App-local action enum for the file picker.
///
/// `hjkl_picker::PickerAction` no longer carries app-specific variants; every
/// consumer boxes its own type and downcasts on dispatch.
#[derive(Debug)]
pub(crate) enum SqeelFileAction {
    /// Open / switch to the `.sql` file at this path.
    OpenPath(std::path::PathBuf),
}

/// Thin wrapper around `hjkl_picker::FileSource` that overrides `select` to
/// emit `PickerAction::Custom(Box::new(SqeelFileAction::OpenPath(...)))`.
pub(crate) struct SqeelFileSource {
    inner: hjkl_picker::FileSource,
}

impl SqeelFileSource {
    fn new(root: std::path::PathBuf) -> Self {
        Self {
            inner: hjkl_picker::FileSource::new(root),
        }
    }
}

impl hjkl_picker::PickerLogic for SqeelFileSource {
    fn title(&self) -> &str {
        self.inner.title()
    }

    fn item_count(&self) -> usize {
        self.inner.item_count()
    }

    fn label(&self, idx: usize) -> String {
        self.inner.label(idx)
    }

    fn match_text(&self, idx: usize) -> String {
        self.inner.match_text(idx)
    }

    fn preview(&self, idx: usize) -> (hjkl_buffer::Buffer, String) {
        self.inner.preview(idx)
    }

    fn select(&self, idx: usize) -> hjkl_picker::PickerAction {
        let path = self
            .inner
            .items
            .lock()
            .ok()
            .and_then(|g| g.get(idx).map(|p| self.inner.root.join(p)));
        match path {
            Some(abs) => {
                hjkl_picker::PickerAction::Custom(Box::new(SqeelFileAction::OpenPath(abs)))
            }
            None => hjkl_picker::PickerAction::None,
        }
    }

    fn requery_mode(&self) -> hjkl_picker::RequeryMode {
        self.inner.requery_mode()
    }

    fn enumerate(
        &mut self,
        query: Option<&str>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<std::thread::JoinHandle<()>> {
        self.inner.enumerate(query, cancel)
    }
}

/// Build a fresh leader+space picker rooted at sqeel's queries dir
/// (`~/.local/share/sqeel/queries/`). Lists every saved `.sql` buffer,
/// not just currently-open tabs — closed buffers stay reachable via
/// fuzzy find. Selection emits `SqeelFileAction::OpenPath` via `PickerAction::Custom`.
pub(crate) fn open_query_picker() -> anyhow::Result<hjkl_picker::Picker> {
    let dir = sqeel_core::persistence::queries_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine sqeel queries dir"))?;
    std::fs::create_dir_all(&dir).ok();
    Ok(hjkl_picker::Picker::new(Box::new(SqeelFileSource::new(
        dir,
    ))))
}

/// App-local action enum for the history picker.
#[derive(Debug)]
pub(crate) enum SqeelHistoryAction {
    /// Load this query string into the active editor buffer.
    LoadQuery(String),
}

/// Format a `SystemTime` relative to `now` as a human-readable age string.
/// Examples: "5s ago", "3m ago", "2h ago", "4d ago", "2w ago".
pub(crate) fn format_relative_time(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or_default().as_secs();
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 7 * 86_400 {
        format!("{}d ago", secs / 86_400)
    } else if secs >= 365 * 86_400 {
        let years = secs / (365 * 86_400);
        if years == 1 {
            "1y ago".to_string()
        } else {
            format!("{years}y ago")
        }
    } else {
        let weeks = secs / (7 * 86_400);
        format!("{}w ago", weeks)
    }
}

/// In-memory picker source backed by a snapshot of `AppState::query_history`.
/// Newest entries appear first (snapshot is reversed on construction).
pub(crate) struct SqeelHistorySource {
    /// Snapshot taken at picker-open time; newest entry at index 0.
    entries: Vec<sqeel_core::state::HistoryEntry>,
    /// Wall-clock instant captured at construction, used for all label age
    /// calculations so every row's relative time is consistent.
    opened_at: SystemTime,
}

impl SqeelHistorySource {
    fn new(mut history: Vec<sqeel_core::state::HistoryEntry>) -> Self {
        history.reverse(); // newest first
        Self {
            entries: history,
            opened_at: SystemTime::now(),
        }
    }
}

impl hjkl_picker::PickerLogic for SqeelHistorySource {
    fn title(&self) -> &str {
        "Query history"
    }

    fn item_count(&self) -> usize {
        self.entries.len()
    }

    fn label(&self, idx: usize) -> String {
        let Some(entry) = self.entries.get(idx) else {
            return String::new();
        };
        let first_line = entry.query.lines().next().unwrap_or("").trim();
        let truncated = if first_line.chars().count() > 60 {
            let s: String = first_line.chars().take(60).collect();
            format!("{}…", s)
        } else {
            first_line.to_owned()
        };
        let age = format_relative_time(self.opened_at, entry.timestamp);
        format!("{:<63}  {}", truncated, age)
    }

    fn match_text(&self, idx: usize) -> String {
        self.entries
            .get(idx)
            .map(|e| e.query.clone())
            .unwrap_or_default()
    }

    fn preview(&self, idx: usize) -> (hjkl_buffer::Buffer, String) {
        let text = self
            .entries
            .get(idx)
            .map(|e| e.query.as_str())
            .unwrap_or_default();
        let buf = hjkl_buffer::Buffer::from_str(text);
        (buf, String::new())
    }

    fn select(&self, idx: usize) -> hjkl_picker::PickerAction {
        match self.entries.get(idx) {
            Some(entry) => hjkl_picker::PickerAction::Custom(Box::new(
                SqeelHistoryAction::LoadQuery(entry.query.clone()),
            )),
            None => hjkl_picker::PickerAction::None,
        }
    }

    fn requery_mode(&self) -> hjkl_picker::RequeryMode {
        hjkl_picker::RequeryMode::FilterInMemory
    }

    fn enumerate(
        &mut self,
        _query: Option<&str>,
        _cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<std::thread::JoinHandle<()>> {
        None
    }
}

/// Build a history picker from a snapshot of `AppState::query_history`.
/// Newest entries appear first. Returns `None` when history is empty.
pub(crate) fn open_history_picker(
    snapshot: Vec<sqeel_core::state::HistoryEntry>,
) -> Option<hjkl_picker::Picker> {
    if snapshot.is_empty() {
        return None;
    }
    Some(hjkl_picker::Picker::new(Box::new(SqeelHistorySource::new(
        snapshot,
    ))))
}
