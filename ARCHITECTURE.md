# PingMyBell — Architecture Overview

> Companion to `PRD.md`. This is the technical source of truth: stack, component boundaries, wire formats, event flows, and build order.

## 1. Stack

| Layer | Choice | Rationale |
|---|---|---|
| App shell | **Tauri v2** | One codebase → native macOS + Windows; ~10 MB installers; system webview (no bundled Chromium); Rust backend has direct OS API access for window levels, TTS, and window focusing |
| Core logic | **Rust** | Ingest server, session registry, TTS, approval broker, focus service — all in `src-tauri` |
| UI (overlay + board) | **Svelte 5 + Vite** | Compiles framework away; overlay bundle stays tiny for an always-resident window; UI is deliberately dumb (render state, emit commands) |
| Toolchain / package mgr / test runner (JS side) | **Bun** | Dev-time only — never ships. `bun install`, `bun run dev/build`, `bun test` |
| Hook shim | **Rust static binary** (`pingmybell-shim`) | Spawned on every agent event; needs <10 ms startup and zero runtime deps. NOT Bun/Node — a JS runtime here would add 50–90 MB or a user-installed dependency |
| TTS | **`tts` crate** → AVSpeechSynthesizer (macOS) / WinRT `Windows.Media.SpeechSynthesis` (Windows) | Free, offline, unlimited; behind a `Speaker` trait so Piper/Kokoro can be added later |
| Persistence | **SQLite** (`rusqlite`) | Session registry survival, event history, settings |
| Project site (phase 2) | **Astro + Starlight** in `site/` | Docs/landing only — never part of the app |

## 2. Component diagram

```
┌────────────────────────────────────────────────────────────────┐
│ PingMyBell (Tauri v2, single process)                          │
│                                                                │
│  Rust core (src-tauri)                Webview UI (Svelte)      │
│  ┌──────────────────────┐   Tauri     ┌──────────────────┐     │
│  │ ingest: axum HTTP    │   events/   │ Overlay window   │     │
│  │  127.0.0.1:<rand>    │◄──commands─►│  (notch / pill)  │     │
│  │ registry: sessions   │             ├──────────────────┤     │
│  │ broker: pending      │             │ Board window     │     │
│  │  approvals (oneshot) │             │  (on demand)     │     │
│  │ speaker: TTS queue   │             └──────────────────┘     │
│  │ focus: term switcher │   ┌─────────────────┐                │
│  │ tray + autostart     │   │ SQLite          │                │
│  └─────────▲────────────┘   └─────────────────┘                │
└────────────┼───────────────────────────────────────────────────┘
             │ HTTP POST, loopback only, Bearer token
   ┌─────────┴─────────────┬──────────────────────┐
   │ pingmybell-shim       │ pingmybell-shim      │
   │ (Claude Code hooks)   │ (Codex notify)       │
   └───────────────────────┴──────────────────────┘
```

## 3. Repository layout

```
pingmybell/
├─ PRD.md / ARCHITECTURE.md / CLAUDE.md
├─ src-tauri/
│  ├─ src/
│  │  ├─ main.rs            # app bootstrap, tray, window creation
│  │  ├─ ingest.rs          # axum server, auth, payload validation
│  │  ├─ registry.rs        # session state machine + SQLite sync
│  │  ├─ broker.rs          # pending PreToolUse map: id → oneshot channel
│  │  ├─ speaker.rs         # Speaker trait, OS TTS impl, queue, templates
│  │  ├─ focus/             # mod.rs, macos.rs (AppleScript), windows.rs (Win32/UIA), tmux.rs
│  │  ├─ adapters/          # mod.rs (trait), claude_code.rs, codex.rs
│  │  └─ platform/          # macos.rs (NSWindow level, notch geometry via objc2), windows.rs (WS_EX_* styles)
│  └─ tauri.conf.json
├─ src/                     # Svelte UI
│  ├─ overlay/              # idle sliver, toast, approval card
│  ├─ board/                # session list, history drawer, settings
│  └─ lib/                  # shared stores fed by Tauri events
├─ shim/                    # separate tiny cargo crate → pingmybell-shim
├─ installers/              # settings.json / config.toml merge+uninstall logic (Rust, called from app)
├─ site/                    # (phase 2) Astro + Starlight
├─ package.json / bun.lock
└─ .github/workflows/ci.yml # bun + cargo checks; tauri-action release matrix (macos-14 universal, windows-latest)
```

## 4. Wire protocol (shim → core)

Discovery: shim reads `~/.pingmybell/port` and `~/.pingmybell/token` (user-only perms, written by app at startup). If either is missing or the POST fails → exit 0 silently (fail-open).

- `POST /v1/event` — fire-and-forget events. Body: normalized event (below). Responds 202.
- `POST /v1/approval` — blocking long-poll for PreToolUse. Responds when the user decides, or 204 on broker timeout (110 s, safely inside the 120 s hook timeout).
- Auth: `Authorization: Bearer <token>` on every request.

Normalized event:

```jsonc
{
  "agent": "claude-code" | "codex",
  "event": "session_start" | "turn_complete" | "needs_attention" | "permission_request" | "session_end",
  "session_id": "string",             // agent-native id (claude session_id / codex thread-id)
  "cwd": "/abs/path",
  "summary": "string|null",           // raw text; core does cleanup/truncation
  "transcript_path": "string|null",   // claude-code only
  "tool": { "name": "Bash", "input": { } } | null,   // permission_request only
  "terminal": {                        // captured by shim at session_start
    "pid": 123, "ppid_chain": [..], "tty": "/dev/ttys003",
    "tmux_pane": "%5|null", "term_program": "iTerm.app|null", "hwnd": "0x..|null"
  } | null
}
```

## 5. Adapters

### 5.1 Claude Code (`adapters/claude_code.rs` + installer)

Installed into `~/.claude/settings.json` (merge, never overwrite; keyed so uninstall can remove exactly our entries):

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "<shim> claude session-start" }] }],
    "Stop":         [{ "hooks": [{ "type": "command", "command": "<shim> claude stop" }] }],
    "Notification": [{ "hooks": [{ "type": "command", "command": "<shim> claude notification" }] }],
    "SessionEnd":   [{ "hooks": [{ "type": "command", "command": "<shim> claude session-end" }] }],
    "PreToolUse":   [{ "matcher": "Bash|Write|Edit|MultiEdit",
                       "hooks": [{ "type": "command", "command": "<shim> claude pretool", "timeout": 120 }] }]
  }
}
```

Shim behavior per subcommand: read hook JSON from stdin → map to normalized event → POST. For `stop`, the hook payload carries `last_assistant_message` directly (verified against claude 2.1.198 on 2026-07-30 by dumping live hook payloads; common fields `session_id`, `cwd`, `transcript_path`, `hook_event_name` also confirmed) — the shim sends it as `summary`; when absent, the core falls back to reading `transcript_path` for the last assistant message. Cleanup (strip markdown/code/paths, first sentence ≤ 220 chars) always happens in the core. For `pretool`, shim POSTs `/v1/approval` and blocks; on user decision prints to stdout:

```json
{ "hookSpecificOutput": { "hookEventName": "PreToolUse",
    "permissionDecision": "allow" | "deny" | "ask",
    "permissionDecisionReason": "PingMyBell: <reason>" } }
```

On 204/timeout/error: print nothing, exit 0 → Claude Code falls through to its own prompt.

> ⚠️ Verify hook event names and output schema against the live docs (https://code.claude.com/docs/en/hooks) and the installed `claude --version` before finalizing the shim — the schema evolves. Treat this file's JSON as the design, the docs as the authority.

### 5.2 Codex CLI (`adapters/codex.rs` + installer)

`~/.codex/config.toml`: `notify = ["<shim path>", "codex"]`. Codex invokes the shim with one JSON argument; event `agent-turn-complete` carries `type`, `thread-id`, `turn-id`, `cwd`, `input-messages`, `last-assistant-message` → normalized `turn_complete` with `summary = last-assistant-message`. No blocking hooks exist → no approval support; do not touch `tui.notifications`.

### 5.3 Adapter trait

```rust
trait Adapter {
    fn id(&self) -> AgentKind;
    fn install(&self) -> Result<InstallReport>;   // merge config, idempotent
    fn uninstall(&self) -> Result<()>;            // remove only our entries
    fn normalize(&self, raw: serde_json::Value) -> Result<Event>;
}
```

Future adapters (Cursor, generic tmux idle-watcher) implement this without core changes.

## 6. Core services

- **Registry** (`registry.rs`): `HashMap<SessionId, Session>` + SQLite write-through. Session state machine: `Working → NeedsAttention | Done → Working…`; emits `session-updated` Tauri events consumed by both windows. Recovery: on startup, load sessions younger than 24 h, mark `Unknown` until next event.
- **Broker** (`broker.rs`): `DashMap<ApprovalId, oneshot::Sender<Decision>>`. Ingest inserts + parks the HTTP response on the receiver with `tokio::time::timeout(110 s)`; UI command `decide(approval_id, decision)` completes it. Timeout → 204. Preempts speaker queue with an announcement on insert.
- **Speaker** (`speaker.rs`): `trait Speaker { fn enumerate(&self) -> Vec<Voice>; fn speak(&self, u: Utterance) -> Result<()>; fn stop(&self); }`. One background task drains a priority queue (approvals > attention > completions). Template engine: `{agent}`, `{project}`, `{summary}`, `{tool}` placeholders; 3 built-in styles. Per-session 5 s dedup.
- **Focus** (`focus/`): strategy chain per session: tmux pane → platform terminal focus → no-op with UI feedback. macOS via `osascript` (Terminal/iTerm2/WezTerm scripts selected by `term_program`); Windows via `SetForegroundWindow`/`SwitchToThisWindow` on stored HWND, then best-effort UIA tab selection for Windows Terminal.

## 7. Overlay windows (platform notes)

Both platforms: Tauri window with `decorations:false, transparent:true, alwaysOnTop:true, skipTaskbar:true, focusable:false`, positioned top-center of the display containing the most recent event's terminal (fallback: primary).

- **macOS** (`platform/macos.rs`, ~30 lines of `objc2`): raise `NSWindow.level` to `.statusBar+1`; `collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]`; notch geometry from `NSScreen.safeAreaInsets` (insets.top > 0 ⇒ notch present; width from `auxiliaryTopLeftArea`/`auxiliaryTopRightArea` gap). Idle width = notch width; expand animation handled in Svelte (CSS), window resized via Tauri API.
- **Windows** (`platform/windows.rs`): apply `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` via `SetWindowLongPtrW` after creation; keep topmost with `HWND_TOPMOST`. Buttons must handle clicks without window activation (webview receives mouse input regardless; verify no focus theft with a foreground terminal — this is an explicit test case).

## 8. Data model (SQLite)

```sql
sessions(id TEXT PK, agent TEXT, cwd TEXT, title TEXT, state TEXT,
         terminal_json TEXT, started_at INT, last_event_at INT);
events(id INTEGER PK, session_id TEXT, kind TEXT, summary TEXT,
       decision TEXT NULL, created_at INT);
settings(key TEXT PK, value_json TEXT);   -- voice map, templates, mute, autostart
```

## 9. Security & privacy invariants (enforce in code review)

1. Ingest binds loopback only; token compared constant-time; files in `~/.pingmybell/` are 0600.
2. Shim: any failure path = exit 0, no stdout. Never log transcript content; log event kinds only.
3. No outbound network in MVP except optional, user-enabled updater (GitHub Releases).
4. Spoken/preview text is derived data; raw transcripts are read from disk on demand, never stored in our DB.

## 10. Build order (dependency-driven, each step independently verifiable)

1. **Spine**: Tauri app + tray + ingest server + token/port files + SQLite. ✔ `curl` a fake event → row in DB.
2. **Claude Code happy path**: shim (session-start/stop) + installer + speaker with OS TTS. ✔ real `claude` run speaks a completion on both OSes.
3. **Overlay**: window + platform styles + idle/toast states wired to registry events. ✔ completion shows toast, no focus stolen.
4. **Approvals**: broker + `/v1/approval` + pretool shim subcommand + approval card + decision voicing. ✔ approve a real bash command from the overlay; let one time out → terminal prompt appears.
5. **Codex adapter**: installer + normalizer. ✔ codex turn speaks.
6. **Board + focus**: session list, history, jump-to-session (tmux → AppleScript → Win32). ✔ click focuses correct window on both OSes.
7. **Polish**: settings UI, voice picker, autostart, updater, CI release artifacts (`tauri-action`; unsigned OK for dev builds, universal macOS binary).

Testing notes: unit-test normalizers and template engine with recorded fixture payloads (`bun test` for UI stores, `cargo test` for core); integration script that replays fixture events against a running app; manual matrix in §7 of PRD (success criteria).
