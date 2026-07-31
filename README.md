# PingMyBell

**Free, open-source voice notifications and a notch command center for AI
coding agents. Your fleet speaks when it finishes or needs you — 100% offline.**

Run several agents in parallel and the bottleneck becomes *you noticing them*:
sessions finish or block on a permission prompt while you're looking elsewhere.
PingMyBell gives each agent a distinct OS-native voice that speaks a short
summary the moment a session completes or needs attention, plus a notch-style
island and session board to see the whole fleet, approve or deny agent actions,
and jump to any session's terminal in one click. No cloud, no accounts, no
telemetry — the only network traffic is loopback.

## Status: early alpha — working end to end on macOS

The full loop runs today on macOS 13+: hooks fire, voices speak, the island
toasts and pins approvals, the board tracks live sessions with history, and the
app ships as a real `PingMyBell.app` with launch-at-login. Covered by a test
suite (`cargo test --workspace`) and gate-verified on real notch hardware.

Honest caveats, stated plainly:

- **From source only.** No signed prebuilt releases yet; you build the app
  yourself (below). The [`pingmybell`](https://crates.io/crates/pingmybell) /
  [`pingmybell-shim`](https://crates.io/crates/pingmybell-shim) crates on
  crates.io are name placeholders, not the app.
- **Windows is scaffolded, not shipped.** The platform layer compiles against
  the Windows APIs but hasn't been run or gated there yet.
- One callout template (terse) so far; the auto-updater is off by default.

## What it does

- **A voice per agent** — OS-native TTS (AVSpeechSynthesizer), distinct default
  voices per agent type, per-agent voice/rate pickers in the board. Priority
  queue (approvals preempt completions), 5 s dedup, tray mute.
- **The island** — a frameless, non-activating overlay flush with the notch
  (floating pill on non-notch displays): idle session dots → event toasts →
  pinned approvals. Never steals focus; visible over full-screen apps.
- **Approvals without the terminal** — gate tool calls and approve or deny from
  the island; a hook timeout or app kill always fails open to the agent's own
  prompt.
- **The board** — every live session at a glance with per-session history
  drawer; click any row to jump to that session's terminal.
- **Fail-open by design** — the hook shims always exit 0 silently on any
  internal error or when the app isn't running. A broken or absent PingMyBell
  never blocks or alters agent behavior.
- **Local-first, verifiably** — loopback-only ingest with a per-install bearer
  token, config and SQLite state under `~/.pingmybell/` with user-only
  permissions.

## Install (from source)

Requires [Bun](https://bun.sh) and Rust stable.

```sh
git clone https://github.com/Davidb3l/pingmybell && cd pingmybell
bun install
bun run tauri build        # → src-tauri/target/release/bundle/ (PingMyBell.app + .dmg)
```

Move `PingMyBell.app` to `/Applications`, launch it (tray bell appears), then
install the agent integrations — both are parse → merge → write (existing
settings preserved; uninstall removes exactly our entries):

```sh
/Applications/PingMyBell.app/Contents/MacOS/pingmybell install-claude   # Claude Code hooks
/Applications/PingMyBell.app/Contents/MacOS/pingmybell install-codex    # Codex CLI notify
```

For development: `bun run tauri dev` and `cargo test --workspace`.

## Agents

| Agent | Integration | Callouts | Approvals |
| --- | --- | --- | --- |
| Claude Code | hooks merged into `~/.claude/settings.json` | ✓ | ✓ (PreToolUse gating) |
| Codex CLI | `notify` in `~/.codex/config.toml` | ✓ | — (no blocking hook) |

The adapter design keeps new agents addable without core changes. On the
roadmap: an adapter for the [Sothis suite](https://github.com/Davidb3l/Sirius-Forester)
event spine (`.suite/events/*.jsonl`), so a [Sirius Forester](https://github.com/Davidb3l/Sirius-Forester)
fleet — gate verdicts, lock collisions, receipts — speaks too.

See the [PRD](PRD.md) for scope and the [architecture](ARCHITECTURE.md) for the
technical design (ingest server, shims, overlay, voice engine, build order).

## License

[MIT](LICENSE)
