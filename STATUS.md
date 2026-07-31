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

### Next session

- Step 3: Overlay — window + platform styles (macOS notch geometry, never
  steal focus) + idle/toast states wired to registry events. Also sanity-check
  board UI rendering under the CSP set in step 1.
