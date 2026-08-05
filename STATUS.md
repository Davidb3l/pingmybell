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

## 2026-08-01 (later) — Real-codex gate passed; step 6 board complete

- Real-codex gate CONFIRMED in production: ChatGPT-app Codex sessions ring
  the bell through the chained notify (their SkyComputerUseClient keeps
  working). Two field bugs found via user reports and fixed: payload ids
  rotate PER TURN (sessions now keyed by cwd hash), and identical-text
  callouts within 10 s are muted. cwd "/" titles fall back to agent name.
  Island rows/toasts carry agent glyphs (evocative, not trademarked marks).
- Step 6 board: redesigned in the island's instrument language — sticky
  header (monogram + live counts), session cards with state lights, agent
  tag+glyph, title, latest summary, age, ↗ jump button, and an expandable
  per-session history drawer (last 50 events, newest first, decision badges;
  board_snapshot/session_history commands). Board re-pulls the snapshot on
  each session-updated so summaries stay fresh; 60 s tick only while the
  window is open. Rows are keyboard-accessible (button-in-button was a vite
  build error — .row is a div[role=button]).
- PROCESS LESSONS (twice bitten): (1) piping build/test output through
  grep/tail masks failures — keep the failing command unpiped in && chains;
  (2) direct sqlite DELETEs are invisible to the running app's in-memory
  registry — restart the app after DB surgery or rows ghost on the board.
- Step 6 remaining (deferred): in-app tab selection best-effort, Windows
  focus implementation (compile-gated), per-terminal display tracking.

## 2026-08-01 (later) — Step 7 shipped: PingMyBell.app installed

- First real `tauri build`: PingMyBell.app (16 MB) + 5.4 MB dmg (NFR ≤25 MB),
  sidecar shim bundled next to the binary, installed to /Applications and
  running (debug binary retired). Unsigned local build — fine without
  quarantine; signing/notarization only matters for distribution.
- Launch at Login tray toggle (tauri-plugin-autostart, LaunchAgent).
- Board settings panel (gear): per-agent voice pickers (list_voices/
  get_settings/set_voice; persisted in config.json, applied per-utterance,
  sample spoken on pick — AC-4.2) + gating toggle. Tray checkbox for gating
  reflects config only at launch (known cosmetic drift).
- USER ACTION after install: re-run install-claude / install-codex from the
  .app CLI so hooks point at the bundled shim instead of target/debug.
- MVP remainder (deliberate): updater (off by default per AC-9.3), CI
  release matrix (tauri-action, universal macOS), callout template styles
  (terse only), Windows focus + tab selection, gate_tool_calls matcher UX.

## 2026-07-31 — Answer-from-the-banner + scrollable island

- USER REPORT that started it: a live Codex session was missing from the
  hover list, and a Claude question (AskUserQuestion) in the desktop app
  could not be opened from the island. Both diagnosed, both real:
  - Missing session = `EXPANDED_MAX_ROWS = 6` truncating silently (the row
    was 7th). NOT a Codex bug. Note for future debugging: ended sessions
    can never flood the island — `Registry::apply` removes them from the
    live map (registry.rs:284), so DB history rows never reach it.
  - Question never arrived because the installed PreToolUse matcher was
    `Bash|Write|Edit|MultiEdit` — AskUserQuestion was not matched at all.
- KEY EMPIRICAL FINDING (claude 2.1.198, dumped through a pty; NOT in the
  public hook docs — written up in ARCHITECTURE §5.1.1):
  1. PreToolUse DOES fire for `AskUserQuestion`, carrying the full
     `questions[]` array (question, header, options[label+description],
     multiSelect) — everything a card needs.
  2. A hook can ANSWER it by returning `permissionDecision: "deny"` with the
     user's choice in `permissionDecisionReason`. Verified end to end: the
     TUI selector never rendered, and Claude proceeded on the injected
     answer. No tmux, no tty, no macOS permission — so this works for
     sessions hosted in the Claude desktop app.
  This killed the original plan (migrate to terminal/tmux sessions and use
  `send-keys`). tmux is now needed only for Codex, which has no hook.
- Shipped: `/v1/question` endpoint + broker question support + shim question
  path (ignores `gate_tool_calls` — a question is already blocking the user;
  fail-open unchanged), installer matcher now includes AskUserQuestion,
  `Display::Question` overlay card with option buttons, and a separate
  FOCUSABLE `reply` window (reply.rs + Reply.svelte) for free text, since
  the island may never take the keyboard (AC-5.1).
- Island list: `EXPANDED_MAX_SESSIONS = 40` delivered, `EXPANDED_VISIBLE_ROWS
  = 8` bounds the window, list scrolls inside `list_max`. Verified with 18
  live sessions that the wheel reaches the non-focusable window and
  `lsappinfo front` never changes.
- `tmux.rs` (for the Codex path): TWO REAL BUGS found by measuring, not
  reviewing — (1) `send-keys -l` is NOT inert: an embedded newline EXECUTES
  what precedes it, so `send_literal` now refuses all C0 controls; (2) tmux
  silently truncates a trailing `;`, now escaped. Also `PaneTrust`: pane ids
  restart at %0 after a tmux server restart, so a RECORDED pane id can be a
  stranger's — require `Verified` before typing an answer; `Recorded` is
  only good enough for jump-to-session. tmux stays OPTIONAL (`available()`
  gates everything); it was installed on this machine for testing only.
- `focus_session` now runs `focus::jump` under `spawn_blocking` — it shells
  out (tmux + up to 24 `ps`) and was blocking a Tokio worker, which is the
  same runtime agents are parked against.
- cargo test 101 green (66 core + 15 installers + 20 shim), svelte-check
  clean, `bun run build` run before every binary build.

## 2026-07-31 (later) — Codex CAN be answered too; review fixes shipped

- FRESH-EYES REVIEW found 2 release-blocking bugs, both in the free-text
  reply window (the one path not verified live before shipping):
  (1) `reply` was missing from `src-tauri/capabilities/default.json`
  `windows`, so the webview's `listen()` was ACL-rejected and the window
  could never receive a prompt — it would open with a dead textarea;
  (2) it rendered BEHIND the question card (NSFloatingWindowLevel 3 vs the
  island's 26) and was click-blocked by it. Both fixed (capability entry +
  `apply_reply_styles` at level 27, placement moved onto the main thread
  because it reads NSScreen). Plus: reply window now closes when its
  question expires/defers (it used to strand and silently eat typing),
  submit emits BEFORE clearing state, stale submits are rejected on
  (id, question_index) not id alone, and option lists are keyed by index
  (duplicate labels after 200-char truncation would have failed the card).
- CODEX HOOKS CONFIRMED (codex-cli 0.146.0-alpha.9.2, bundled in
  ChatGPT.app, NOT on PATH). Codex has Claude-style hooks; a PreToolUse
  hook on `request_user_input` returning deny+reason DOES deliver the
  answer — verified in an isolated CODEX_HOME: one turn, no re-ask,
  `FINAL_ANSWER=PMB_CANARY_OPTION_B`. So tmux is NOT needed for Codex
  either; the tmux module stays only as a terminal-session convenience.
  Deltas from Claude: each question carries a required `id`, there is NO
  `multiSelect`, `description` is required, and Codex WRAPS the reason as
  `Tool call blocked by PreToolUse hook: {reason}.` — our existing
  "Treat this as their answer; do not ask again" phrasing is load-bearing
  precisely because it overcomes that wrapper, so do not reword it.
  Only `deny` is usable; hook timeout is 600 s uncapped (vs Claude's 120).
  Config: `~/.codex/hooks.json`. Hooks start UNTRUSTED — the user must
  approve once in Codex's hook-review UI, and the trust hash cannot be
  precomputed by an installer.
- Plan mode is NOT required: the hook runs in the tool router BEFORE the
  mode-availability check, so the answer is injected even in Default mode
  (where the tool would otherwise report "unavailable"). Discovery is the
  only limit — the model rarely reaches for it outside Plan mode.
- LEAD (untested end to end): /Applications/Claude.app registers the
  `claude://` scheme and handles `claude://resume`, with a UUID regex and
  a `claude://<host>/<segment>` parser next to error strings about CLI
  session transcripts. Our registry already stores exactly those CLI
  session UUIDs, so `claude://resume/<session-id>` may turn jump-to-session
  into "open THAT session" instead of just raising the app. Fired once for
  the current session; effect unconfirmed.

## 2026-07-31 (later) — Codex question path built; toast made clickable

- Shim `codex-ask` subcommand (distinct from the `codex` notify mode: notify
  delivers argv, the hook delivers stdin — never conflate them). Normalizes
  Codex's `request_user_input` payload into the SAME shape the app already
  takes (drops Codex's per-question `id`, pins `multiSelect:false`), so the
  Rust core and the Svelte card needed ZERO changes. `encode_reason` is
  shared, so the answer string is byte-identical to the Claude path.
- Codex sessions stay keyed by CWD HASH on both paths (`codex_session_id`).
  The hook's `session_id` is unrelated to notify's rotating ids, so keying
  on it would split one project into two board rows and a question would
  never share a card with that session's turn-complete callouts.
- `installers/src/codex.rs`: `install_hooks`/`uninstall_hooks` for
  `~/.codex/hooks.json` (JSON — separate from the existing `notify`
  config.toml path, which is untouched). Refuses malformed/foreign shapes,
  keeps a pristine first backup, reinstall replaces rather than stacks,
  uninstall removes only pingmybell entries and does not rewrite the file
  when we own nothing in it. CLI + tray: install-codex-hooks /
  uninstall-codex-hooks, both of which SAY that Codex ignores the hook
  until it is approved in ChatGPT → Settings → Hooks.
- Toast + attention cards are now CLICKABLE (user report: "this
  notification is not clickable"). `ToastView` gained `session_id` — the
  Rust side was not sending one, so there was literally nothing to jump to.
  Both are real buttons now, ↗ fades in on hover.
- cargo test 119 green (66 core + 27 installers + 26 shim). Two adversarial
  fail-open harnesses (Claude + Codex) pass against the REAL release shim.

### CODEX LIVE GATE (not yet run — needs a human click)

1. Run `install-codex-hooks` from the INSTALLED bundle (so the command
   string points at the bundled shim, not target/debug).
2. Approve the hook in ChatGPT → Settings → Hooks. Nothing fires until
   then, and changing the command string re-triggers review.
3. In a real Codex session, trigger a question: the Codex selector must NOT
   render, the notch card must show it, and answering must let Codex
   continue in the same turn on that answer — landing on the SAME board
   session as that project's turn-complete callouts.

## 2026-07-31 (late) — Typing keeps the park alive; hook budgets re-sized

- USER-REPORTED BUG from real use: a long typed answer was thrown away when
  the question timed out mid-sentence. "If I'm currently typing it, it
  shouldn't time out since I'm clearly active on it." Correct.
- KEY FINDING: **Claude Code has NO hook-timeout cap at 120 s** — that was
  our own config all along. Verified against 2.1.198 with a hook that slept
  560 s: the deny was still delivered and the model acted on it. Budgets now
  nest 600 (hook, AskUserQuestion matcher ONLY) > 570 (shim read) > 540
  (park ceiling) > 110 (base park, and the entire approval path, which stays
  unextendable by construction). The Claude installer writes TWO PreToolUse
  groups now — routine Bash/Write/Edit stay at 120 s so a wedged shim can
  never stall a normal tool call for ten minutes. A compile-time assert pins
  the nesting because the shim and installers can't import each other.
- Extensions are driven by EVIDENCE OF A PERSON, not by an open window:
  opening the reply window, typing (throttled), and a 20 s heartbeat that
  STOPS after 120 s with no pointer/key/focus activity — a window left open
  on a locked screen must not hold an agent to the ceiling.
- Expiry no longer destroys the draft: window and text stay, banner says the
  agent moved to the terminal, Send becomes Copy (via select+execCommand —
  this webview is not a secure context so navigator.clipboard is absent),
  and the window moves out from under the notch so a dead draft cannot
  occlude the next approval card.
- Reply window got a real NSVisualEffectView backdrop + native shadow (CSS
  backdrop-filter cannot blur behind a transparent native window), and the
  island now collapses to the sliver ENTIRELY while it is open — the card is
  wider and the expanded list taller, so anything drawn peeked out.
- CODEX PLUMBING VERIFIED LIVE: drove the real installed shim with the exact
  `request_user_input` payload → card on the notch → click → correct deny
  JSON → recorded as `codex | Caroline Levielle - Marketing | answered`,
  i.e. merged onto the SAME board row as that project's turn-completes
  (cwd keying works). Codex itself calling the tool is still unexercised.
- cargo test 131 green. Pushed to main (3dbf044).

### Next session

- LIVE GATE PASSED (2026-07-31 21:37): shipped to /Applications, re-ran
  install-claude (matcher now `AskUserQuestion|Bash|Write|Edit|MultiEdit`),
  and answered a REAL AskUserQuestion by clicking the notch card — the
  in-app selector never rendered, the answer arrived in the asking session
  through the deny channel, and event 351 recorded `needs_attention` with
  decision `answered`.
- STILL NOT EXERCISED: the full 110 s park to timeout-fallback, and a
  multi-question (n>1) call stepped through the card.
- Known gap: a stale `~/.pingmybell/port` after an unclean quit can make
  each question stall the shim for its full 115 s budget (still fail-open).
  A `/v1/health` probe before committing to the park would close it.
- HAZARD worth remembering: running a dev instance (`cargo run` /
  `tauri dev`) rewrites `~/.pingmybell/{port,token}` and silently hijacks
  the installed app's notifications. Kill dev instances and relaunch
  /Applications/PingMyBell.app afterwards.
- Then: CI release workflow, template styles, or Windows work.

## 2026-07-31 (later) — Questions no longer time out while you type

User report, reproduced: "I couldn't type out the answer, it timed out. If
I'm currently typing it, it shouldn't time out since I'm clearly active on
it." The `/v1/question` park was a fixed 110 s, oblivious to whether a human
was mid-sentence in the reply window; when it fired the card vanished and the
typed text was discarded.

- MYTH BUSTED: Claude Code does NOT cap PreToolUse hooks at 120 s. Measured
  against claude 2.1.198 with a temp `--settings` file (never the real
  ~/.claude/settings.json): a hook configured `"timeout": 600` slept **560 s**
  and its `permissionDecision:"deny"` still reached the model (`claude -p`
  total 570 s, exit 0, model obeyed the reason instead of running the tool).
  A 150 s control run behaved identically. 120 s was OUR number, not a cap.
- Budgets now nest 600 (hook) > 570 (shim `/v1/question` read) > 540 (server
  extension ceiling) > 110 (base park). The Claude installer writes TWO
  PreToolUse groups: `AskUserQuestion` at 600 s, `Bash|Write|Edit|MultiEdit`
  left at 120 s — only the path that waits on a human gets the long rope, so
  a wedged shim can never stall a routine tool call for ten minutes.
  Approvals are otherwise untouched — 110 s park, 115 s shim read,
  unextendable: a tool-call approval is a 2 s decision.
- `broker::Deadline` (base + hard ceiling, `extend` clamps and never moves
  backwards) is shared between the parked handler and the broker;
  `ingest::park_until` LOOPS on it instead of one `sleep`. `open_reply`,
  `submit_reply` and the new `keep_question_alive` command each buy 120 s.
  `Reply.svelte` beats every 20 s + throttled on keystrokes, and STOPS after
  120 s of no pointer/key/focus activity — an open window on a locked screen
  releases the agent in ~4 min instead of sitting on the ceiling.
- Draft is never eaten: `reply::expire_for` (new, distinct from `close_for`)
  drops the pending prompt so a late submit is refused, but LEAVES the window
  and the text on screen and emits `reply-expired`; the webview shows a banner
  and swaps Send/Cancel for Copy/Close. `close_for` now only runs when the
  user is genuinely done (answered / deferred).
- Bugs caught by the fresh-eyes review pass and fixed: a heartbeat issued for
  question A could land after question B loaded and mark B expired (the
  webview is only hidden between questions, never destroyed) — the id is now
  captured and re-checked; an expired window left under the notch occluded the
  next approval card — it is moved to the bottom of the screen; `open_reply`
  on an already-dead question latched `reply_open` with nobody left to clear
  it — it now refuses to open; `cancel_reply` hid whatever window was up even
  if a newer question owned it — `ReplyController::dismiss` guards that;
  `navigator.clipboard` is unavailable in this webview's non-secure custom
  scheme — Copy falls back to select + `execCommand`.
- Race closed while writing it: the parked handler can wake and drop its
  `QuestionCleanup` guard BEFORE `answer_question` clears the reply prompt, so
  the guard's expiry notice would flash "timed out" over a just-sent answer.
  The Answered branch now clears the prompt itself; the guard is a no-op.
- Also closed: an extension landing in the same instant the park timer fires
  could be lost. `Broker::expire_question_if_due` re-checks the deadline under
  the SAME lock `extend_question` takes, and `park_until` re-arms on
  `Expiry::Extended` instead of expiring.
- cargo test 131 green (77 core + 28 installers + 26 shim, 2 pre-existing
  ignored). Both adversarial fail-open harnesses still ALL PASS against the
  real release shim. `bun run check` 0 errors, clippy clean on changed files.
  Extra probe: the real release shim waits 130 s on a mock `/v1/question` and
  still delivers the answer (old wall was 115 s).

### ⚠️ ACTION REQUIRED

The installers now write `"timeout": 600`. Existing installs still have 120 s
in `~/.claude/settings.json` / `~/.codex/hooks.json`, so the agent kills the
hook mid-answer (fail-open — the terminal selector renders, nothing breaks,
but the extension does nothing). **Re-run `install-claude` and
`install-codex-hooks` from the INSTALLED bundle.** Changing the Codex command
string is not required here, but any hooks.json rewrite re-triggers Codex's
trust review (ChatGPT → Settings → Hooks).

## 2026-08-01 — Codex approvals (exec + file changes) from the overlay

Codex's hooks also fire for exec and file-change approvals, which happen far
more often than questions. Approve/deny a Codex command or patch from the same
notch card the Claude approval path already uses.

### Ground truth first (isolated CODEX_HOME, deleted; credentials never printed)

Established against codex-cli 0.146.0-alpha.9.2 with a hook script that logged
its stdin and returned canned decisions, then cross-read against the binary's
own hook sources. Full write-up in ARCHITECTURE.md §5.2.2.

- Approvals are NOT a `PreToolUse` matcher. They arrive on a separate
  `PermissionRequest` event, which fires ONLY where Codex was already going to
  block and ask a human. No `tool_use_id` in the payload.
- `tool_name` is `Bash` for every exec flavour (shell AND unified_exec — Codex
  reuses Claude Code's names) and `apply_patch` for file edits. `tool_input` is
  `{"command": …}` in BOTH cases: a shell command line, or the raw
  `*** Begin Patch …` text.
- **Approve is expressible, and it really runs the command** — the thing that
  was in doubt. `{"hookSpecificOutput":{"hookEventName":"PermissionRequest",
  "decision":{"behavior":"allow"}}}`, with NO `updatedInput`. Proof: the same
  `codex exec` prompt failed `Rejected("approval request failed")` with no hook
  decision, and with the allow reply instead produced `exec … exited 6` — it
  executed. The apply_patch run wrote the file. This is NOT the question path's
  limitation; that limitation belongs to `PreToolUse` only.
- `updatedInput` / `updatedPermissions` / `interrupt` each make Codex fail the
  hook CLOSED. Never send them.
- Deny's `message` is NOT wrapped (unlike §5.2.1) — it reaches the model as
  `Rejected("<message>")`, so it is written for the model.
- There is no `ask`. The card's Terminal button prints nothing; Codex then runs
  its own approval flow. Garbage stdout does the same — verified live.
- **Under auto-approval the hook does not fire at all.** With
  `approval_policy = "never"` (what `codex exec` defaults to, and the moral
  equivalent of bypassPermissions) the command ran with zero hook invocations.
  Known-safe read-only commands auto-approve under `untrusted` too and never
  reach the hook. Full-auto users pay literally nothing.

### Built

- Shim `codex-approve` (a THIRD channel — different event, different output
  schema, and unlike `codex-ask` it IS gated). Fast path first: the
  `gate_tool_calls` check happens BEFORE reading stdin (Codex's hook runner
  explicitly tolerates the resulting broken pipe), measured at 1.99 ms median
  vs the Claude `pretool` off-path's 1.78 ms. That return is an optimisation
  only — the flag is enforced inside `codex_approval_body` too.
- Gating decision: **same `gate_tool_calls` flag, same tray toggle, default
  off** — not for latency (Codex is already stopped when this fires) but
  because this channel can GRANT permission without the user ever seeing
  Codex's own prompt. That deserves an explicit yes. Questions still ignore it.
- Park budget: the approval ladder, not the question one — 110 s park / 115 s
  shim read / 120 s hook timeout, unextendable, identical to Claude's
  `Bash|Write|Edit|MultiEdit` matcher. Asserted at compile time.
- Session keying: `codex-<fnv(cwd)>`, so an approval lands on the SAME board
  row as that project's questions and turn-completes.
- `installers/src/codex.rs`: `HOOKS` table now writes both entries in one
  hooks.json pass (`PreToolUse:request_user_input` @600 s,
  `PermissionRequest:Bash|apply_patch` @120 s), keeping every merge/backup/
  uninstall semantic — including that an empty array the USER wrote is never
  pruned, now tracked per event key.
- `tool_summary` gained an `apply_patch` arm (raw patch → `Add canary.txt`,
  `Update src/a.rs, Delete b.txt`) and `speakable_tool` voices it "a file
  edit". `Bash` needed nothing: Codex's name and field already matched.
- Zero changes to broker.rs, overlay.rs, Overlay.svelte or the `decide`
  command — `/v1/approval` was already agent-agnostic.

### Verification

- `cargo test --workspace` 146 green (78 core + 33 installers + 35 shim, 2
  pre-existing ignored), up from 131. New tests cover the payload mapping for
  both flavours, the allow encoding (asserting the reserved keys are absent),
  deny/ask, drift fail-open, the tool allowlist, the cwd session key, gating,
  and installer merge/reinstall/uninstall against fixtures.
- New adversarial harness `codex-approve-failopen-harness.py` (72 checks) ALL
  PASS against the real release shim; the three existing harnesses (Claude
  approval, Claude question, codex-ask) still ALL PASS.
- `bun run check` 0 errors. Clippy clean on changed files (the one warning is
  pre-existing in overlay.rs).

### Fixed by the fresh-eyes review pass (all mutation-checked)

- `patch_summary` appended "…" at EXACTLY six files, telling the user files
  were hidden when none were. It now only says so after seeing a seventh.
- The 160-char cap test was vacuous — `patch_summary` stops at six entries, so
  the cap never fired and deleting it left the suite green. Replaced with
  multi-byte inputs (é/ü) that genuinely reach the cap, which is also the only
  guard on the char-boundary slicing.
- `codex_approval_body` hard-coded `should_gate_with(true, …)`; the only real
  gate was one early return no test invoked, so deleting it would have made
  approvals always-on for every default-off install with a green suite. The
  flag is now threaded through and enforced in the body builder.
- `tool_name` is now ALLOWLISTED to `Bash`/`apply_patch`. Previously any tool
  arriving on `PermissionRequest` (widened matcher, future Codex routing) could
  be approved from a card showing raw JSON.
- The new uninstall test asserted the wrong thing and never reached the
  two-event pruning logic. Replaced with the case that does exercise it: our
  entry removed from one event while an empty array the USER wrote under the
  sibling event survives (plus the mirror image). The logic was already
  correct; the test now proves it.
- The budget assertion compared two literals declared in the test body.
  `installers::codex::HOOKS` is now `pub` and the assertion reads the real
  installed timeouts — shortening one there fails the core test.
- `patch_summary`'s unrecognized-envelope fallback walked the whole body to
  keep 160 chars; bounded to 60 tokens.

### ⚠️ ACTION REQUIRED to turn it on

1. Ship the build, then run `install-codex-hooks` from the INSTALLED bundle
   (tray: "Install Codex Hooks", or `pingmybell install-codex-hooks`).
2. Approve **both** hooks in ChatGPT → Settings → Hooks. Trust is per hook,
   and rewriting hooks.json re-triggers review for the question hook too.
3. Turn ON the tray toggle "Approve Tool Calls From Overlay"
   (`gate_tool_calls`). Without it, approvals stay off and only questions work.
4. Codex must be running in a mode that actually asks (`untrusted` /
   `on-request`). In full-auto nothing fires, by design.

### Not yet exercised

- The full 110 s park to timeout-fallback on a REAL Codex approval.
- A real interactive (TUI/ChatGPT-app) approval end to end — everything above
  was proven through `codex exec`, where an unresolved approval errors out,
  which is exactly what made the allow evidence unambiguous.
- speaker.rs got a one-line `apply_patch` arm; it was outside the file list I
  was handed, so give it a glance.

## 2026-08-01 — open items after the five-area review

### Waiting on a Windows machine

Nothing Windows was verified at runtime; `cargo check --target
x86_64-pc-windows-msvc` cannot run on this Mac (libsqlite3-sys's C build has
no MSVC toolchain here). Reasoned-through only:

- `WS_EX_TOOLWINDOW` is now applied AFTER `window.show()`, because tao's
  `set_visible` recomputes the ex-style from scratch and drops it — the
  overlay was appearing in Alt-Tab (AC-5.3). The focus invariant survives
  either ordering, since the recompute re-emits `WS_EX_NOACTIVATE`.
- `SetForegroundWindow` focus hand-back when the reply window closes.
- `shell_quote` uses `cmd.exe` semantics there (double quotes, NO backslash
  escaping — a backslash is a path separator). If either agent runs hooks
  through PowerShell instead, `$` and backticks are live again and this is
  wrong. Nothing in the repo records which shell Windows actually uses.
- `titles.rs` infers the desktop app's store as `%APPDATA%` (Roaming) via
  `dirs::data_dir()`. If it is really under Local, session titles silently
  fall back to cwd basenames — no error, just the old behaviour.

Plan: install and run one session from Windows, note what breaks, fix later.

### Todo — overlay geometry has no unit tests

`WinState` now separates `applied` from `desired` so a failed resize is
retryable rather than sticky, and there is a bounded backoff. None of it is
unit-tested: the window paths need a live `AppHandle`, and extracting
`WinState` into a testable shell was judged more churn than it was worth
mid-review. Worth doing on a clean run, together with the screen-change
re-probe, which is also untested.

## 2026-08-04 — step 8 (Codex lifecycle over hooks) shipped

Root cause of "no Codex status": the Codex Computer Use app evicted our
shim from the single-occupancy notify slot at 18:58 (its binary shows no
evidence it forwards `--previous-notify`). Lifecycle now rides hooks.json
(SessionStart/UserPromptSubmit/Stop/SessionEnd → shim `codex-*` subcommands),
which nothing can evict. notify stays installed but is treated as dead.

User action pending: trust all SIX hook entries in ChatGPT → Settings →
Hooks (the two old ones changed command strings with the quoting fix), then
submit any prompt — the board should flip that session to working even on an
out-of-credits error. Unverified until credits refill: Stop on a real turn
(summary field), SessionEnd semantics, envelope-UUID session stability
across turns (two-prompt test; migrate identity if stable).

### 2026-08-05 00:04 — step 8 verified live

session_start + turn_start arrived from the real ChatGPT-app Codex and the
island showed MoodScene WORKING (user-confirmed, screenshot). Two findings
from the errored-turn test:

- An out-of-credits turn does NOT fire Stop: the session stays "working"
  with nothing coming. Self-corrects at next event or app restart
  (recovery marks it unknown). If it annoys, the fix is a staleness
  downgrade (working + no events for N hours -> unknown), NOT a Stop
  guess — long Codex turns are real.
- The hooks page still says "1 needs review" — almost certainly SessionEnd,
  below the fold. Until it is trusted, session_end never arrives and ended
  Codex sessions age out instead of leaving the board promptly.

Still pending credits (Aug 8): Stop on a REAL turn (does
last_assistant_message arrive), and the two-prompt session_id stability
test that decides the UUID identity migration.

## 2026-08-05 — step 12 (core half) shipped, step 13 (triage hotkey) complete

### Step 12 is BLOCKED on one thing: an authenticated `claude` CLI

`claude` on PATH answers "Not logged in · Please run /login", and the auto-mode
permission classifier refuses to let an agent write a hook entry into either
`~/.claude/settings.json` or the project's `.claude/settings.local.json`. So
the §12.1 capture rig — a `/bin/cat >>` PostToolUse entry, real payload on
disk before any mapper exists — cannot be run from inside a session. David
chose "log the CLI in" from the three unblock options; until that happens the
shim half stays unwritten, on purpose (the TurnStart lesson: never implement an
external surface from names alone).

Everything that does NOT depend on the payload shipped (6e5d872):

- `POST /v1/activity` with its OWN payload type, so an activity has no route
  into `Registry::apply` and cannot become an events row. Memory only: no
  state change, no `last_event_at` bump (it would distort ages, retention and
  step 11's waiting spans), never spoken, empty after a restart.
- Every path back to a running state retires the label — `apply` for lifecycle
  events, and a new `Session::resume_working` for the two that bypass it
  (answered approval, expired park). A label recorded while parked is HIDDEN,
  not dropped, so without this, approving a long command republishes a
  pre-park label under a live dot for minutes.
- Pacing: one emit per session per 500 ms, trailing emit reads the registry so
  the newest label wins and a beat landing after `turn_complete` corrects
  itself to null. An armed entry is believed for 5 s — a trailing task the
  runtime never ran must not silence that session for the day.
- Its own `session-activity` event, not `session-updated`: the board answers
  that one with a full snapshot plus a 50-row history query, and the island
  refresh is skipped entirely while the list is collapsed (`emit` deep-clones
  every session to build a view with no rows in it).

Review found and fixed before commit: the always-on overlay refresh, an
ellipsis declared on a flex container (no-op — labels clipped mid-glyph), the
two non-`apply` state flips above, and a `Session` clone per tool call on the
registry mutex.

### Step 13 — triage hotkey, gate PASSED live (65d3813)

`Ctrl+Alt+Space` (override: `hotkey.next` in config.json) focuses the
longest-waiting session; repeat presses cycle. Two design-level bugs found by
the fresh-eyes review, both confirmed by hand before fixing:

- A per-session skip TTL STARVES the tail: the first entry expires mid-walk
  and, being the oldest, wins the next lookup. At 8 parked agents and 3 s a
  press, sessions 5-8 were permanently unreachable. The skip list is now a
  round with one clock, abandoned as a whole when you stop pressing.
- Running out of unvisited sessions is NOT "all clear" — with two parked
  agents every third press claimed the board was empty while both sat blocked.
  A press with nothing new wraps to the longest wait instead.

Also: a 250 ms repeat guard (Windows repeats `WM_HOTKEY` while held and the
plugin reports every repeat as a new press; macOS's Carbon event does not),
and the "all clear" pill says how many sessions have not reported since a
restart rather than overclaiming, since recovered sessions are `unknown` and
`unknown` is not a triage target.

Live verification against the installed bundle, driven with `osascript` key
codes: press → oldest, press → the other, press → wraps, clear them → "nobody
waiting"; a 100 ms double-tap produced exactly ONE decision; `lsappinfo front`
unchanged across every press (AC-5.1 holds).

Two throwaway sessions (`pmb-triage-check-a`/`-b`) were created for this and
removed again — app stopped first, exactly those two ids deleted, count back
to where it started. NOTE for future live tests: David was at the keyboard and
the amber cards landed on his notch; a `permission_request` also SPEAKS. Use
`session_end` to clear test rows quietly, and keep synthetic global keystrokes
to a minimum while someone is working.

`cargo test --workspace` 271 (158 core + 64 installers + 49 shim, 2 pre-existing
ignored), clippy clean apart from the known `installers/src/codex.rs:382`
warning, `bun run check` 0 errors. Windows cross-check (`cargo check --target
x86_64-pc-windows-msvc`) still cannot run on this Mac; CI owns it — worth
watching for the new `tauri-plugin-global-shortcut` dependency.

## 2026-08-05 (later) — steps 9, 11 and 16 shipped; 12 still blocked

Order run this session: 12 (core half only — blocked), 13, 9, 11, 16. Each
shipped to /Applications and gated live before the next was started.

### Step 9 — callout styles + rate/volume (a2c12c1)

Four sentence builders became one pure `callout(style, kind, agent, project)`.
Three styles; status-only drops the summary ENTIRELY, which is the reason to
pick it, so a test asserts the summary cannot leak into it.

Rate and volume map through the ranges the ENGINE reports, measured not
assumed (§11.2 asks for this): this Mac's AVFoundation says rate 0.1/0.5/2.0,
volume 0.0/1.0/1.0. `1.5x` is `normal * 1.5` clamped — an interpolation
between min and max would make "half speed" mean a fifth of normal here.
Unsupported features are never touched: one backend's `min_rate` is
`unimplemented!()`, so probing it eagerly would kill the speaker thread at
startup for anyone running a screen reader.

Settings auditions are a new `Utterance::audition` flag: they bypass both
dedup windows and interrupt. Without it every sample says the same sentence
and the 10 s identical-text guard swallows it — the sliders would have been
silent after the first move.

`update()` now REFUSES to write when config.json exists but does not parse.
Readers falling back to defaults is right; a setter doing it replaces the file
with `{}` plus one key and destroys every voice and gate setting. Sliders
write often, so a bad hand-edit plus one drag was all it took.

Gate (measured, not by ear): the same sentence took 8 s at rate 0.6 and under
1 s at 2.0 — the worker cannot start the next utterance until the current one
ends, so the gap between log lines IS the speaking time. The three styles
produced 105 / 112 / 30 characters for identical input. Both survived a
restart.

### Step 11 — waiting metrics + one escalation (a3241ad)

§11.4 assumed this was a pure query. It is not: a wait ending in a DECISION
ends with an UPDATE and no row, so the only close time available was the
session's NEXT event. Hence `events.decided_at`, migrated in on open.

Spans close at the FIRST of four things (MIN, not a COALESCE ladder): the
decision, the next event, the moment the session stopped being parked, or now.
A priority ladder double-counts overlapping sibling parks and lets a late
click bill for time nobody was blocked. The third term is what stops a span
running forever — a `needs_attention` from the Notification hook has no broker
park to time out, and after a restart the session is `unknown`, where
`clear_attention_state` will not touch it. One dead terminal was worth 168 h.

Waits recorded before the column existed count as ZERO, not as estimates: on
this machine's real database the next-event estimate produced "you kept agents
waiting 21h 59m this week" on first launch, 14 h of it from one approval whose
session went quiet overnight. The migration reported 41 unmeasured waits
zeroed and the header then read 0.

Gate: a parked session read 100 s after 100 s; the week total read exactly
that; exactly one reminder fired at 30 s with none in the next 70 s; with
`quiet.remind_after_secs` off, none at all.

### Step 16 — morning digest (36395f1)

First ingest event of a local day, plus a launch catch-up, sharing ONE day
claim in managed state (they had one mutex each at first, and the disk write
lands only after aggregation — both could speak). Weekends: a Monday covers
since Friday. chrono added (default-features off, `clock` + `std`) because
"local calendar day" is a claim about the user's clock; DST-ambiguous
midnights, the hour Brazil does not have, leap days and year boundaries are
all tested.

A single wait counts for at most an hour IN THE DIGEST (`DIGEST_SPAN_CAP`).
An overnight park is 15 h of blocked agent and saying so over breakfast makes
the feature read as broken; the board's live row still shows the real figure,
where it is the useful alarming truth. `events` gained the `created_at` index
its queries always wanted.

Gate: first event of the day spoke it once; three further events and the
launch catch-up spoke nothing; a restart with the day recorded stayed silent.
The board card itself has only been verified through its unit tests and the
`digest-ready` event — worth a glance on screen.

### Review process note

All three steps went through a fresh-eyes subagent that had not written the
code, and all three came back with real defects — several proven by the
reviewer running mutations against a scratch copy rather than by reading. The
pattern worth keeping: the reviewer is asked to reason about whether each new
test would FAIL if its behaviour were broken. That is what caught three
tautologies in the digest's end-to-end test and an untested `decided_at`
write whose deletion left the suite green.

### Test-data hygiene

Every throwaway session created for a live gate was removed again (app stopped
first, exactly those ids deleted, counts back where they started). NOTE: 12
`pmb-scrolltest-*` rows from an EARLIER session are still in the database —
not mine to delete, but worth clearing.

`cargo test --workspace` 305 (192 core + 64 installers + 49 shim, 3 ignored:
2 tmux + the new speech-range probe, which prints this machine's backend
ranges on demand). Clippy clean apart from the known
`installers/src/codex.rs:382`. `bun run check` 0 errors.

## 2026-08-05 (later) — step 12 COMPLETE: the ticker is live

David logged the CLI in, so the §12.1 capture rig finally ran: four real
`PostToolUse` payloads from claude 2.1.198 through a throwaway `--settings`
file, and the mapper written from those rather than from the field names.

What the capture settled: the envelope is PreToolUse's (`session_id`, `cwd`,
`hook_event_name`) plus `tool_name`, `tool_input`, `tool_response`,
`tool_use_id`, `duration_ms`, `effort`, `prompt_id`. A matcher-less row really
does fire for everything — `Read`, `ToolSearch` and `TaskCreate` all arrived,
none of which our PreToolUse matcher lists. Only `Bash` carries `command`;
only file tools carry `file_path`; everything else names its payload something
of its own (`query`, `subject`, `description`), which is why every other tool
shows its name alone.

### Two bugs the unit tests could never have caught

1. **The transport.** The shim first posted WITHOUT reading the response — a
   202 says nothing and skipping the round trip measured 1.88 ms vs 2.59 ms.
   But axum drops a handler whose client has gone (the same cancellation
   `/v1/approval` relies on deliberately), so the shim raced the server into it
   and the ticker never fired at all. All 54 shim tests passed: they cover the
   mapper, not the wire. ONLY the live gate found it. The response is read now
   on a 400 ms budget, and there is a real socket test — which still would not
   have caught this, and says so in its own comment.

2. **The label was a credential.** `activity_label` took the first
   whitespace token of a command, and agents run `AWS_SECRET_ACCESS_KEY=…
   npm run deploy` and `GITHUB_TOKEN=ghp_… gh pr create` all day. A 43-char
   AWS key renders essentially whole inside the core's 48-char cap, on an
   always-on-top window, in every screen share. It also showed absolute
   program paths in full while the file arm basenamed — the asymmetry was the
   tell. Now: skip `NAME=value` prefixes, basename the program, drop anything
   still carrying shell punctuation. Found by the fresh-eyes review, verified
   on the wire, fixed, and re-verified live (`SECRET_TOKEN=… echo hi` → "echo",
   secret appears nowhere).

### Gate (installed bundle, real claude turns)

Five ticks arrived as the agent worked (Read, Bash, Edit, Read, Bash); the
events table grew by exactly the four lifecycle rows and `SELECT COUNT(*)
FROM events WHERE kind NOT IN (<the six lifecycle kinds>)` is 0; exactly one
thing was spoken (the completion). 126 adversarial fail-open checks pass
against the real release shim with the app both running and absent. Ticks from
David's own concurrent session showed up too — the ticker is running in real
work, not just in the harness.

### Follow-ups

- `NotebookEdit` and `BashOutput` were REMOVED from the mapper: their inputs
  were never captured (`notebook_path` and `bash_id`, reportedly), and §12.1
  is exactly the rule against inferring that. Capture them, then add them.
- ARCHITECTURE §12.1 updated to the shipped design: its own `/v1/activity`
  route and its own `session-activity` event, both deliberate deviations from
  the written design, with reasons.
- Codex's own `PostToolUse` is untouched — §12.1 says capture it separately,
  after Claude ships. It has now shipped.
- A `RUST_LOG=debug` run logs one line per tick (sanitized text) plus the two
  silent paths, which is how "is the ticker firing?" gets answered without
  putting file names in a log by default.

## 2026-08-05 — jump-to-exact-session re-checked: still no

Question: does Claude.app 1.25927.0 (built 08-04, newer than the 07-31
investigation) let us focus a SPECIFIC session by id, instead of only raising
the app? Answer: no, and the reason is the same one as before.

Tested by firing each form and reading the app's own log
(`~/Library/Logs/Claude/main.log`), against a session that ALREADY had a
desktop entry — the case where "focus" is the only sensible behaviour:

- `claude://resume/<uuid>` → `[warn] Resume deep link: missing or invalid
  session { sessionId: null }`. The path segment is not read; the 07-31 note
  that this form "fired once, effect unconfirmed" is now explained — it was
  rejected outright.
- `claude://resume?sessionId=<uuid>` → the same warning. Wrong param name.
- `claude://resume?session=<uuid>` → `[info] Resume deep link: importing CLI
  session …` and a 49th desktop session appeared, duplicating the one that
  already existed for that `cliSessionId`.

It is an import feature. It was one in July and it is one now, and it
duplicates even when the target is already in the app.

The route we actually want EXISTS in the bundle —
`setFocusedSession(sessionId)` and `signalSessionIntent({kind:"resume",
sessionId})` on a `LocalAgentModeSessions` interface — but as internal
Electron IPC (`$eipc_message$…` channels behind origin validation), with no
URL or CLI surface. Nothing for us to call. Re-check when a `claude://` route
maps to those, or when the app ships a CLI for it.

Method note for whoever re-checks: `~/Library/Application Support/Claude/
claude-code-sessions/**/local_*.json` carries `cliSessionId` and
`lastFocusedAt`, so import-vs-focus is measurable without watching the screen —
a new file means import, a changed `lastFocusedAt` means focus. The app's
main.log says which it did in as many words.

### The deeper dig, so the next re-check starts here rather than from scratch

`claudeURLHandler` switches on the URL HOST, mapped through a route enum, and
three cases matter:

- **`resume`** — `?session=<cli uuid>` imports (and duplicates); the path form
  and `?sessionId=` are both rejected with "missing or invalid session
  { sessionId: null }". Verified above.
- **`cowork`** — recognizes exactly `/shared-artifact?uuid=…` and `/new`
  (`q`/`folder`/`file` params, navigates to `/task/new`). Anything else logs
  "unrecognized cowork path".
- **`code`** — `if (d.a(o.a(o.n + pathname))) se(...)`, where `se` is a
  "code session deep link" that NAVIGATES (not imports), emits telemetry
  `desktop_code_deeplink_session_received`, and is behind the feature gate
  `a.ka("2143883161")` — its failure mode logs "code session deep link gated
  off". This is the closest thing to the route we want.

Sixteen pathname shapes were probed against the `code` host and every one
logged "unrecognized code path": `/code/sessions/<desktop id>`,
`/code/sessions/<cli id>`, `/code/<either id>`, `/sessions/<id>`,
`/session/<id>`, `/s/<id>`, `/local_sessions/<id>`, `/hub/<id>`, and the bare
routes `/code`, `/code/hub`, `/code/agents`, `/code/sessions`. Since a
genuine route like `/code/hub` is rejected too, `o.n` and `d.a` are not the
base-path-plus-code-route-predicate they look like, and were not resolved.

**Recommendation: do not build on this even if the shape is found.** It is
undocumented, minified, feature-gated per account, and the surrounding
handler changed between two builds a week apart. Today's jump — raise the
host app — needs no permission, cannot duplicate anything, and is right often
enough. The gate to revisit is a DOCUMENTED route, or `setFocusedSession`
gaining a surface outside Electron IPC.

The Accessibility route (drive the app's own session list via AXUIElement) is
deliberately not pursued: it would trade a no-permission jump for a permission
prompt plus UI automation that breaks on every redesign.
