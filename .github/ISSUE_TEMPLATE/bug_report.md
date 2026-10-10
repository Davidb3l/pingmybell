---
name: Bug report
about: Something is broken in the app, the hook shim, or one of the agent integrations.
title: "[bug] "
labels: bug
assignees: ''
---

<!--
Thanks for taking the time to file a bug. The more concrete detail you can
give us, the faster we can reproduce it.

Please do not paste the contents of your agent sessions. The app derives
session titles and summaries from your actual work, so a log excerpt can
carry your code or your prompts in it — redact, or describe instead.

If the bug is that PingMyBell BLOCKED or CHANGED what an agent did, say so
first and plainly. The shim is supposed to fail open on every error, so that
class of bug outranks everything else here.
-->

## Environment

- **OS + version** (macOS 14.5, Windows 11 23H2, ...):
- **Commit you built from** (`git rev-parse --short HEAD`):
- **How you ran it** (`bun run tauri dev` / a `bun run tauri build` bundle):
- **Which agent** (Claude Code hooks / Codex CLI `notify` / the suite event spine):
- **Where it happened** (voice callout / the island overlay / the board / approvals / install command):

## What happened

A clear and concise description of the bug.

## Steps to reproduce

1.
2.
3.

## Expected behavior

What you expected to happen.

## Actual behavior

What actually happened. Include exact error messages where possible.

## Logs

Run the app from a terminal to capture its log output, and paste the relevant
lines (redact anything from your own work):

```
<paste output here>
```

## Additional context

Screenshots or a screen recording help a lot for overlay and focus bugs —
especially anything about the island taking keyboard focus, which is a release
blocker. Anything else you think might help.
