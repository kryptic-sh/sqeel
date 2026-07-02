//! End-to-end PTY test suite.
//!
//! Spawns the `sqeel` binary in `--sandbox` mode under a real pseudo-terminal
//! and scrapes the rendered screen via `vt100`. This catches the bug class
//! where internal state moves but the terminal display the user sees doesn't
//! follow — the exact class unit tests can't reach (render geometry, key
//! decoding, the query→results round-trip against the real SQLite backend).
//!
//! Run with:
//!   cargo test -p sqeel --test e2e
//!
//! Under nextest the suite is serialized by the `pty-e2e` test group (see
//! `.config/nextest.toml`) so concurrent ptys don't skew each other's timing.
//!
//! Unix-only: ConPTY behaves differently enough on Windows that the harness
//! assertions don't hold; gating the suite keeps Windows CI green.

#[cfg(unix)]
mod pty_harness;
