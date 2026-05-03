# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/) once it reaches
0.1.0; the 0.0.x series is a churn phase where breaking changes may land on
patch bumps.

## [Unreleased]

### Added

- `sqeel --help` now renders an ASCII-art banner (figlet "ANSI Regular" font)
  with the package version inline. Banner lives in `apps/sqeel/src/bin/art.txt`,
  embedded via `include_str!`. Regenerate with
  `figlet -f "ANSI Regular" sqeel > apps/sqeel/src/bin/art.txt`.
- `--version` flag (clap auto-derive from `CARGO_PKG_VERSION`).
- CLI smoke tests: `--version` returns `CARGO_PKG_VERSION`, long-form help
  contains the embedded art block and the version string.
