# PingMyBell

**Free, open-source voice notifications and command center for AI coding agents — coming soon.**

PingMyBell will be a cross-platform (macOS + Windows) desktop app that gives AI coding agents like Claude Code and Codex CLI distinct voices — speaking a summary aloud when an agent finishes or needs your attention — plus a notch-style overlay to approve or deny agent actions and jump between sessions. 100% offline: OS-native TTS, no accounts, no telemetry, no cloud.

See the [PRD](PRD.md) for the full scope and the [architecture overview](ARCHITECTURE.md) for the technical design.

## Status

Early development — not usable yet. The spine is in place (Tauri v2 shell with tray icon, loopback-only ingest server, SQLite-backed session registry); voice, overlay, and agent integrations are coming per the build order in [ARCHITECTURE.md §10](ARCHITECTURE.md). The versions of [`pingmybell`](https://crates.io/crates/pingmybell) and [`pingmybell-shim`](https://crates.io/crates/pingmybell-shim) on crates.io are placeholders reserving the names for this project.

### Developing

Requires [Bun](https://bun.sh) and Rust stable.

```
bun install
bun run tauri dev     # run the app
cargo test --workspace
```

## License

[MIT](LICENSE)
