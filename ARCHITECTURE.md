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
   │ (Claude Code hooks)   │ (Codex notify+hooks) │
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
    "PreToolUse":   [{ "matcher": "AskUserQuestion",
                       "hooks": [{ "type": "command", "command": "<shim> claude pretool", "timeout": 600 }] },
                     { "matcher": "Bash|Write|Edit|MultiEdit",
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

**Park budgets.** Note the two PreToolUse groups. They are two different kinds of wait and must not share a budget:

- **`AskUserQuestion` → 600 s.** A typed free-text answer extends its park while the user is actually typing (§5.1.1), so the hook has to outlast that. Verified empirically against claude 2.1.198 on 2026-07-31: a hook configured `"timeout": 600` was allowed to sleep **560 s** and its `permissionDecision: "deny"` still reached the model — `claude -p` waited 570 s and the model obeyed the reason instead of running the tool. A 150 s control run behaved identically. **120 s was our own config, never a Claude Code cap**; treat "hooks cap at 120 s" as folklore. Codex's own default is 600 s and uncapped (§5.2), so both agents match.
- **`Bash|Write|Edit|MultiEdit` → 120 s** (unchanged). An approval is a two-second yes/no and the shim abandons it after 115 s regardless, so the long rope buys nothing here — and keeping it short means a wedged shim can never stall a routine tool call for ten minutes.

Four budgets, each strictly outlasting the one below it, so the party that gives up first is always **us** and never the agent:

| s | what | where |
|---|---|---|
| 600 | agent hook timeout, question matcher only | `installers/src/{claude_code,codex}.rs` |
| 570 | shim read timeout, `/v1/question` | `QUESTION_READ_TIMEOUT`, `shim/src/main.rs` |
| 540 | question park **ceiling** | `QUESTION_MAX_PARK_SECS`, `src-tauri/src/ingest.rs` |
| 110 | question park **base**, and the whole approval park | `APPROVAL_TIMEOUT_SECS`, same file |

Approvals nest the same way one rung shorter, identically for both agents — Claude's `Bash|Write|Edit|MultiEdit` matcher and Codex's `PermissionRequest` matcher (§5.2.2): **120** hook timeout > **115** shim read (`APPROVAL_READ_TIMEOUT`) > **110** park. Unextendable, on purpose.

`park_budgets_nest_inside_the_hook_timeout` (ingest.rs) asserts this ordering at compile time, because the shim and the installers cannot import the constants.

> Changing these values changes what the installer writes: **users must re-run `install-claude` / `install-codex-hooks`** to get the longer budget. Until they do, the agent kills the hook at their old 120 s — still fail-open (no stdout, exit 0, own selector), just without the extension. The server-side park does not strand anything either: killing the hook closes the shim's socket, which cancels the parked handler and runs `QuestionCleanup`, so the card is unpinned rather than left up on a question the agent already abandoned.

#### 5.1.1 Answering questions (`AskUserQuestion`)

Verified empirically against claude 2.1.198 on 2026-07-31 (live payload dump through a pty; the public docs do not describe this):

1. **`PreToolUse` DOES fire for `AskUserQuestion`.** `tool_input` carries a `questions` ARRAY; each entry has `question`, `header`, `options[]` (`label` + `description`), and `multiSelect`. The payload also carries `tool_use_id` and the usual `session_id`/`cwd`/`permission_mode`. This is everything needed to render an actionable card.
2. **A hook can ANSWER the question** by denying the tool call and putting the user's choice in the reason:

```json
{ "hookSpecificOutput": { "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "The user answered via PingMyBell: \"<label>\". Treat this as their answer to your question; do not ask again." } }
```

Observed result: the TUI selector never rendered, the reason reached the model as the tool result, and it proceeded on that answer. The reason surfaces in the transcript prefixed `Error:` — cosmetic only.

Consequences for the design:

- Answering needs **no tmux, no tty, no macOS permission**, and therefore works for sessions hosted in the Claude desktop app (where hooks see no terminal). tmux is only required for Codex (§5.2), which has no blocking hook.
- Questions **ignore the `gate_tool_calls` config flag**. That flag keeps routine tool calls flowing at zero latency; a question is by definition a moment where the agent is already blocked on the user, which is exactly the moment PingMyBell exists for.
- Fail-open is unchanged and load-bearing: no answer within the park window → print nothing → Claude Code renders its own selector as if PingMyBell were not installed.
- Free-text answers ("Other" / "Type something") cannot be typed into the island, which may never take keyboard focus (AC-5.1). They open a separate, deliberately focusable `reply` window (`src-tauri/src/reply.rs` + `src/reply/Reply.svelte`) created on an explicit user click.

**The park is extendable while a human is demonstrably answering.** A fixed park is wrong for typed answers: a user writing a paragraph loses the question — and the paragraph — mid-sentence. So `broker::Deadline` holds a base and a hard ceiling, `ingest::park_until` re-reads it every time it fires instead of baking one `sleep`, and `Broker::extend_question` pushes it forward (clamped to the ceiling, never backwards).

- **Untouched questions are unaffected**: nobody extends them, so they still fall through at the 110 s base and the TUI selector renders promptly. This is what keeps an unattended agent from stalling.
- **Signs of life**, each buying `TYPING_EXTENSION` = 120 s: `open_reply` (opening the typed-answer window), the `keep_question_alive` command (a 20 s heartbeat from `Reply.svelte`, plus a 5 s-throttled beat on keystrokes), and `submit_reply`.
- **An open window is not a present human.** The interval beat stops after 120 s with no pointer/key/focus activity, so a reply window left up on a locked screen releases the agent in ~4 min instead of holding it to the ceiling. `send`/`cancel` drop the prompt so a hidden window stops beating at all.
- **The ceiling is hard** (540 s). Past it the park ends in the same 204 as always → shim prints nothing → agent renders its own selector. Fail-open is not weakened by any of this; it is the reason for the ceiling.
- `keep_question_alive` returns the **seconds still left**, or `null` when the park is over — a heartbeat can never resurrect a dead park. As the ceiling nears, that number stops growing and starts shrinking, and the window warns the user rather than letting the banner arrive mid-sentence.
- The expiry check itself (`Broker::expire_question_if_due`) compares the deadline **under the same lock** `extend_question` takes. Without that there is a hairline window where a keystroke landing exactly as the timer fires still loses the question — the original bug, just narrower.

**A dead question must not eat the draft.** When a question stops being answerable while its reply window is open (ceiling reached, or the shim's connection died), `reply::expire_for` — *not* `close_for` — runs:

- it drops the pending prompt, so a late `submit_reply` is correctly refused;
- it leaves the window and the typed text on screen and emits `reply-expired`, so the webview explains what happened and offers Copy/Close (the copy path falls back to select + `execCommand`, because this webview is served from a custom scheme and `navigator.clipboard` is not available in a non-secure context);
- it **moves the window to the bottom of the screen** (`park_expired`). The reply window floats one level above the island, so a dead draft left under the notch would occlude the next approval card — an unclickable approval is worse than the bug this window exists to prevent.

`close_for` (which hides the window) is reserved for the paths where the user is finished: answered, or deferred to the terminal. `cancel_reply` goes through `ReplyController::dismiss`, which refuses to hide a window a *newer* question has taken over while still closing an expired draft that has no prompt left.

> ⚠️ Verify hook event names and output schema against the live docs (https://code.claude.com/docs/en/hooks) and the installed `claude --version` before finalizing the shim — the schema evolves. Treat this file's JSON as the design, the docs as the authority.

### 5.2 Codex CLI (`adapters/codex.rs` + installer)

Three integrations: notifications (below, `config.toml`), questions (§5.2.1) and approvals (§5.2.2). Notifications install and uninstall independently of the other two; questions and approvals share one `hooks.json` write and are installed and removed together.

**Notifications** — `~/.codex/config.toml`: `notify = ["<shim path>", "codex"]`. Codex invokes the shim with one JSON argument; event `agent-turn-complete` carries `type`, `thread-id`, `turn-id`, `cwd`, `input-messages`, `last-assistant-message` → normalized `turn_complete` with `summary = last-assistant-message`. Codex allows exactly ONE notify program, so the installer chains any pre-existing one (`<shim> codex --chain <prog> <args…>`). Do not touch `tui.notifications`.

#### 5.2.1 Answering questions (`request_user_input`)

Verified empirically against `codex-cli 0.146.0-alpha.9.2` (bundled at `/Applications/ChatGPT.app/Contents/Resources/codex`, not on PATH) on 2026-07-31; the output schema is confirmed against the binary's own embedded `pre-tool-use.command.output` JSON schema.

Codex has Claude-shaped hooks. A `PreToolUse` hook on the `request_user_input` tool receives the question payload and can ANSWER it with the same deny channel Claude Code uses (§5.1.1) — one turn, no re-ask.

Config lives in `$CODEX_HOME/hooks.json` (i.e. `~/.codex/hooks.json`) — **JSON, not `config.toml`**:

```json
{ "hooks": { "PreToolUse": [
    { "matcher": "request_user_input",
      "hooks": [{ "type": "command", "command": "<shim> codex-ask", "timeout": 600 }] } ] } }
```

`timeout` must outlast the shim's longest park — 570 s on `/v1/question`, itself outlasting the 540 s server ceiling (§5.1). 600 s is both Codex's own default (uncapped) and what the Claude Code entry now uses, so the two agents behave identically. Command hooks run through `$SHELL -lc`, so the shim path is shell-quoted.

> ⚠️ **Trust gate — a manual step.** A new or changed hook starts UNTRUSTED and is silently inert until the user approves it in Codex's own hook-review UI (ChatGPT app → Settings → Hooks). The trust key is a sha256 over the hook's normalized identity, which we cannot precompute — so the installer writes the entry and the human approves it once. Any later change to the command string (e.g. moving the app bundle) re-triggers review. Every success message must say so, or the feature looks broken.

Hook payload (verified on stdin):

```jsonc
{ "session_id": "019fbb08-…", "turn_id": "019fbb08-…",   // envelope extras vs Claude
  "transcript_path": "…/sessions/2026/07/31/rollout-….jsonl",
  "cwd": "/path/to/project", "hook_event_name": "PreToolUse",
  "model": "gpt-5.6-sol", "permission_mode": "bypassPermissions",
  "tool_name": "request_user_input",
  "tool_use_id": "call_mxZtKQ9NIQrRQcgRNrc5DhHR",
  "tool_input": { "questions": [
    { "id": "deployment_target",          // REQUIRED, snake_case; Claude has none
      "header": "Target",
      "question": "Which deployment target should I use?",
      "options": [ { "label": "Staging (Recommended)", "description": "…" },   // description required
                   { "label": "Production", "description": "…" } ] } ] } }     // max 3 questions × 2–3 options
```

**The shim normalizes at the boundary** (`codex-ask` in `shim/src/main.rs`): Codex's questions are mapped into the exact `/v1/question` body the Claude path already sends — `{question, header, options[label, description], multiSelect: false}` — so `ingest.rs`, `broker.rs`, the overlay card and the reply window are agent-agnostic and needed **zero changes**. Specifically:

- the per-question `id` is **dropped**: the deny channel carries prose, not an id-keyed map, so nothing downstream can use it;
- there is no `multiSelect` on the wire — Codex questions are single-select and Codex renders its own free-form "Other" affordance — so the shim pins `multiSelect: false`;
- envelope extras (`turn_id`, `model`, `agent_id`/`agent_type`, Codex's extra `permission_mode` values `dontAsk`/`bypassPermissions`) are not read.

**Session identity**: the question is keyed by `cwd` hash (`codex-<fnv(cwd)>`), the SAME key `map_codex_notify` uses — NOT the hook's `session_id`, which is unrelated to the rotating ids the notify payload reports. Keying on it would split one Codex session into two board rows so a question and that session's turn-complete callouts would never share a card.

Output (identical shape to §5.1.1, and the phrasing is byte-identical — it demonstrably survives Codex's wrapper):

```json
{ "hookSpecificOutput": { "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "The user answered via PingMyBell: \"<label>\". Treat this as their answer to your question; do not ask again." } }
```

Codex wraps the reason before the model sees it: `Tool call blocked by PreToolUse hook: {reason}. Tool: request_user_input`. Only `deny` is usable — `allow` requires `updatedInput`, `ask` is always rejected. Fail-open is native on Codex's side too (a blank/missing reason or malformed output just lets the tool run) and unchanged on ours: any error → exit 0, no stdout → Codex renders its own selector.

#### 5.2.2 Approving commands and file changes (`PermissionRequest`)

Verified empirically against `codex-cli 0.146.0-alpha.9.2` on 2026-07-31 in a throwaway `CODEX_HOME` (deleted afterwards), and cross-read against the binary's own hook sources.

**Approvals are NOT a `PreToolUse` matcher.** They arrive on a separate hook event, `PermissionRequest`, with a different payload and a different output schema. This is the whole reason Approve is expressible here and is not on the question path:

| | `PreToolUse` | `PermissionRequest` |
|---|---|---|
| fires | every tool call | only where Codex was already going to block and ask a human |
| payload extra | `tool_use_id` | *(none — no `tool_use_id`)* |
| output | `permissionDecision` + `permissionDecisionReason` | `decision: { behavior, message }` |
| `allow` | requires `updatedInput` | **needs nothing else** |
| deny text | wrapped `Tool call blocked by PreToolUse hook: …` | reaches the model verbatim as `Rejected("…")` |

Config (installed alongside the question hook, one `hooks.json` write):

```json
{ "hooks": { "PermissionRequest": [
    { "matcher": "Bash|apply_patch",
      "hooks": [{ "type": "command", "command": "<shim> codex-approve", "timeout": 120 }] } ] } }
```

`Bash|apply_patch` is an EXACT matcher, not a regex: Codex treats a pattern of only `[A-Za-z0-9_|]` as a literal alternation list. `Bash` is Codex's hook-facing name for **every** exec flavour (shell and unified_exec both report it — Codex deliberately reuses Claude Code's names), `apply_patch` for file edits; Codex also accepts `Write`/`Edit` as aliases for the latter, which we do not need.

Verbatim payloads (dumped from the live binary):

```jsonc
// exec approval
{"session_id":"019fbb4f-ac81-…","turn_id":"019fbb4f-acba-…",
 "transcript_path":"…/sessions/2026/07/31/rollout-….jsonl",
 "cwd":"/path/to/project","hook_event_name":"PermissionRequest",
 "model":"gpt-5.6-sol","permission_mode":"default",
 "tool_name":"Bash","tool_input":{"command":"curl -sS https://example.com -o /dev/null"}}

// file-change approval — same envelope, raw patch text in the SAME field
{"…":"…","tool_name":"apply_patch",
 "tool_input":{"command":"*** Begin Patch\n*** Add File: canary.txt\n+PMB_PATCH_CANARY\n*** End Patch"}}
```

Output, one of exactly two shapes (or nothing):

```json
{ "hookSpecificOutput": { "hookEventName": "PermissionRequest",
    "decision": { "behavior": "allow" } } }

{ "hookSpecificOutput": { "hookEventName": "PermissionRequest",
    "decision": { "behavior": "deny", "message": "<prose the model reads>" } } }
```

- **`allow` genuinely runs the command.** Proven, not inferred: in `codex exec` an unresolved approval fails with `command execution approval is not supported in exec mode` → `Rejected("approval request failed")`. With the hook returning `behavior: "allow"` the identical prompt instead produced `exec /bin/zsh -lc 'curl …' … exited 6` — it executed. The `apply_patch` run wrote the file.
- **Never send `updatedInput`, `updatedPermissions`, or `interrupt`.** Each makes Codex fail the hook CLOSED (`PermissionRequest hook returned unsupported updatedInput`) and discard the decision. `allow` needs the behavior and nothing else.
- **`ask` does not exist.** The card's Terminal button prints NOTHING, so Codex runs its own approval flow — the same shape as every fail-open path.
- **Deny is unwrapped**, unlike §5.2.1, so the message is written for the model directly.
- **Fail-open is native on Codex's side too**, verified: a hook whose stdout is garbage (`not json at all`) had its decision ignored and the normal approval flow ran. Timeouts, non-zero exits, and empty stdout behave the same. (Do not exit 2: `PermissionRequest` treats exit 2 + stderr as a **deny**. The shim only ever exits 0.)

**Gating: `gate_tool_calls`, same flag as Claude approvals, default off.** Not because of latency — Codex only fires this hook where it was already stopping to ask — but because this channel can *grant* permission without the user ever seeing Codex's own prompt, which deserves an explicit yes. Two consequences:

- The shim checks the flag **before reading stdin** (Codex's hook runner explicitly tolerates the resulting broken pipe), so the off path is a ~2 ms process that never connects to anything — measured, and level with the Claude `pretool` off path at ~1.8 ms. That early return is an optimisation only: the flag is *enforced* inside `codex_approval_body`, so deleting it could never make approvals always-on.
- Questions still ignore the flag (§5.2.1); they are a different thing.

**`tool_name` is allowlisted to `Bash` and `apply_patch`.** Our matcher selects only those, but a widened matcher — or a future Codex routing another tool through `PermissionRequest` — must not let the overlay grant permission for something the card cannot summarize (`tool_summary` would fall through to raw JSON). Anything unrecognized falls through to Codex's own prompt, the same stance the question path takes by pinning `request_user_input`.

**The hook does not fire at all under auto-approval.** `permission_request` runs from Codex's approval path, which is only entered when the tool call resolves to `NeedsApproval`. Verified: under `approval_policy = "never"` (what `codex exec` uses by default, and the moral equivalent of `bypassPermissions`) the same command ran with no hook invocation whatsoever. A known-safe read-only command under `approval_policy = "untrusted"` likewise auto-approves and never reaches the hook. So a user in full-auto pays exactly nothing, with or without the flag.

**Park budget: the approval budget, not the question one.** An approval is a two-second yes/no, so it nests one rung shorter all the way down and is *not* extendable — 110 s park (`APPROVAL_TIMEOUT_SECS`) < 115 s shim read (`APPROVAL_READ_TIMEOUT`) < 120 s hook timeout, identical to Claude's `Bash|Write|Edit|MultiEdit` matcher. `park_budgets_nest_inside_the_hook_timeout` asserts both ladders at compile time.

**Session identity**: `codex-<fnv(cwd)>`, the SAME key the notify and question paths use, so an approval lands on the same board row as that project's questions and turn-completes.

**Card and voice**: `tool_summary` already read `input.command` for `Bash`, so exec approvals needed nothing. `apply_patch` gets an arm that reduces the raw patch to the files it touches (`Add canary.txt`, `Update src/a.rs, Delete b.txt`) — 400 characters of diff is useless on a one-line card — and `speakable_tool` voices it as "a file edit".

> ⚠️ Same trust gate as §5.2.1, and trust is **per hook**: the user must approve BOTH entries in ChatGPT → Settings → Hooks. Installing approvals rewrites `hooks.json`, which re-triggers review for the question hook too.

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
