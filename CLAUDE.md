# CLAUDE.md — PingMyBell

Read `PRD.md` (scope, acceptance criteria) and `ARCHITECTURE.md` (stack, schemas, build order) before any implementation work. Build strictly in the order of ARCHITECTURE.md §10 — each step has a verification gate.

## Project rules

- Stack is fixed: Tauri v2 + Rust core, Svelte 5 + Vite frontend, Bun for all JS tooling (`bun install`, `bun run`, `bun test`), separate `shim/` cargo crate for the hook shim. No Node-based tooling; no JS runtime in shipped artifacts.
- The shim must always fail open: any error → exit 0, no stdout. This is a hard invariant (PRD AC-2.4).
- Ingest server: loopback only, bearer token, never crash on bad input (PRD FR-1).
- The overlay window must never take keyboard focus (`focusable: false`, `WS_EX_NOACTIVATE` on Windows). Treat focus theft as a release-blocking bug.
- No outbound network calls anywhere except the optional Tauri updater.
- Before implementing the Claude Code shim, verify hook event names and PreToolUse output schema against https://code.claude.com/docs/en/hooks for the locally installed version; update ARCHITECTURE.md §5.1 if it differs.
- Keep UI dumb: all state transitions and decisions live in Rust; Svelte renders registry snapshots delivered via Tauri events and sends commands back.

## Commands (once scaffolded)

- `bun install` — JS deps
- `bun run tauri dev` — run app in dev
- `cargo test --workspace` / `bun test` — core / UI tests
- `bun run tauri build` — release artifacts

## Definition of done per step

The verification gate listed for that step in ARCHITECTURE.md §10 passes on macOS; Windows-specific gates may be deferred to a Windows machine but code must compile for both targets (`cargo check --target x86_64-pc-windows-msvc` via CI).

<!-- hayvenhurst:reflex -->
## Code navigation: prefer `hayven` over grep

This repo is indexed by Hayvenhurst. To find code, reach for `hayven` FIRST:
- `hayven query "<natural language or identifier>"` — semantic/identifier search over the code graph (faster and higher-signal than grep; never returns empty on a real query).
- `hayven neighbors <id>` — callers/callees of a node (follow the call graph instead of guessing).
- `hayven view` — open the browser graph.
Fall back to grep only when hayven has no answer. Run `hayven reindex` after large changes if results look stale.
<!-- /hayvenhurst:reflex -->
