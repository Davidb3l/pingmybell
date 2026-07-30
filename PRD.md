# PingMyBell — Product Requirements Document

> **Handoff note for the implementing agent:** This PRD defines *what* to build. `ARCHITECTURE.md` (same directory) defines *how* — stack, schemas, event flows, and build order. Read both before writing code. Where this document and ARCHITECTURE.md conflict, ARCHITECTURE.md wins on technical detail; this document wins on scope.

## 1. One-liner

A free, open-source, cross-platform (macOS + Windows) desktop app that gives AI coding agents distinct voices, speaks a summary aloud when they finish or need attention, and provides a notch-style overlay to approve/deny agent actions and jump between sessions — with zero cloud dependency.

## 2. Problem

Developers run multiple AI coding agents (Claude Code, Codex CLI) in parallel terminals. Agents finish or block on permission prompts while the developer is looking elsewhere; sessions sit idle for minutes. The existing solution (Agent Callout) is macOS-only, subscription-priced, and routes spoken summaries through a cloud TTS service.

## 3. Goals

1. Instant, audible awareness of agent state changes — completion, waiting-for-input, permission request — with a recognizable voice per agent.
2. Act on agent prompts (approve/deny) without switching windows, from an always-on-top overlay.
3. See all live sessions at a glance and jump to any session's terminal in one click.
4. 100% free and offline: OS-native TTS, no accounts, no telemetry, no network calls beyond loopback.
5. Single codebase shipping native installers for macOS 13+ (universal binary) and Windows 10 1903+.

## 4. Non-goals (MVP)

- No cloud TTS, no LLM-generated summaries (template-based summaries only).
- No Linux build (architecture must not preclude it; Tauri makes it near-free later).
- No Cursor/Pi/Antigravity adapters (adapter trait must make them addable without core changes).
- No free-form reply injection into terminals (phase 2 — requires Accessibility/SendInput; design for it, don't build it).
- No approval support for Codex (it has no blocking hook; notification-only).
- No mobile/remote notifications.

## 5. Functional requirements

Each requirement lists acceptance criteria (AC). "Agent" below means a monitored CLI agent instance (one session).

### FR-1 — Agent event ingestion
The app runs a loopback-only HTTP server that receives events from hook shims installed into Claude Code and Codex CLI.
- AC-1.1: Server binds `127.0.0.1` on a random free port; port and a per-install bearer token are written to `~/.pingmybell/` with user-only permissions.
- AC-1.2: Requests without the token are rejected 401; server never binds a non-loopback interface.
- AC-1.3: A malformed or unknown payload is logged and dropped — never crashes the server.

### FR-2 — Claude Code integration
- AC-2.1: A one-click "Install Claude Code integration" action merges hook entries (SessionStart, Stop, Notification, PreToolUse, SessionEnd) into `~/.claude/settings.json`, preserving all existing user hooks and settings (parse → merge → write; never blind-overwrite). An uninstall action removes only our entries.
- AC-2.2: On `Stop`, a voice callout plays within 1.5 s containing agent name, project name, and a one-sentence summary derived from the last assistant message.
- AC-2.3: On `Notification` (permission/idle prompt), the overlay shows a "needs attention" state and a voice callout plays.
- AC-2.4: The shim ALWAYS exits 0 with no output on any internal error or if the app is not running (fail-open — a broken/absent PingMyBell must never block or alter agent behavior).

### FR-3 — Codex CLI integration
- AC-3.1: One-click install writes `notify = [<shim path>, "codex"]` into `~/.codex/config.toml`, preserving existing config.
- AC-3.2: `agent-turn-complete` produces a voice callout with project (from `cwd`) and summary (from `last-assistant-message`), same latency budget as AC-2.2.

### FR-4 — Voice engine
- AC-4.1: TTS uses OS-native synthesis (AVSpeechSynthesizer on macOS, WinRT SpeechSynthesis on Windows) — no network, no bundled models.
- AC-4.2: Settings map each agent type to a voice + rate + volume, chosen from enumerated system voices with sensible distinct defaults.
- AC-4.3: Callout text comes from user-selectable templates (terse / conversational / status-only). Summaries strip markdown, code blocks, and paths; cap at 220 characters.
- AC-4.4: Speech queue: one utterance at a time; permission requests preempt completion callouts; per-session dedup window of 5 s; global mute toggle in tray.

### FR-5 — Overlay ("notch" command center)
- AC-5.1: A frameless, transparent, always-on-top, non-activating window sits at top-center of the display. It NEVER steals keyboard focus.
- AC-5.2: macOS: sits flush with the notch/menu-bar area, visible on all Spaces and over full-screen apps; idle width ≈ notch width, expanding on events. Non-notch Macs: floating pill below the menu bar.
- AC-5.3: Windows: floating top-center pill; hidden from Alt-Tab and taskbar.
- AC-5.4: States: idle (session-count dots) → toast (event summary, auto-collapse after 6 s) → pinned approval (persists until resolved or hook timeout).
- AC-5.5: Idle CPU usage ~0% (no polling loops; all updates event-driven).

### FR-6 — Approve/deny from overlay (Claude Code only)
- AC-6.1: A `PreToolUse` event renders tool name + primary input (e.g. the bash command) in the overlay with Approve / Deny / Ask buttons within 500 ms of hook fire.
- AC-6.2: Approve resolves the blocked hook with `permissionDecision: "allow"`, Deny with `"deny"` + reason, Ask with `"ask"` (escalates to terminal prompt).
- AC-6.3: If the user does nothing, the shim exits cleanly before the configured hook timeout (120 s) with no decision, and Claude Code falls back to its normal terminal prompt. Under no circumstance can the overlay strand a session.
- AC-6.4: Every decision is voiced ("Approved bash command in api-server") and logged to session history.

### FR-7 — Session board
- AC-7.1: A main window lists live sessions: agent icon, project (cwd basename), state (working / waiting on you / done), last summary, elapsed time in state.
- AC-7.2: Board updates in real time from the session registry; closing the window does not stop monitoring (app lives in the tray).
- AC-7.3: Clicking a session focuses its terminal window (see FR-8); session history (last 50 events per session) viewable per session.

### FR-8 — Jump to session
- AC-8.1: tmux sessions: focus via `tmux switch-client` / `select-window` using pane id captured at SessionStart (both platforms).
- AC-8.2: macOS: focus the owning window/tab in Terminal.app, iTerm2, or WezTerm via AppleScript, using the tty recorded at SessionStart. First use triggers the Automation permission prompt; denial degrades gracefully (button shows tooltip, everything else works).
- AC-8.3: Windows: focus the owning console window via HWND recorded at SessionStart; Windows Terminal tab-level focus is best-effort via UIA, window-level focus is the guaranteed baseline.

### FR-9 — App shell
- AC-9.1: Tray/menu-bar icon with: open board, mute, per-agent volume, launch at login toggle, install/uninstall integrations, quit.
- AC-9.2: Settings persisted locally; survive restart; no first-run account/network step.
- AC-9.3: Auto-update via Tauri updater against GitHub Releases, user-confirmable, off by default in MVP builds.

## 6. Non-functional requirements

- **NFR-1 Latency**: hook fire → audible speech start ≤ 1.5 s; hook fire → overlay render ≤ 500 ms.
- **NFR-2 Footprint**: total RSS ≤ 150 MB with 5 active sessions; installer ≤ 25 MB; shim binary ≤ 1.5 MB with <10 ms startup.
- **NFR-3 Privacy**: only network activity is loopback HTTP and (if enabled) the GitHub updater check. Transcripts never leave disk; no analytics of any kind.
- **NFR-4 Robustness**: shims fail open (AC-2.4); app crash must not affect running agent sessions; ingest server restarts recover session state from SQLite.
- **NFR-5 Licensing**: MIT (or Apache-2.0); all dependencies permissive; no GPL in shipped artifacts.

## 7. Success criteria for MVP

Running `claude` in two terminals and `codex` in a third on both a MacBook Pro (notch) and a Windows 11 machine: all three sessions appear on the board; each completion speaks in a distinct voice within budget; a Claude Code bash permission request is approved from the overlay without touching the terminal; killing PingMyBell mid-session leaves all agents fully functional.

## 8. Scope sequencing

MVP = FR-1 → FR-9 in the build order defined in ARCHITECTURE.md §10. Phase 2 (explicitly out of MVP): reply-to-agent text injection, Cursor + generic tmux-watcher adapters, optional Piper/Kokoro local neural voices behind the existing `Speaker` trait, Linux build, project website (Astro + Starlight in `site/`).
