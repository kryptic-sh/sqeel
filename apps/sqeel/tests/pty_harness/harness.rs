//! PTY-based test harness for end-to-end testing.
//!
//! [`TerminalSession`] spawns the `sqeel` binary in `--sandbox` mode under a
//! real pseudo-terminal, feeds keystrokes, and queries the rendered screen via
//! the [`vt100`] parser. Ported from `hjkl`'s pty harness (apps/hjkl/tests/
//! pty_harness/harness.rs) and trimmed to sqeel's needs.
//!
//! # Timing
//!
//! Never trust a fixed post-key settle for content assertions — poll with
//! [`TerminalSession::wait_for_text`] until the expected content renders.
//! The default settle after `keys()` is 200 ms (override `E2E_SETTLE_MS`);
//! the initial spawn wait is 500 ms (override `E2E_SPAWN_MS`) — sqeel opens
//! a SQLite connection and loads the sandbox session on startup, so it needs
//! a beat longer than a bare editor.

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem as _};
use std::io::{Read as _, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn settle_ms() -> u64 {
    std::env::var("E2E_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

fn spawn_ms() -> u64 {
    std::env::var("E2E_SPAWN_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500)
}

/// An active sqeel session running under a real pty.
pub struct TerminalSession {
    /// Master side of the pty. Kept alive so the pty stays open; the reader
    /// thread holds a separate clone.
    #[allow(dead_code)]
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
    rows: u16,
    cols: u16,
    /// Isolated XDG cache dir (bonsai grammar cache etc.) so e2e runs never
    /// touch — or race on — the real user cache.
    #[allow(dead_code)]
    cache_dir: tempfile::TempDir,
}

impl TerminalSession {
    /// Spawn `sqeel --sandbox --no-splash` at the default size (80x24).
    ///
    /// The sandbox seeds a SQLite `sample` connection plus a `sample_users.sql`
    /// buffer (CREATE TABLE users + INSERTs + SELECT), so the session lands on
    /// a working editor with a live database and no user config involved.
    ///
    /// If `sqls` is missing on `$PATH` (always true in CI) sqeel opens a y/N
    /// LSP-install modal on startup; this constructor dismisses it when
    /// present so tests start from a plain editor either way.
    pub fn spawn_sandbox() -> Self {
        let mut s = Self::spawn_inner(24, 80);
        s.dismiss_sqls_modal_if_present();
        s
    }

    fn spawn_inner(rows: u16, cols: u16) -> Self {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let sqeel_bin = env!("CARGO_BIN_EXE_sqeel");
        let mut cmd = CommandBuilder::new(sqeel_bin);
        cmd.arg("--sandbox");
        cmd.arg("--no-splash");
        cmd.env("TERM", "xterm-256color");
        // Skip the exit-time "Delete sandbox dir?" prompt and always clean:
        // the TUI's async stdin reader races any scripted answer bytes, so
        // prompt-driven cleanup from the harness is unreliable.
        cmd.env("SQEEL_SANDBOX_AUTOCLEAN", "1");
        // Isolated per-session cache so the background tree-sitter grammar
        // fetch (hjkl-bonsai) never races another test process on the shared
        // `~/.cache/bonsai` clone dir. Assertions never depend on highlight,
        // so a failed/slow grammar load in CI is harmless.
        let cache_dir = tempfile::tempdir().expect("e2e cache tempdir");
        cmd.env("XDG_CACHE_HOME", cache_dir.path());

        let child = pair.slave.spawn_command(cmd).expect("spawn sqeel");

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let parser_clone = Arc::clone(&parser);
        let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut p = parser_clone.lock().unwrap();
                        p.process(&buf[..n]);
                    }
                }
            }
        });

        let writer = pair.master.take_writer().expect("take pty writer");

        let session = Self {
            master: pair.master,
            writer,
            child,
            parser,
            rows,
            cols,
            cache_dir,
        };

        session.wait_ms(spawn_ms());
        session
    }

    /// Dismiss the startup "SQL LSP not found" y/N modal iff it rendered.
    ///
    /// Deterministic in both environments: with `sqls` on `$PATH` (dev
    /// machines) the modal never opens and nothing is sent; without it (CI)
    /// the modal is up within the spawn wait and `n` closes it. Never send a
    /// blind `n` — with no modal open it would insert into the buffer.
    fn dismiss_sqls_modal_if_present(&mut self) {
        // The modal paints synchronously with the first frame; a short poll
        // covers slow CI first-frame timing.
        for _ in 0..20 {
            if self.screen_contains("SQL LSP not found") {
                self.keys("n");
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    // ── Input ─────────────────────────────────────────────────────────────

    /// Send a vim-notation key sequence and wait for the screen to settle.
    ///
    /// Accepted notation: bare characters, `<Esc>`, `<Enter>`, `<Tab>`,
    /// `<Backspace>`, `<Space>`, `<C-x>` (ctrl-x), arrows, and sequences
    /// thereof (`gg`, `:q!<Enter>`, `<Space><Tab>`).
    ///
    /// A bare Esc (`<Esc>` followed by more keys) is flushed in its own
    /// write with a pacing gap: when an Esc and the byte after it land in
    /// one `read()`, crossterm decodes them as a single Alt+key (dropped
    /// by the app) instead of Esc-then-key. macOS ptys deliver a burst in
    /// one read far more consistently than Linux, which is the mechanism
    /// behind the "`:cmd\r` typed as literal text" flake class hjkl's
    /// suite hit there. Escape SEQUENCES (arrows, `\x1b[A`) must stay in
    /// one write — only a standalone Esc splits.
    pub fn keys(&mut self, seq: &str) {
        let bytes = vim_notation_to_bytes(seq);
        for chunk in split_after_bare_esc(&bytes) {
            self.writer.write_all(chunk).expect("write to pty");
            self.writer.flush().expect("flush pty");
            if chunk.last() == Some(&0x1b) {
                // Give the app's ESC-disambiguation timer room to fire so
                // the next byte can't fuse into an Alt+key.
                std::thread::sleep(Duration::from_millis(60));
            }
        }
        self.wait_ms(settle_ms());
    }

    // ── Screen queries ────────────────────────────────────────────────────

    /// Snapshot the current screen state.
    pub fn screen(&self) -> vt100::Screen {
        self.parser.lock().unwrap().screen().clone()
    }

    /// Rendered text of a 0-based screen row (trailing spaces stripped).
    pub fn line(&self, row: u16) -> String {
        let screen = self.screen();
        let mut s = String::new();
        for col in 0..self.cols {
            let cell = screen.cell(row, col);
            let ch = cell.map(|c| c.contents()).unwrap_or("");
            if ch.is_empty() {
                s.push(' ');
            } else {
                s.push_str(ch);
            }
        }
        s.trim_end().to_string()
    }

    /// True when any screen row currently contains `needle`.
    pub fn screen_contains(&self, needle: &str) -> bool {
        (0..self.rows).any(|r| self.line(r).contains(needle))
    }

    /// Poll until any row contains `needle`, up to `timeout_ms` (20 ms
    /// granularity). Returns `true` as soon as it appears. This is the
    /// primary assertion primitive — content-poll, never fixed-settle.
    pub fn wait_for_text(&self, needle: &str, timeout_ms: u64) -> bool {
        let steps = (timeout_ms / 20).max(1);
        for _ in 0..steps {
            if self.screen_contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.screen_contains(needle)
    }

    /// Poll until the spawned process exits, up to `timeout_ms`. Returns
    /// `true` on clean exit, `false` if it's still running at the deadline.
    pub fn wait_for_exit(&mut self, timeout_ms: u64) -> bool {
        let steps = (timeout_ms / 20).max(1);
        for _ in 0..steps {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Full screen dump for failure messages.
    pub fn screen_dump(&self) -> String {
        (0..self.rows)
            .map(|r| format!("{r:2}| {}", self.line(r)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_ms(&self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Best-effort graceful quit: `SQEEL_SANDBOX_AUTOCLEAN=1` makes the
        // binary delete its sandbox temp dir on exit with no prompt, so a
        // force-quit is all that's needed for a clean shutdown.
        //
        // The Esc must go in its own write, separated by a beat: `\x1b:` in
        // one burst is decoded by crossterm as Alt+`:` (ignored) and the
        // rest types garbage instead of quitting.
        let _ = self.writer.write_all(b"\x1b");
        let _ = self.writer.flush();
        std::thread::sleep(Duration::from_millis(60));
        let _ = self.writer.write_all(b":q!\r");
        let _ = self.writer.flush();
        // Give the process a moment to unwind, then hard-kill as backstop.
        for _ in 0..40 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
    }
}

/// Split `bytes` into chunks so every standalone Esc (`0x1b` NOT followed
/// by `[` or `O`, i.e. not a CSI/SS3 escape sequence) ends its chunk. The
/// caller writes chunks separately with a pacing gap after each Esc-final
/// chunk.
fn split_after_bare_esc(bytes: &[u8]) -> Vec<&[u8]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let next = bytes.get(i + 1);
            let is_sequence = matches!(next, Some(b'[') | Some(b'O'));
            if !is_sequence && i + 1 < bytes.len() {
                chunks.push(&bytes[start..=i]);
                start = i + 1;
            }
        }
        i += 1;
    }
    if start < bytes.len() {
        chunks.push(&bytes[start..]);
    }
    chunks
}

// ── Key translation ───────────────────────────────────────────────────────

/// Translate a vim-style notation string to raw bytes suitable for pty input.
fn vim_notation_to_bytes(seq: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = seq.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '<' {
            let mut tag = String::new();
            let mut closed = false;
            for next in chars.by_ref() {
                if next == '>' {
                    closed = true;
                    break;
                }
                tag.push(next);
            }
            if !closed {
                out.push(b'<');
                out.extend_from_slice(tag.as_bytes());
                continue;
            }
            let lower = tag.to_ascii_lowercase();
            match lower.as_str() {
                "esc" | "escape" => out.push(0x1b),
                "enter" | "cr" | "return" => out.push(b'\r'),
                "tab" => out.push(b'\t'),
                "bs" | "backspace" => out.push(0x7f),
                "space" => out.push(b' '),
                "up" => out.extend_from_slice(b"\x1b[A"),
                "down" => out.extend_from_slice(b"\x1b[B"),
                "right" => out.extend_from_slice(b"\x1b[C"),
                "left" => out.extend_from_slice(b"\x1b[D"),
                _ => {
                    if let Some(bytes) = parse_modifier_tag(&tag) {
                        out.extend_from_slice(&bytes);
                    } else {
                        out.push(b'<');
                        out.extend_from_slice(tag.as_bytes());
                        out.push(b'>');
                    }
                }
            }
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }

    out
}

/// Parse a `C-x` modifier tag into its control byte. Returns `None` for
/// unrecognised tags.
fn parse_modifier_tag(tag: &str) -> Option<Vec<u8>> {
    let (modifier, key) = tag.split_once('-')?;
    if !modifier.eq_ignore_ascii_case("c") {
        return None;
    }
    if key.len() == 1 {
        let c = key.chars().next().unwrap();
        return Some(vec![(c as u8) & 0x1f]);
    }
    match key.to_ascii_lowercase().as_str() {
        "enter" | "cr" => Some(vec![b'\r']),
        "tab" => Some(vec![b'\t']),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_esc_splits_chunk() {
        // Esc mid-sequence ends its chunk; remainder is a new chunk.
        let bytes = vim_notation_to_bytes("i-- x<Esc>:q!<Enter>");
        let chunks = split_after_bare_esc(&bytes);
        assert_eq!(chunks.len(), 2, "chunks: {chunks:?}");
        assert_eq!(chunks[0].last(), Some(&0x1b));
        assert_eq!(chunks[1], b":q!\r");
    }

    #[test]
    fn escape_sequences_stay_whole() {
        // Arrow keys are CSI sequences — must NOT split after their Esc.
        let bytes = vim_notation_to_bytes("<Up><Down>x");
        let chunks = split_after_bare_esc(&bytes);
        assert_eq!(chunks.len(), 1, "chunks: {chunks:?}");
    }

    #[test]
    fn trailing_esc_single_chunk() {
        let bytes = vim_notation_to_bytes("ihello<Esc>");
        let chunks = split_after_bare_esc(&bytes);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].last(), Some(&0x1b));
    }

    #[test]
    fn notation_simple_keys() {
        assert_eq!(vim_notation_to_bytes("abc"), b"abc");
        assert_eq!(vim_notation_to_bytes("<Esc>"), &[0x1b]);
        assert_eq!(vim_notation_to_bytes("<Enter>"), b"\r");
        assert_eq!(vim_notation_to_bytes("<Space><Tab>"), b" \t");
    }

    #[test]
    fn notation_ctrl_keys() {
        assert_eq!(vim_notation_to_bytes("<C-p>"), &[0x10]);
        assert_eq!(vim_notation_to_bytes("<C-n>"), &[0x0e]);
    }

    #[test]
    fn notation_composite() {
        assert_eq!(vim_notation_to_bytes(":q!<Enter>"), b":q!\r");
        assert_eq!(vim_notation_to_bytes("/SELECT<CR>"), b"/SELECT\r");
    }
}
