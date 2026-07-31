# STATUS

Session log and next-session notes. Durable rules live in CLAUDE.md.

## 2026-07-30 — Step 1 (spine) complete

- Repo initialized, crate names `pingmybell` / `pingmybell-shim` reserved on
  crates.io at v0.0.1 (placeholders; nothing further published — local only
  from here).
- Spine implemented per ARCHITECTURE.md §10 step 1: Tauri v2 app (tray icon,
  hidden board window, macOS Accessory policy), axum ingest server on
  127.0.0.1:<random> with bearer auth (constant-time compare), port/token
  written 0600 to `~/.pingmybell/` (dir 0700, SQLite files also forced 0600),
  rusqlite schema from §8, registry with state machine + write-through +
  24 h `unknown` recovery on restart.
- Verification gate passed on macOS: valid `session_start`/`turn_complete`
  POSTs → 202 + correct sessions/events rows; bad token → 401; malformed and
  schema-invalid bodies → 400 with server staying up; restart marks sessions
  `unknown`, next event revives them.
- Frontend is a minimal Svelte 5 board (renders `session-updated` events);
  `bun run build` output is embedded via `frontendDist` — `bun run tauri dev`
  builds the frontend first (no HMR devUrl yet; revisit when UI work starts
  in step 3).
- Tray icon is a generated placeholder (amber circle); replace with real art
  before release.

## 2026-07-30 (later) — Step 2 (Claude Code happy path) complete

- Hook schemas verified against live docs AND empirically against installed
  claude 2.1.198 (dumped real payloads): Stop carries `last_assistant_message`
  directly — shim sends it as summary, core keeps a bounded-tail (256 KB)
  transcript fallback. ARCHITECTURE §5.1 updated.
- Real shim (`shim/`): claude subcommands session-start/stop/notification/
  session-end; raw-TcpStream HTTP POST (serde_json + libc only, ~560 KB
  release, ~3 ms startup); fail-open verified empirically (bad/huge/binary
  stdin, app down, panics — always exit 0, no stdout). Notifications with
  benign `notification_type` (e.g. auth_success) are dropped; absent type errs
  toward speaking. To verify interactively later: real Notification payload
  shape + whether Notification matchers work in settings.json.
- Installers crate (`installers/`, publish=false): parse→merge→write into
  ~/.claude/settings.json, atomic + symlink-resolving writes, keeps first
  backup as pristine snapshot, uninstall removes exactly our entries.
  App CLI: `pingmybell install-claude` / `uninstall-claude`; also tray items.
- Speaker (`speaker.rs`): dedicated thread over the `tts` crate
  (AVFoundation), priority queue (approval > attention > completion),
  per-(session,priority) 5 s dedup, mute in tray, distinct default voices
  (Samantha/Daniel), panic-contained TTS calls. Summaries cleaned in core
  BEFORE store/speak (≤220 chars, markdown/paths stripped) — DB never holds
  raw assistant text (§9.4).
- Shim ships as Tauri sidecar (`externalBin` + `bun run sidecar` staging;
  wired into beforeDev/BuildCommand and CI). Full `tauri build` bundle not yet
  exercised — verify in step 7.
- Gate passed on macOS: real `claude -p` run (via --settings) fired hooks →
  session_start/turn_complete/session_end rows → spoke "Claude finished in
  realgate. The voice pipeline works." cargo test 28/28 green.
- NOT yet done: hooks not installed into the real ~/.claude/settings.json
  (permission classifier blocked the write from the agent session) — the user
  runs `./target/debug/pingmybell install-claude` themselves.

## 2026-07-31 — Step 3 (overlay) complete

- Overlay window (second webview entry `overlay.html`, vite multi-page):
  frameless/transparent/always-on-top/skip-taskbar/focusable:false,
  click-through in this step (`set_ignore_cursor_events(true)` — flip off
  when approvals land in step 4).
- macOS (`platform/macos.rs`, objc2 msg_send): window level 26 (menu bar +1),
  canJoinAllSpaces | fullScreenAuxiliary, notch probe via NSScreen
  safeAreaInsets + auxiliary areas (respondsToSelector-guarded; bundle
  minimumSystemVersion 13.0). `macOSPrivateApi: true` + `macos-private-api`
  feature REQUIRED for transparency — silently opaque without it.
  Windows (`platform/windows.rs`): WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE +
  HWND_TOPMOST; compile-verified against windows 0.61 for msvc in isolation
  (full workspace msvc check impossible on macOS — libsqlite3-sys needs cl).
- Overlay controller (`overlay.rs`): Rust owns idle⇄toast states; seq-guarded
  6 s collapse timers; window geometry serialized via a window-ops lock and
  always derived from current mode; `on_page_load` replays state to the
  late-loading webview; overlay init failure degrades to voice-only.
- Gate passed on real notch hardware (179 pt notch, 32 pt inset): idle
  179×48 flush top-center at layer 26 (above menu bar 25), toast 480×78,
  frontmost app unchanged through the whole cycle (lsappinfo), collapse at
  exactly 6 s. Window geometry verified via CGWindowList; screenshot-based
  visual check not possible (terminal lacks Screen Recording permission) —
  user sees it live. cargo test 31/31.

## 2026-07-31 (later) — Step 4 (approvals) complete, one interactive check open

- Broker (`broker.rs`): Mutex<HashMap> of oneshot senders (functionally the
  §6 DashMap); register/decide/expire with race-safe semantics + unit tests.
- `/v1/approval` (`ingest.rs`): auth → registry apply (permission_request,
  exact event row id captured) → broker register → overlay pin → preempting
  voice announcement → park via select! with 110 s timeout
  (PINGMYBELL_APPROVAL_TIMEOUT_SECS override for tests) + 50 ms grace for
  the click-vs-timeout race → 200 {"decision"} or 204. An RAII
  ApprovalCleanup guard expires+unpins on future CANCELLATION too (shim
  connection death) — verified live by kill -9 mid-park; without it a
  clickable card strands over the notch forever. Voice-only mode (overlay
  init failed) answers 204 immediately instead of stalling agents.
- Shim `pretool`: POSTs permission_request, blocks (read timeout 115 s
  inside the 120 s hook timeout), prints hookSpecificOutput JSON on a real
  decision ONLY; 204/garbage/errors → silent exit 0. parse_decision
  unit-tested against adversarial responses.
- `decide` Tauri command (async — sync would run on the main thread and
  risk deadlock against window-ops getters): broker → record_decision on
  the exact event row (resume-to-Working only when no sibling approvals
  pending) → voice decision → unpin. Stale-card clicks defensively unpin.
- Overlay: approval mode > toast > idle; queue badge (+N more); cursor
  events enabled ONLY while approvals are pinned; acceptFirstMouse on.
- Installer: PreToolUse entry matcher "Bash|Write|Edit|MultiEdit",
  timeout 120. NOTE: the user must re-run install-claude to get it.
- KNOWN DESIGN CONSEQUENCE (review finding, deliberate): PreToolUse fires
  before Claude Code's own permission evaluation, so EVERY matched tool
  call shows a card / waits — including allowlisted commands that used to
  run instantly. Revisit in step 7 (matcher config, per-session auto-allow,
  shorter park). Do not forget.
- Gate FULLY passed: timeout path verified with a real headless claude run
  (hook → card pinned 110 s → clean fallback), and the interactive path
  verified live — the user clicked Approve on the overlay card for a real
  Bash PreToolUse in an active session; decide → 200 → shim JSON → command
  ran immediately; decision row recorded and voiced. cargo test 34/34,
  svelte-check clean.
- POST-SHIP BUG (fixed same session): the approval card rendered as a blank
  black box in production — dist/ still held the step-3 bundle because
  `bun run build` was skipped after the Svelte changes; the compiled app
  embeds dist at build time. LESSON: after ANY src/ (frontend) change,
  run `bun run build` BEFORE `cargo build`, or the binary ships a stale UI.
  CI is immune (builds frontend first); local manual flows are not.
- AC-5.1 formal focus check around a real overlay click still pending
  (user-reported no issues; measure lsappinfo around a click in step 7).

## 2026-07-31 (later) — Feedback round: flow-through gating, hover island, design pass

- USER FEEDBACK (durable, also in agent memory): David works in auto mode —
  tool calls must flow with ZERO added latency; the valuable moments are only
  when Claude genuinely asks a decision. Gating everything was wrong.
- `~/.pingmybell/config.json` (written by app, read by shim per invocation):
  `gate_tool_calls`, DEFAULT FALSE. Tray toggle "Approve Tool Calls From
  Overlay". When off, `pretool` exits in ~1 ms (no park, no card). When on,
  the shim also skips bypassPermissions entirely and Write/Edit/MultiEdit
  under acceptEdits (the mode auto-approves those anyway).
- Ask-moments are now PINNED attention cards (not 6 s toasts): a
  needs_attention event (permission/idle/elicitation notification) pins an
  amber card that persists until the session's next event or ✕ dismiss
  (`dismiss_attention` command). Verified: pinned at 500×96, survives past
  toast lifetime, cleared by turn_complete.
- Hover: the island is now always cursor-interactive (still never focusable).
  Hovering expands the idle sliver into a session list (Rust-computed rows:
  state light, agent tag, title, state, age; capped 6) via `overlay_hover`
  command; collapses 300 ms after mouseleave. Precedence:
  approval > attention > toast > hover-expanded > idle.
- Overlay redesigned ("precision instrument" language): pure-black surface,
  hairline bezel highlights, glowing status lights, mono uppercase
  micro-labels, springy notch-origin rise animations, amber brand accent,
  Approve as the amber primary button. Board window untouched (its design
  pass belongs to step 6).
- Hover + visuals verified by the user on-screen; state machine + geometry
  verified via CGWindowList. cargo test 37/37, svelte-check clean.

## 2026-08-01 — Hover/monogram/morph polish, jump slice, step 5 (Codex) built

- Island polish round (user feedback): OS-level NSEvent mouse monitors for
  hover entry (never-key windows get no AppKit hover events; handler stays
  off window_ops — main-thread deadlock rule), native same-space cursor
  containment (NSEvent.mouseLocation vs NSWindow.frame — tauri's cursor API
  mixes coordinate spaces on retina), shell-morph animation (window snaps
  invisibly, shell springs 260 ms; shrinks deferred past the animation),
  bell monogram (idle + expanded header with live counts), monochrome
  template tray icon (macOS; Windows keeps color).
- Step 6 first slice: expanded-island rows are buttons → focus_session →
  tmux pane (if any) → walk process tree from shim's recorded ppid to the
  first NSRunningApplication and activate it. Works for ANY host app (user
  runs sessions inside the Claude desktop app — hooks see no tty, so the
  AppleScript-by-terminal plan from §6 was a dead end for this case). No
  permissions needed. Verified live by the user. Remaining step 6: board
  window redesign + history, in-app tab selection, Windows focus.
- Step 5 (Codex): shim `codex` mode — JSON as single argv (stdin fallback),
  kebab/snake key tolerant, agent-turn-complete → turn_complete with
  terminal capture at that moment (no session-start exists); other notify
  types dropped. installers/codex.rs: toml_edit-preserving
  `notify = ["<shim>", "codex"]`, refuses to clobber a foreign notify,
  first-backup kept; CLI install-codex/uninstall-codex + tray items.
  Shim-level gate passed (fake agent-turn-complete → row + toast + spoken
  callout in Daniel's voice). REAL codex gate pending: user is installing
  Codex now — after `pingmybell install-codex`, a real codex turn must
  speak. Note: notify docs verified only at shape level (payload keys not
  in public reference; tolerant parsing covers drift). cargo test 45/45.

### Next session

- Confirm real-codex gate (turn speaks; check payload keys in practice).
- Step 6 remainder: board window redesign + per-session history drawer,
  in-app tab selection best-effort, Windows focus. Then step 7 polish.
