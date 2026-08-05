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
- `POST /v1/activity` — fire-and-forget activity ticker (§12.1). Body: `{"session_id": …, "tool": "Bash", "label": "cargo"|null}`. 202 once the body parses — including for a session the registry does not know, and for one that is parked or finished (nothing to update, and nothing the shim could do about it either way); 400 only for a body that is malformed or carries no tool name at all. Its OWN route, not an `event` kind: an activity must never be persisted, and giving it a type that `Registry::apply` cannot accept is what guarantees that rather than a code review.
- Auth: `Authorization: Bearer <token>` on every request.

Normalized event:

```jsonc
{
  "agent": "claude-code" | "codex",
  "event": "session_start" | "turn_start" | "turn_complete" | "needs_attention" | "permission_request" | "session_end",
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
    "SessionStart":     [{ "hooks": [{ "type": "command", "command": "<shim> claude session-start" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "<shim> claude prompt-submit" }] }],
    "Stop":             [{ "hooks": [{ "type": "command", "command": "<shim> claude stop" }] }],
    "Notification":     [{ "hooks": [{ "type": "command", "command": "<shim> claude notification" }] }],
    "SessionEnd":       [{ "hooks": [{ "type": "command", "command": "<shim> claude session-end" }] }],
    "PreToolUse":       [{ "matcher": "AskUserQuestion",
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

`timeout` must outlast the shim's longest park — 570 s on `/v1/question`, itself outlasting the 540 s server ceiling (§5.1). 600 s is both Codex's own default (uncapped) and what the Claude Code entry now uses, so the two agents behave identically. Command hooks run through `$SHELL -lc`, so the shim path is shell-quoted — SINGLE quotes, not double: inside double quotes `$`, backticks and `$(…)` are still live, so a bundle path containing `$` would resolve elsewhere and silently kill the integration (the shim fails open, so nothing reports it), and backticks would be executed on every agent event. Changing the emitted command string re-triggers Codex's trust gate below.

Installer file discipline (both agents): every write goes to a temp file that
is chmod'd to the TARGET's mode before a single byte is written, fsync'd, then
renamed — so installing never widens a `chmod 600` config and never leaves
secrets in a world-readable temp. A `<file>.pingmybell.bak` exists exactly
while PingMyBell is installed in that file; a completed uninstall discards it.
An uninstall that finds nothing of ours leaves the file BYTE-IDENTICAL rather
than reformatting a config we never wrote into, and neither install nor
uninstall may delete a hook group the user wrote and emptied themselves.

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

**Gating: `gate_codex_approvals`, three-state, default `auto` — see §5.2.3.** Not a latency question (Codex only fires this hook where it was already stopping to ask) and not really a safety one either. The real cost is *place*: an approval is a decision about a command whose reasons are on screen in the agent, and moving it to a card that must be cleared is a downgrade unless the user had already told Codex to interrupt them about everything. So the default mirrors their Codex setting instead of overriding it. Questions still ignore the setting entirely (§5.2.1); they are a different thing.

**`tool_name` is allowlisted to `Bash` and `apply_patch`.** Our matcher selects only those, but a widened matcher — or a future Codex routing another tool through `PermissionRequest` — must not let the overlay grant permission for something the card cannot summarize (`tool_summary` would fall through to raw JSON). Anything unrecognized falls through to Codex's own prompt, the same stance the question path takes by pinning `request_user_input`.

**The hook does not fire at all under auto-approval.** `permission_request` runs from Codex's approval path, which is only entered when the tool call resolves to `NeedsApproval`. Verified: under `approval_policy = "never"` (what `codex exec` uses by default, and the moral equivalent of `bypassPermissions`) the same command ran with no hook invocation whatsoever. A known-safe read-only command under `approval_policy = "untrusted"` likewise auto-approves and never reaches the hook. So a user in full-auto pays exactly nothing, with or without the flag.

**Park budget: the approval budget, not the question one.** An approval is a two-second yes/no, so it nests one rung shorter all the way down and is *not* extendable — 110 s park (`APPROVAL_TIMEOUT_SECS`) < 115 s shim read (`APPROVAL_READ_TIMEOUT`) < 120 s hook timeout, identical to Claude's `Bash|Write|Edit|MultiEdit` matcher. `park_budgets_nest_inside_the_hook_timeout` asserts both ladders at compile time.

**Session identity**: `codex-<fnv(cwd)>`, the SAME key the notify and question paths use, so an approval lands on the same board row as that project's questions and turn-completes.

**Card and voice**: `tool_summary` already read `input.command` for `Bash`, so exec approvals needed nothing. `apply_patch` gets an arm that reduces the raw patch to the files it touches (`Add canary.txt`, `Update src/a.rs, Delete b.txt`) — 400 characters of diff is useless on a one-line card — and `speakable_tool` voices it as "a file edit".

> ⚠️ Same trust gate as §5.2.1, and trust is **per hook**: the user must approve BOTH entries in ChatGPT → Settings → Hooks. Installing approvals rewrites `hooks.json`, which re-triggers review for the question hook too.

#### 5.2.3 Deciding *when* to intercept an approval — mirroring the user's Codex setting

Approvals shipped as a boolean and the boolean was wrong in both positions. On by default the user was pulled to the notch for `bun install` all evening; off by default the feature never fires for the person who would benefit. The setting that actually predicts the answer is one the user has already made, inside Codex:

| ChatGPT app setting | what it means | PingMyBell should |
|---|---|---|
| **Approve for me** | auto-approve what's safe, escalate only the unsafe | **not intercept** — they said don't interrupt me; do not override that |
| **Ask for approval** | ask me about everything | **intercept** — they opted into being asked, so answering from the notch beats context-switching |
| full auto | never ask | moot — the hook does not fire (see above) |

So `gate_codex_approvals` is three-state: `"auto"` (**default**, the table above), `"always"`, `"never"`. The key shipped as a bool and existing configs are read unchanged — `true` ≡ `always`, `false` ≡ `never` — so a live install keeps its current behavior until the user picks something. Tray: a submenu, "Match My Codex Setting (recommended)" / "Always" / "Never". `gate_tool_calls` (Claude) is a separate flag and is untouched.

**The signal is NOT `permission_mode`.** Verified against codex-cli 0.146.0-alpha.9.2 in an isolated `CODEX_HOME` (deleted afterwards), a hook logging its stdin verbatim, one `codex exec` run per policy:

| `approval_policy` | `PermissionRequest` fires? | payload `permission_mode` |
|---|---|---|
| `untrusted` | yes | `default` |
| `on-request` | yes | `default` |
| `on-failure` (alias of `on-request`) | yes | `default` |
| `never` | **no** — `PreToolUse` only | `bypassPermissions` |

Codex's `hook_permission_mode` collapses `UnlessTrusted | OnRequest | Granular` to `"default"` and only `Never` to `"bypassPermissions"`, so the field cannot separate "ask me about everything" from "bother me only when it's unsafe". Nor can anything else in the payload: the two ChatGPT settings move `approvals_reviewer` (`user` = "Ask for approval", `auto_review`, alias `guardian_subagent`, = "Approve for me"), and `PermissionRequestCommandInput` is `deny_unknown_fields` with no such member.

**What the payload does carry is `transcript_path`.** The session rollout's `turn_context` records hold `approval_policy` and `approvals_reviewer` verbatim, and are already flushed when the hook fires — verified by a hook that read the file at hook time and saw the right pair for every run, including a `never`/`user` control. So `auto` reads the rollout:

```
intercept  ⇔  approval_policy ∈ {untrusted, on-request}
              ∧ (approvals_reviewer == "user"                 // the axis the app moves
                 ∨ (approvals_reviewer absent ∧ policy == "untrusted"))   // pre-reviewer Codex
```

Everything else — `never`, `granular`, an unknown policy, a reviewer that is present but not a readable string, a missing/unreadable/garbage rollout, no `turn_context` in the scanned window — is **false**: no evidence of consent is not consent, and declining is the fail-open direction (no stdout, exit 0, Codex runs its own prompt). `on-failure` never appears here: it is a serde *alias* of `OnRequest`, and a run started with `-c approval_policy=on-failure` records `"approval_policy":"on-request"` in the rollout (verified).

`last_turn_context` reads a bounded 2 MiB tail (the largest rollout observed is ~6 MB across dozens of turns; `turn_context` is written once per user turn, so the newest is at the end) and takes the **newest** record, so switching modes mid-session takes effect on the next approval. It stats the path first and reads **regular files only** — opening a FIFO blocks in `open()` until a writer appears, and a hung hook is strictly worse than not parking. Every guard on this path is mutation-tested: deleting any one of them fails a test.

**Cost.** The load-independent number, `last_turn_context` measured in-process (min over 300 iterations, release build): **0.38 ms** for a 0.4 MB rollout, **2.0 ms** at the 2 MiB cap — and that cap is the ceiling, so no rollout can cost more. End to end, against the real release shim with `HOME` on a temp dir holding no port/token (60 runs each, idle machine):

| setting | median | p90 |
|---|---|---|
| `never` — decided before stdin | 1.71 ms | 1.89 ms |
| `always` | 1.69 ms | 1.80 ms |
| `auto`, declines, 0.4 MB rollout | 2.01 ms | 2.17 ms |
| `auto`, declines, 6.4 MB rollout | 4.52 ms | 4.64 ms |

(Re-run these on an idle machine: at load ~18 every row, `never` included, inflates to 8–18 ms, because process startup rather than the read dominates.)

`never` returns *before* reading stdin — Codex's hook runner tolerates the broken pipe — so it is untouched. `auto` cannot take that exit: its decision needs the payload. It costs **+0.3 ms** on a typical rollout and at most ~2 ms on any rollout, on a hook that only fires when Codex has already stopped and is waiting for a human. The rollout read is deliberately the **last** check in `codex_approval_body`, after the event guard, the `permission_mode` guard, the `tool_name` allowlist and the `tool_input` shape check, so the only path that touches the filesystem is one that was otherwise going to park.

`never` is enforced inside `codex_approval_body`, not only by the fast-path return in `run_codex_approve` — that return is an optimisation, and this channel can *grant* permission, so the opt-out must not rest on one early exit.

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
CREATE INDEX idx_events_session ON events(session_id);  -- every hot query keys on it
```

Settings do NOT live here. They are `~/.pingmybell/config.json`, because the
shim reads them on every hook invocation and cannot link SQLite.

Rows are swept once a day: a session whose last event is over 30 days old is
deleted along with its history, EXCEPT while the running process is still
tracking it (see `Registry::prune`). The board only ever shows the last 24 h,
so nothing visible is ever removed.

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
8. **Codex lifecycle over hooks** (§11.1): capture → verify payloads → shim subcommands + installer rows → uninstall notify. ✔ gate in §11.1.
9. **Templates + rate/volume** (§11.2): closes AC-4.2/4.3. ✔ gate in §11.2.
10. **Focus-aware quieting** (§11.3). ✔ gate in §11.3.
11. **Waiting metrics + escalation** (§11.4). ✔ gate in §11.4.
12. **Activity ticker** (§12.1): PostToolUse capture → ephemeral `activity` events → live row labels. ✔ gate in §12.1.
13. **Triage hotkey** (§12.2): global shortcut → oldest-waiting jump with cycling. ✔ gate in §12.2.
14. **Phone push, user-owned endpoint** (§12.3): amends §9 deliberately; away-detection gates it. ✔ gate in §12.3.
15. **Third adapter** (§12.4): Gemini CLI via the §11.1 playbook; the real cost is the AgentKind fan-out. ✔ gate in §12.4.
16. **Morning digest** (§12.5): after step 11; first-event-of-day trigger, once daily. ✔ gate in §12.5.

Testing notes: unit-test normalizers and template engine with recorded fixture payloads (`bun test` for UI stores, `cargo test` for core); integration script that replays fixture events against a running app; manual matrix in §7 of PRD (success criteria).

## 11. Next: designs for steps 8–11

Ordered: 8 is a live breakage, 9 closes promised PRD scope, 10–11 are the
retention features. Each lands separately with its own gate.

### 11.1 Step 8 — Codex turn lifecycle over hooks, and OFF notify

**Why now.** Two compounding failures. (a) notify has no turn-start, so a
Codex session that is actively working reads "done" from its previous turn —
the status lie the board exists to prevent. (b) The notify slot is
single-occupancy and CONTESTED: on 2026-08-04 the Codex Computer Use app
(SkyComputerUseClient) made itself the notify program and folded our entry
into a `--previous-notify` argument its binary shows no evidence of ever
invoking. Our Codex events stopped that minute. Any future tool can do the
same; hooks compose as arrays and cannot be evicted.

**Verified against the installed codex** (ChatGPT.app-embedded, framework
150.0.7871.182): the binary knows `SessionStart`, `TurnStart`,
`UserPromptSubmit`, `Stop`, `SessionEnd`, `PostToolUse` hook events, and its
payload vocabulary carries `hook_event_name`, `thread_id`, `turn_id`,
`last_assistant_message`, `stop_hook_active`, `transcript_path`. Exact
payload shapes are being confirmed empirically before any code: temporary
capture entries in `~/.codex/hooks.json` append each event's stdin to
`~/.pingmybell/codex-hook-capture.jsonl` (0600). DO NOT implement from the
names alone — same rule as the Claude shim.

**Design.**
- `installers/src/codex.rs` HOOKS gains four rows: `SessionStart` →
  `codex-session-start`, `UserPromptSubmit` → `codex-prompt-submit`, `Stop`
  → `codex-stop`, `SessionEnd` → `codex-session-end`. NOT `TurnStart`: the
  string exists in the binary but this build's loader silently drops the
  event (it never appears in the hook-review UI; the "1 issue" banner turned
  out to be the harmless SessionEnd timeout clamp, not this). The UI's own
  descriptions confirm the boundaries we need anyway (UserPromptSubmit "when
  the user submits a prompt", Stop "right before ChatGPT ends its turn"),
  matching the Claude integration one-for-one.

  **Captured 2026-08-04** (SessionStart + UserPromptSubmit, real payloads):
  fields are `hook_event_name`, `session_id` (a UUID, stable across both
  events of the one captured turn), `cwd`, `transcript_path`, `model`,
  `permission_mode`, plus `prompt` (raw user text — dropped in the shim, §9)
  and `turn_id` from the prompt on. Identity STAYS `codex-<cwd hash>`: the
  2026-07 observation in shim comments records the envelope id as only
  turn-stable, and one captured turn cannot refute that. Follow-up: capture
  two turns in one session; if `session_id` holds, migrate every codex
  channel to it and stop sharing a row between two sessions in one cwd.
  Still unconfirmed: whether an ERRORED turn fires `Stop`, and whether Stop
  carries `last_assistant_message` — its absence costs the spoken sentence,
  not the event (test pins this). Shim maps them to the
  normalized events with `agent: codex`. Terminal info is captured at
  session-start (today it is captured at turn-complete, i.e. too late for
  the first jump).
- Session identity: keep `codex-<cwd hash>` unless capture shows a stable
  `thread_id` in every payload — switching ids orphans existing rows, so it
  must be all-or-nothing and decided from evidence.
- `Stop` is expected to carry `last_assistant_message` (Claude's does; the
  strings are present). Once capture confirms it, notify is REDUNDANT:
  uninstall it and the Sky fight is escaped entirely. Until then both fire —
  registry writes two `turn_complete` rows and the speaker's 5 s dedup keeps
  it silent; acceptable for the transition only.
- Chaining hazard while notify still exists: the installer chains any
  pre-existing program, and Sky's entry now EMBEDS our shim in its
  `--previous-notify` JSON. Chaining Sky verbatim could invoke us twice per
  turn. The installer must strip a `--previous-notify` whose payload names
  our shim before chaining.
- Trust gate: every new hook starts untrusted; the user approves each in
  Codex's hook-review UI. Install messaging must say so (it already does).

**Gate:** run one Codex turn. Board reads working DURING the turn and done
after, with the summary spoken once. Kill Codex mid-turn: the session ages
out as unknown rather than sticking at working. Uninstall notify; a further
turn still reports everything.

### 11.2 Step 9 — callout templates + rate/volume (closes AC-4.2/4.3)

- `speaker.rs` gets `fn callout(style, kind, agent, project, summary) ->
  String` — a pure function over an enum of the three built-in styles
  (terse / conversational / status-only), unit-tested like the voice
  selection rules. `completion_text`/`attention_text`/`decision_text`
  collapse into it.
- Config keys: `speech.style` (global), `speech.<agent>.rate` (0.5–2.0),
  `speech.<agent>.volume` (0.0–1.0). Rate/volume clamp at read time; the
  worker applies them per utterance via the tts crate's set_rate/set_volume,
  normalized per platform (the crate's normal/min/max differ by backend —
  verify on macOS before trusting the numbers).
- Settings UI: one style selector plus two sliders per agent, under the
  voice picker; preview reuses `preview_voice`, which must apply
  style+rate+volume so what you hear is what you get.
- **Gate:** switch styles → next callout changes shape; set rate 1.5 →
  audibly faster; both survive an app restart (config store is atomic).

### 11.3 Step 10 — focus-aware quieting

The app's core risk is becoming the thing you mute. If the session's own
terminal is frontmost you are already looking at it; announcing it is noise.

- Decision at SPEAK time, in the worker (already off-main): completions and
  attention callouts downgrade to a short chime when the utterance's session
  terminal app is frontmost. Approvals always speak — they are the product.
- Frontmost check: session `terminal_json` pid (captured at session-start)
  resolved to its host app once, compared against
  `NSWorkspace.frontmostApplication` pid (macOS) /
  `GetForegroundWindow`→pid (Windows). Cheap, read-only, cached per
  utterance. Never on the main thread; never blocks — an error means SPEAK
  (fail loud, the safe direction for a notifier).
- Config: `quiet.focus_aware` (default ON), `quiet.hours` ("22:00-08:00",
  default off), per-session mute toggled from the board row (in the registry
  so it survives restart; cleared when the session ends).
- **Gate:** completion with the terminal frontmost chimes; ⌘-tab away, next
  completion speaks. Approval speaks in both cases.

### 11.4 Step 11 — waiting-on-you metrics + one escalation

The events table already holds every transition; this is a query, not a
schema change.

- Definition: a waiting span opens at a `needs_attention`/`permission_request`
  row and closes at that session's next decision or event. Computed in
  `registry.rs` (SQL over `idx_events_session`), exposed on `BoardRow` as
  `waiting_secs` plus a 7-day per-session total. UI renders numbers it is
  handed — no derivation in Svelte (§project rule).
- Board: "waiting 4m" on the row while amber; weekly total in the header
  ("you kept agents waiting 47m this week").
- Escalation: an attention pin older than `quiet.remind_after_secs`
  re-announces ONCE ("still waiting in {project}"), then never again for
  that pin. Off by default. Driven from the existing prune/timer machinery,
  not a new poll loop (AC-5.5).
- **Gate:** park a session 2 minutes → row shows the wait; with reminders on,
  exactly one repeat announcement fires; with them off, none.

## 12. Designs for steps 12–16 (the league-changers)

Same contract as §11: each lands separately, each has a gate, and NOTHING
that talks to an external surface is implemented from documentation or
binary strings — capture real payloads first (§11.1 proved why: the codex
binary advertises `TurnStart`; its loader drops it).

### 12.1 Step 12 — live activity ticker

A working session is a silent dot for the whole turn. `PostToolUse` can
narrate it: "Bash: cargo test", "Edit: registry.rs", changing as the agent
works. The difference between a status light and a cockpit — you catch an
agent editing the wrong tree WHILE it happens.

- **Verify first**: register a capture entry for `PostToolUse` against the
  installed claude (same `/bin/cat >>` rig as §11.1) and read the real
  payload before writing the mapper. Codex lists `PostToolUse` in its
  loader too — same capture step, separately, and only after Claude ships.
- Shim subcommand `posttool`, installer row with NO matcher (all tools; the
  ticker is exactly for the tools we do not gate — verified 2026-08-05 against
  claude 2.1.198, where `Read`, `ToolSearch` and `TaskCreate` all arrived
  through a matcher-less row). Payload reduced in the shim to `tool_name` plus
  ONE label: the file's basename for edit-shaped tools, and for Bash the
  PROGRAM — not "the first token", which is the credential in
  `AWS_SECRET_ACCESS_KEY=… npm run deploy`; leading `NAME=value` assignments
  are skipped, the program is basenamed like any other path, and anything
  still carrying shell punctuation is dropped. Never arguments, never content
  — §9 invariant 4 applies to the ticker exactly as it does to summaries, and
  the label passes `sanitize_capped(_, 48)` in the core.
- **Its own route, `POST /v1/activity`, not an `event` kind** (the design said
  a new normalized event; this is the one deviation). An activity must never
  be persisted, and a type `Registry::apply` cannot accept is what guarantees
  that rather than a code review. For the same reason it emits its OWN
  `session-activity` event rather than `session-updated`: the board answers
  that one by re-pulling the whole snapshot and reloading the open history
  drawer, which for a change that writes no history would be the most
  expensive no-op in the app.
- The shim READS the response, on a 400 ms budget. Writing and walking away
  looks free and is not: axum drops a handler whose client has gone — the same
  cancellation `/v1/approval` relies on deliberately — so a shim that exited
  the instant the bytes were written raced the server into it and the ticker
  never fired at all, with every unit test still passing. The saving was
  0.7 ms.
- **Ephemeral by design**: `activity` updates `Session.last_activity`
  (memory only) and emits `session-updated`; it writes NO events row and
  never reaches the speaker. A busy turn is hundreds of tool calls —
  persisting them would swamp a table sized for lifecycle events, and
  activity is worthless after restart anyway (recovery leaves it None).
- Coalesce at ingest: at most one emit per session per 500 ms; the newest
  label wins. Parallel tool bursts must not turn the webview into a strobe.
- Board and expanded-island rows show `last_activity` while state is
  `working`, falling back to `last_summary` otherwise. UI renders the
  string it is handed (§project rule).
- **Gate**: a real claude turn shows changing labels on the row; `done`
  swaps back to the summary; `SELECT COUNT(*) FROM events` grows by the
  lifecycle events ONLY; nothing is spoken.

### 12.2 Step 13 — triage hotkey ("who needs me next")

With one agent a board is enough. With eight, the only question is "who has
waited longest?" — one global hotkey answers it, turns monitoring into a
workflow, and completes jump-to-session's reason to exist.

- `tauri-plugin-single-instance`'s sibling `tauri-plugin-global-shortcut`
  (official, v2). Default chord `Ctrl+Alt+Space`, configurable as
  `hotkey.next` in config.json. Registration failure (chord taken) must
  degrade gracefully: log, show the failure in board settings, never crash.
- Behavior: focus the `needs_attention` session with the OLDEST
  `last_event_at`, via the existing `focus::jump` on `spawn_blocking`.
  Jumping does not clear the state (answering does), so repeated presses
  must cycle: keep a `recently_jumped: HashMap<SessionId, Instant>` with a
  10 s TTL and skip entries inside it. No waiting sessions → the island
  shows a quiet "all clear" toast; nothing is spoken.
- Registry gains one read helper (`oldest_waiting(skip: &[...])`); the
  decision lives in Rust, the hotkey handler is dumb.
- **Gate**: park two sessions; press → oldest's terminal focused; press
  again → the other; answer both, press → "all clear". The overlay never
  takes focus (invariant 3 untouched — the TERMINAL gets focus, via the
  same path clicking a row uses).

### 12.3 Step 14 — phone push to a USER-OWNED endpoint

The moment you stand up, the product's value evaporates — and that is when
the 20-minute turn finishes. This is the most-requested feature this app
will ever have; shaping it now, on privacy-first terms, beats bolting it on
under demand later.

- **This amends §9 invariant 3** and must be written there as the single,
  explicit exception: one outbound POST, opt-in, OFF by default, to a URL
  the USER supplies (self-hosted or hosted ntfy, Gotify, any webhook).
  There is no vendor server and never will be; an empty `push.url` means
  the code path does not exist at runtime.
- Config: `push.enabled` (false), `push.url`, `push.events`
  (default `attention,approval` — completions opt-in), quiet hours shared
  with §11.3's `quiet.hours`.
- **Away-detection is what makes it humane**: at the desk the voice speaks,
  away the phone buzzes — never both. Push only when the Mac has been
  input-idle > `push.after_idle_secs` (default 120, via
  `CGEventSourceSecondsSinceLastEventType` — a LOCAL query) or the screen
  is locked. The speaker path stays untouched; the push task taps the same
  utterance stream after the mute/dedup logic, so the payload is EXACTLY
  the sentence the voice would have spoken — already cleaned, already
  capped, nothing else. No ids, no paths, no raw text.
- Plumbing: one background task fed by a channel; blocking HTTP client
  (`ureq`, matching the shim's no-async posture) with a 5 s timeout,
  fire-and-forget, failures logged at debug and DROPPED — a notification
  system must never retry-storm someone's phone. Settings UI: URL field +
  "send a test push" button.
- **Gate**: with a phone subscribed to a self-hosted topic — lock the
  screen, finish a turn → phone shows the exact spoken sentence; unlock,
  finish another → voice only, no push; `push.enabled=false` → `lsof`
  shows zero outbound sockets from the app over a full session.

### 12.4 Step 15 — third adapter (Gemini CLI first, opencode fallback)

Every adapter multiplies who the app is for, and it is the only test of
whether the adapter seam is real or a two-example coincidence. The §11.1
playbook is the spec: discover the installed CLI's hook/notify surface →
capture rig → map from OBSERVED payloads → installer with exact-shape
ownership tails → fixtures from the captures.

- The honest cost is the enum: `AgentKind` gains a variant, which fans out
  to `as_str`/`from_str`, the board/overlay glyph (`AgentMark`), speaker
  defaults (`pick_defaults` generalizes from a pair to one distinct voice
  per agent, preserving the never-collide rule), config voice keys (already
  keyed by agent string), and the installer trio. Budget most of the work
  there, not in the adapter.
- Blocked on the CLI being installed on the dev machine — same rule as
  everything else: no implementation against docs.
- **Gate**: a real session of the new agent shows lifecycle on the board,
  speaks completion in its OWN default voice, and survives the §11.1
  eviction test (its config channel is either compositional or the chaining
  hazard is documented).

### 12.5 Step 16 — the morning digest

"Yesterday: 14 sessions, 9 finished, 3 approvals, and you kept agents
waiting 47 minutes — longest, bc9." One spoken sentence and a small board
panel. The app stops being about the agents' day and starts being about
YOURS. Depends on §11.4's waiting-span SQL; build after step 11.

- Trigger: the FIRST ingest event of a local calendar day (that is the
  moment the user actually sat down), with an app-launch catch-up like the
  prune task. Fires once per day: `digest.last_spoken_day` in config. All
  aggregation in `registry.rs` over data the 30-day retention already
  keeps: session count, completions, approval allow/deny counts, total and
  longest waiting spans, busiest project by event count.
- Spoken through the normal utterance path (so mute, voice choice, and
  §11.2 templates all apply) at `Attention` priority — it should never
  preempt an approval. Board gets a dismissible "Yesterday" card above the
  rows; UI renders numbers Rust hands it.
- `digest.enabled` defaults ON — it is the soul of the thing — but the
  first spoken line must say "say 'mute digest' in settings to turn this
  off"... no. Keep it simple: the board card carries the toggle. Weekends
  aggregate since Friday ("since Friday: ...") rather than reporting a
  silent Saturday.
- **Gate**: seed yesterday's events in a test DB → exact digest text
  asserted (template-aware); live: first event after midnight speaks it
  once, the second event stays silent, the card dismisses and stays gone.

Build order: 12 and 13 are independent and small-to-medium; 14 needs the
§9 amendment reviewed deliberately; 15 is gated on an installed CLI; 16
waits for 11. Recommended: 12 → 13 → 16 → 14 → 15.
