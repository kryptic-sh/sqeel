# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.2.8] - 2026-05-15

### Changed

- **`save_connection` signature gains `tls: Option<&TlsConfig>` parameter**
  (issue #23 phase 1). When `Some`, the TLS configuration is serialised into the
  `[tls]` block of the connection TOML. `None` preserves prior behaviour (no
  `[tls]` written). All in-tree callers updated.
- `migrate_connection_to_keyring` now preserves the existing `tls` field on the
  rewritten TOML (was previously zeroing it).
- `TlsVerifyMode` derives `Default` (variant `Full`).

## [0.2.7] - 2026-05-15

### Added

- **`TlsConfig` + `TlsVerifyMode` types** (issue #23 phase 1 prep).
  `ConnectionConfig` gains an optional `tls: Option<TlsConfig>` field serialized
  as a `[tls]` TOML block. `TlsConfig` carries optional `ca_cert`,
  `client_cert`, `client_key` paths (all `PathBuf`) plus an optional
  `verify_mode` (`Full` / `Skip`, lowercased in TOML). Absent block → driver
  defaults apply. Connections without a `[tls]` block continue to round-trip
  cleanly (the field is `serde(default, skip_serializing_if)`).

## [0.2.6] - 2026-05-15

### Added

- **Keyring-backed credential storage** (`keyring-core = "1"`, `url = "2"` deps
  added). `save_connection` gains a `password: Option<&str>` parameter. When a
  password is supplied:
  - The password segment is stripped from the URL before writing to TOML.
  - The password is stored in the OS keyring under `("sqeel", name)`.
  - On keyring failure (no dbus, etc.) a warning is emitted and the URL with the
    inline password is written to TOML as a graceful fallback.
- `load_connections` now checks the OS keyring for connections whose stored URL
  has no inline password; when a keyring entry is found the password is spliced
  back into the URL before returning.
- `migrate_connection_to_keyring(name) -> anyhow::Result<MigrationResult>` —
  reads the TOML file for `name`, extracts any inline password, stores it in the
  keyring, and overwrites the TOML with the password-stripped URL. Returns
  `MigrationResult::{Migrated, NoPassword, KeyringFailed(String)}`.
- `delete_keyring_entry(name)` — removes the keyring entry for a connection
  (ignores "no entry" errors).
- `delete_connection` now calls `delete_keyring_entry` so keyring secrets are
  cleaned up alongside the TOML file.
- `install_mock_keyring()` test helper (cfg-gated to `#[cfg(test)]`) installs
  the `keyring-core` mock store as the process-wide default so unit tests never
  touch the user's real OS keyring. (kryptic-sh/sqeel#26)

## [0.2.5] - 2026-05-15

### Changed

- Bumped `hjkl-engine` dependency from 0.6 to 0.7. Re-exported types now resolve
  to engine 0.7. Tracks the engine churn that rotted the 0.6 ecosystem snapshot
  — `hjkl-form 0.3.7` caret-minor-bumped its engine pin to 0.7, dragging two
  engine majors into any consumer graph still on 0.6.

## [0.2.4] - 2026-05-14

### Changed

- Bumped `hjkl-engine` dependency from 0.5 to 0.6. The re-exported
  `KeybindingMode` type now resolves to engine 0.6. Track the engine churn that
  rotted the 0.5 ecosystem snapshot a day after release.

## [0.2.3] - 2026-05-13

### Changed

- Bumped `hjkl-engine` dependency from 0.3 to 0.5. The re-exported
  `KeybindingMode` type now resolves to engine 0.5, unifying the dependency
  graph for consumers (`sqeel-core`, `sqeel-tui`) that previously straddled two
  engine majors. No source-level API change — variants are identical.

## [0.2.2] - 2026-05-13

### Added

- `EditorConfig::cursorline` and `EditorConfig::cursorcolumn` fields (default
  `false`). Drives sqeel-tui's optional cursor-line / cursor-column highlights;
  user TOML can enable via `cursorline = true` / `cursorcolumn = true` under
  `[editor]`. (kryptic-sh/sqeel#10)
- `EditorConfig::lsp_auto_install` field (default `true`). When `true` and the
  configured `lsp_binary` is missing from `$PATH`, sqeel-tui prompts the user to
  install via `hjkl-anvil`. Set to `false` to silence the prompt.
  (kryptic-sh/sqeel#13)

## [0.2.1] - 2026-05-07

### Changed

- CI: collapsed `ci.yml` + `release.yml` + `_tests.yml` into a single `ci.yml`;
  added dependabot config for Cargo and GitHub Actions (weekly).

## [0.2.0] - 2026-05-05

### Added

- `ConnectionConfig`, `load_connections`, `save_connection`, and
  `delete_connection` moved here from `sqeel-core`. All connection-oriented TOML
  I/O now lives in one place. `config_dir()` is called directly — no extra
  plumbing needed. Three new tests: roundtrip save/load, delete, and
  invalid-name rejection.

## [0.1.0] - 2026-05-05

### Added

- Initial release. `MainConfig`, `EditorConfig`, `load_main_config`,
  `config_dir`, and `set_config_dir_override` extracted from `sqeel-core` and
  implemented on top of `hjkl_config::AppConfig`. Bundled defaults TOML
  (`config.toml`) is included via `include_str!()`. Deep-merge loading via
  `hjkl_config::load_layered_from` and XDG path resolution via
  `hjkl_config::config_dir`. No defaults are ever written to disk.

[Unreleased]: https://github.com/kryptic-sh/sqeel-config/compare/v0.2.8...HEAD
[0.2.8]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.2.8
[0.2.7]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.2.7
[0.2.6]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.2.6
[0.2.5]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.2.5
[0.2.4]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.2.4
[0.2.3]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.2.3
[0.2.2]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.2.2
[0.2.1]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.2.1
[0.2.0]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.2.0
[0.1.0]: https://github.com/kryptic-sh/sqeel-config/releases/tag/v0.1.0
