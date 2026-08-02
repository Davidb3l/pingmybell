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
