# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.4.0] - 2026-05-05

### Added

- Initial release. `MainConfig`, `EditorConfig`, `load_main_config`,
  `config_dir`, and `set_config_dir_override` extracted from `sqeel-core` and
  implemented on top of `hjkl_config::AppConfig`. Bundled defaults TOML
  (`config.toml`) is included via `include_str!()`. Deep-merge loading via
  `hjkl_config::load_layered_from` and XDG path resolution via
  `hjkl_config::config_dir`. No defaults are ever written to disk.

[0.4.0]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.0
