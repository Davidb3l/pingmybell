# Contributing to PingMyBell

Thank you for considering a contribution. PingMyBell is a small project with a single maintainer; that means PRs get reviewed personally and merged when they are right.

## Code of conduct

This project adopts the [Contributor Covenant 2.1](CODE_OF_CONDUCT.md). Report violations to `dev@getsothis.com`.

## Developer Certificate of Origin (DCO)

Every commit must be signed off:

```sh
git commit -s -m "your message"
```

The sign-off line (`Signed-off-by: Your Name <you@example.com>`) certifies that you wrote the change or otherwise have the right to submit it under the project's MIT license. See [developercertificate.org](https://developercertificate.org) for the full text. We do not require a CLA.

## Development environment

PingMyBell is a Tauri v2 desktop app: a Rust core, a Svelte 5 + Vite frontend, and a separate hook-shim binary. You need [Bun](https://bun.sh) and Rust stable; install [rustup](https://rustup.rs/) if you don't have it.

The cargo workspace has three crates:

- **`src-tauri/`** — the app: ingest server, session registry, voice engine, overlay, and the agent adapters.
- **`shim/`** — the hook shim the agents actually invoke. It has one dependency (`serde_json`, plus `libc` on Unix) and must stay that small.
- **`installers/`** — the merge-and-write logic for `~/.claude/settings.json` and `~/.codex/config.toml`.

Bun is the only JS tooling. There is no npm, pnpm, or Node in the dev loop or in a shipped artifact.

Typical dev loop:

```sh
bun install
bun run tauri dev              # the app, with the frontend on Vite
```

The app keeps its config, token, and SQLite state in `~/.pingmybell/`. A dev build and an installed release build will fight over the port and token files, so stop the released app before running `tauri dev`.

## Tests

CI (`.github/workflows/ci.yml`) runs these on every pull request and on pushes to `main`; run them locally before opening a PR:

```sh
bun run build                                      # vite build
bun run sidecar                                    # build + stage the shim binary
bunx svelte-check --tsconfig ./tsconfig.json       # 0 errors, 0 warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`ci.yml` is the source of truth for what gates a PR; run the list above and you will be ahead of it. Note that `cargo test` needs `bun run sidecar` to have staged the shim first — from a clean checkout it fails otherwise, because the Tauri build script expects that binary.

On Windows CI runs `bun run build`, `bun run sidecar` and `cargo check --workspace`. Windows-specific *behavior* is often deferred to a Windows machine, but the code must always compile for both targets.

Clippy is worth a word, because it is easy to misread a green run: it compiles crate by crate and **stops at the first failure**, so one unfixed lint hides every later one. Three had accumulated in this repo for exactly that reason. If you fix a lint and new ones appear, that is why — they were always there.

## Invariants that are not negotiable

A PR that breaks one of these will not merge, however good the rest of it is. They are the reasons the app is safe to leave running.

1. **The shim fails open.** Any internal error, or the app not running, means exit 0 with no stdout. A broken or absent PingMyBell must never block or alter what an agent does.
2. **The ingest server is loopback-only** (`127.0.0.1`, random free port) and bearer-token authenticated, and it must never crash on bad input.
3. **The overlay never takes keyboard focus** (`focusable: false`, `WS_EX_NOACTIVATE` on Windows). Focus theft is a release blocker.
4. **No outbound network calls.** The app opens no sockets off the loopback interface. The one carve-out is the opt-in weekly dependency review, which is off by default and works by spawning `bun outdated` / `pnpm outdated` as child processes in repositories you nominate.
5. **Decisions live in Rust.** The Svelte side renders registry snapshots delivered over Tauri events and sends commands back; it does not decide state transitions.

`PRD.md` has the scope and acceptance criteria, and `ARCHITECTURE.md` has the stack, schemas, and build order with a verification gate per step. Read both before a substantive change.

## Pull request process

1. Open an issue first if your change is non-trivial — saves you wasted work if the direction is wrong.
2. Branch from `main`. Keep branches focused; one logical change per PR.
3. Add tests for behavior changes. Prefer a pure function with a test over behavior only reachable by running the app.
4. Run the checks above.
5. Update the docs where behavior changed: `README.md`, `PRD.md` / `ARCHITECTURE.md` where a contract moved, and `CHANGELOG.md` under "Unreleased" for anything user-visible.
6. Sign off every commit (`git commit -s`). A CI check rejects pull requests with unsigned commits.
7. Open the PR with the template.

The maintainer aims to respond within a few days. If the PR is not the right fit, you will hear why and what would change that.

## Questions

See [SUPPORT.md](SUPPORT.md) for where to ask.

## Reporting bugs

Use the bug report issue template. Include your OS and version, whether you built from source or ran a dev build, which agent integration was involved (Claude Code hooks or the Codex `notify` hook), and the app's log output.

## Security issues

Do not open a public issue. See [SECURITY.md](SECURITY.md).
