<!--
Thanks for contributing to PingMyBell. Please fill in the sections below.
Every commit in this PR must be signed off via `git commit -s` per our DCO
policy (see CONTRIBUTING.md).
-->

## Summary

What does this change do, and why?

## Related issue

Closes #<issue-number> (or "n/a" if this is a small drive-by fix).

## Changes

- Bullet list of the meaningful changes in this PR.
-
-

## Checklist

- [ ] Tests added or updated (`cargo test --workspace`).
- [ ] `bun run build`, `bun run sidecar`, and `bunx svelte-check --tsconfig ./tsconfig.json` are clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] It still compiles for Windows (`cargo check --workspace --target x86_64-pc-windows-msvc`, or let CI's Windows job tell you).
- [ ] Docs updated where behavior changed (`README.md`, and `PRD.md` / `ARCHITECTURE.md` where a contract moved).
- [ ] User-visible changes are added to `CHANGELOG.md` under "Unreleased".
- [ ] All commits are signed off (`git commit -s`).
- [ ] CI is green on this PR.

## Invariants

Confirm this change does not break any of these (CONTRIBUTING.md has the full
list and the reasoning). If it touches one, explain how in "Notes" below.

- [ ] The shim still fails open — every error path exits 0 with no stdout.
- [ ] The ingest server is still loopback-only and bearer-authenticated, and still does not crash on bad input.
- [ ] The overlay still never takes keyboard focus.
- [ ] No new outbound network calls, and no new HTTP client in the dependency tree.
- [ ] State transitions still live in Rust, not in Svelte.

## Breaking changes

List any breaking changes to the hook contracts, the ingest API, the
`~/.pingmybell/` on-disk layout, or the settings the installers write into
`~/.claude/settings.json` and `~/.codex/config.toml`. If there are none, write
"None".

## Notes for reviewers

Anything you want a reviewer to look at especially carefully (tricky logic,
unusual dependencies, places you're unsure about). Platform-specific code that
you could not run on the other platform belongs here.
