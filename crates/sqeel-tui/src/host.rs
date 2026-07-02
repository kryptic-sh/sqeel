//! `SqeelHost` — sqeel's [`hjkl_engine::Host`] implementation.
//!
//! Owns the runtime viewport (scroll offsets read/written by the engine,
//! width/height published by the renderer each frame) and the clipboard
//! outbox the engine pushes yanks/cuts into via [`Host::write_clipboard`].
//! The event loop drains the outbox once per key
//! ([`SqeelHost::take_clipboard_writes`]) to copy + toast.

use hjkl_engine::types::Viewport;
use hjkl_engine::{CursorShape, Host};
use std::time::Instant;

/// Host adapter wired into `Editor<Buffer, SqeelHost>`.
pub struct SqeelHost {
    /// Pending clipboard writes queued by the engine. The event loop
    /// drains them; the engine never blocks.
    clipboard_outbox: Vec<String>,
    started: Instant,
    /// Runtime viewport — host-owned since hjkl 0.0.34. The engine
    /// reads/writes scroll offsets; the renderer publishes width/height
    /// per frame.
    viewport: Viewport,
}

impl SqeelHost {
    pub fn new() -> Self {
        Self {
            clipboard_outbox: Vec::new(),
            started: Instant::now(),
            // Sensible default — renderer overwrites width/height per
            // frame from the editor pane's chunk rect.
            viewport: Viewport {
                top_row: 0,
                top_col: 0,
                width: 80,
                height: 24,
                ..Viewport::default()
            },
        }
    }

    /// Drain queued clipboard writes. The event loop copies them to the
    /// OS clipboard and toasts (the engine pushes every yank/cut here
    /// via [`Host::write_clipboard`]).
    pub fn take_clipboard_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.clipboard_outbox)
    }
}

impl Default for SqeelHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for SqeelHost {
    type Intent = ();

    fn write_clipboard(&mut self, text: String) {
        self.clipboard_outbox.push(text);
    }

    fn read_clipboard(&mut self) -> Option<String> {
        // Paste paths read the OS clipboard directly in the event loop
        // (`sync_clipboard_register`); the engine-side cache is unused.
        None
    }

    fn now(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    fn prompt_search(&mut self) -> Option<String> {
        // The `/` prompt is owned by the engine's search-prompt state;
        // this host hook is never consulted.
        None
    }

    fn emit_cursor_shape(&mut self, _shape: CursorShape) {
        // The renderer derives the terminal cursor shape from the vim
        // mode each frame; the engine's emission is redundant here.
    }

    fn emit_intent(&mut self, _intent: Self::Intent) {}

    fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.viewport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn satisfies_host_trait() {
        fn assert_host<H: Host>() {}
        assert_host::<SqeelHost>();
    }

    #[test]
    fn clipboard_outbox_drains() {
        let mut host = SqeelHost::new();
        host.write_clipboard("foo".into());
        host.write_clipboard("bar".into());
        assert_eq!(host.take_clipboard_writes(), vec!["foo", "bar"]);
        assert!(host.take_clipboard_writes().is_empty());
    }
}
