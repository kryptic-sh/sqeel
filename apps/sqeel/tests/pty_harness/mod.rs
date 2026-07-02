//! PTY harness sub-modules, included from the `e2e` test binary.

pub mod harness;
// macOS pty timing mangles `:cmd\r` sequences into literal insert text on
// loaded runners (same flake class hjkl's e2e suite hit); restrict the
// behavioural suites to linux until root-caused. The harness unit tests
// (key notation) stay cross-platform.
#[cfg(all(unix, not(target_os = "macos")))]
pub mod smoke;
