//! Shared animated spinner for "loading" indicators across the TUI.
//!
//! Uses a monotonic wall-clock epoch so the frame advances at a steady
//! ~8 Hz regardless of how often individual widgets redraw. Callers
//! grab the current frame via [`frame`] and render it wherever they
//! like.

use std::sync::OnceLock;
use std::time::Instant;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Returns the spinner character for the current wall-clock tick.
/// Sampled at ~8 Hz (frame advances every 120 ms).
pub fn frame() -> &'static str {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let start = *EPOCH.get_or_init(Instant::now);
    let idx = (start.elapsed().as_millis() / 120) as usize % FRAMES.len();
    FRAMES[idx]
}
