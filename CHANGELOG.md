# Changelog

All notable changes to PingMyBell are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project intends to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it has a release.

There are **no releases yet** — PingMyBell is pre-alpha and built from source,
so everything below the Unreleased heading is what has landed on `main` since
this file was started. Earlier history lives in the git log and in `STATUS.md`.

## [Unreleased]

### Added

- Community health files for the open source program: `CONTRIBUTING.md`,
  `CODE_OF_CONDUCT.md`, `SECURITY.md`, `SUPPORT.md`, issue and pull request
  templates, and this changelog. A PR-only CI check enforces the DCO sign-off
  that `CONTRIBUTING.md` asks for.
- A README header with a one-line pitch and license and CI badges.

### Fixed

- Focus-aware quieting now works on Windows. A callout for a session whose
  terminal is the window in front of you is downgraded to a chime, as it
  already was on macOS; before, the setting did nothing on Windows and every
  callout was spoken. Verified in Windows Terminal, including tabs it adopts
  as the default terminal; like macOS it cannot tell one tab from another
  (PMB-12).
- Focus-aware quieting under tmux: a session is quiet only while its pane is
  the one a terminal in front of you is showing. Before, it never quieted
  under tmux. Unit-tested; not yet checked live (PMB-12).
- `cargo test` runs on Windows. The test binaries were built without the
  app's Windows manifest and could not start, and six tests assumed a Unix
  machine — one of them hung the suite forever there.
