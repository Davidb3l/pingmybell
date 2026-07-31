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

### Next session

- Step 4: Approvals — broker + `/v1/approval` long-poll + `pretool` shim
  subcommand + approval card in overlay (needs cursor events re-enabled and
  focus-safe click handling — verify no activation on click, esp. Windows
  WS_EX_NOACTIVATE button behavior) + decision voicing. Gate: approve a real
  bash command from the overlay; let one time out → terminal prompt appears.
