//! PTY harness sub-modules, included from the `e2e` test binary.

pub mod harness;
// macOS runs the full suite too: the "`:cmd\r` typed as literal text" flake
// class hjkl's suite hit there is (per root-cause on the Esc+byte
// Alt-coalescing mechanism) addressed by the harness splitting writes after
// every bare Esc — see `TerminalSession::keys`. If macOS CI still flakes,
// re-gate with `#[cfg(all(unix, not(target_os = "macos")))]` and note the
// failing test here.
pub mod smoke;
