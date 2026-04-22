//! Unified clipboard sink.
//!
//! Tries `arboard` first (local X11/Wayland/macOS/Windows). When that fails —
//! typically over SSH with no forwarded display — falls back to OSC 52, which
//! ships the payload to the user's terminal emulator so their *local* machine's
//! clipboard receives it (works in iTerm2, WezTerm, Alacritty, kitty, tmux, and
//! recent xterm).
//!
//! When `$SSH_TTY` or `$SSH_CONNECTION` is set we emit OSC 52 in addition to
//! attempting arboard, since arboard inside an SSH session normally succeeds on
//! the remote host's (headless) display and silently drops the payload.

use std::io::{self, Write};

const OSC52_MAX: usize = 74_000;

pub struct Clipboard {
    inner: Option<arboard::Clipboard>,
    over_ssh: bool,
}

impl Clipboard {
    pub fn new() -> Self {
        Self {
            inner: arboard::Clipboard::new().ok(),
            over_ssh: std::env::var_os("SSH_TTY").is_some()
                || std::env::var_os("SSH_CONNECTION").is_some(),
        }
    }

    /// Read current OS clipboard text. Returns `None` when no local clipboard
    /// is available (e.g. SSH with only OSC 52 — OSC 52 is write-only from the
    /// remote side, so we can't pull the user's laptop clipboard back).
    pub fn get_text(&mut self) -> Option<String> {
        self.inner.as_mut().and_then(|cb| cb.get_text().ok())
    }

    pub fn set_text(&mut self, text: &str) {
        let mut native_ok = false;
        if !self.over_ssh
            && let Some(ref mut cb) = self.inner
        {
            native_ok = cb.set_text(text.to_owned()).is_ok();
        }
        if !native_ok {
            let _ = emit_osc52(text);
        }
    }
}

fn emit_osc52(text: &str) -> io::Result<()> {
    let encoded = base64_encode(text.as_bytes());
    if encoded.len() > OSC52_MAX {
        return Err(io::Error::other("payload exceeds OSC 52 size cap"));
    }
    let mut out = io::stdout().lock();
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for chunk in &mut chunks {
        let b = (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8 | (chunk[2] as u32);
        out.push(ALPHA[((b >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((b >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((b >> 6) & 0x3f) as usize] as char);
        out.push(ALPHA[(b & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let b = (rem[0] as u32) << 16;
            out.push(ALPHA[((b >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((b >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let b = (rem[0] as u32) << 16 | (rem[1] as u32) << 8;
            out.push(ALPHA[((b >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((b >> 12) & 0x3f) as usize] as char);
            out.push(ALPHA[((b >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
