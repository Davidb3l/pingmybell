# Security Policy

PingMyBell is a local tool: a desktop app that listens on loopback for hook events from the AI coding agents running on the same machine, speaks them aloud, and shows them in a notch overlay. It has no cloud service, no accounts, and no telemetry. Security reports are welcome and taken seriously.

## Supported versions

There are no releases yet. PingMyBell is pre-alpha and built from source, so the only supported version is current `main`. When releases begin, this section will name the supported line.

## Reporting a vulnerability

**Do not open a public issue.** Report privately through GitHub: open the repository's [Security tab](https://github.com/Davidb3l/pingmybell/security) and choose **Report a vulnerability** (GitHub's private vulnerability reporting). Include your OS and version, the commit you built from, and steps to reproduce.

We commit to:

- Acknowledging your report within **48 hours**.
- A coordinated disclosure window of **90 days** from acknowledgment.
- Crediting reporters in the release notes (unless you prefer to remain anonymous).

There is no bug bounty.

## What a release provides

**Nothing yet — there are no releases.** There are no prebuilt binaries, no installers, no checksums, no signatures, and no build-provenance attestations, because nothing has been published. You build the app yourself from source, as the README describes, and the only integrity guarantee is the one you get from your own `git clone` and from Cargo checking each crate against the committed `Cargo.lock`.

The [`pingmybell`](https://crates.io/crates/pingmybell) and [`pingmybell-shim`](https://crates.io/crates/pingmybell-shim) names on crates.io are placeholders, not the app. Do not treat anything published under them as a PingMyBell release.

When releases do begin, this section will state exactly what each install path verifies. Until then, treat any "PingMyBell release" you did not build yourself as not ours.

## Threat model

What PingMyBell is designed around, and how far each defense goes today.

- **A broken notifier must not break your agents.** The hook shim fails open: any internal error, a missing app, an unreadable token, a timeout — all of them exit 0 with no stdout, which leaves the agent to its own behavior. This is the project's first invariant precisely because the shim sits in the path of every tool call. For the approval feature specifically, a hook timeout or an app that is killed mid-prompt falls back to the agent's own permission prompt rather than denying or hanging.

- **The ingest server is loopback-only and authenticated.** It binds `127.0.0.1` on a random free port and accepts nothing from another machine. Each install generates a fresh 32-byte random bearer token, rendered as 64 hex characters. A presented token of the right length is compared in constant time, so a near-miss guess leaks nothing by timing; the length itself is not a secret, since it is always the same. The port and the token are written to `~/.pingmybell/port` and `~/.pingmybell/token`, in a directory created `0700`, with the files `0600`, so no other user account on the machine can read them. On Windows this relies on the default ACL of your user profile rather than on explicit permissions.

- **Any local process running as you can use the server.** This is the real residual risk and it is not fixable by a token kept on the same filesystem. A process running as your user can read `~/.pingmybell/token`, and with it can inject fake session events, make the app speak arbitrary text, and — where approval gating is enabled — **approve or deny a pending tool call on your behalf**. The token keeps out other user accounts and anything off this machine; it does not keep out code already running as you. Don't enable approval gating on a machine where you don't trust what runs as your user.

- **The server must never crash on bad input.** It is the one process in the loop whose death degrades everything else, so malformed bodies, wrong content types, oversized payloads, and unknown event shapes are all rejected rather than fatal. This is covered by tests, not just by intent.

- **Prompt injection, and agent output reaching your ears and screen.** What the agent is doing becomes spoken text and an overlay card. Content from the repository or the model can therefore influence what is said — so the app speaks **templated** text derived from an event type and a small set of bounded labels, never a raw payload echoed verbatim. The fleet-event bridge is the strictest case: its event struct has no field capable of holding a payload at all, and only an issue id is lifted out of an event's references, because file paths and symbol names are themselves repository content. Summaries of agent activity are bounded in length and stripped of structure before they are spoken.

- **The overlay never takes keyboard focus** (`focusable: false`, and `WS_EX_NOACTIVATE` on Windows). An overlay that could steal a keystroke while you type into a terminal would be a security problem and not merely an annoying one.

- **No outbound network calls.** The app opens no sockets other than its loopback listener. There is no HTTP client in the dependency tree, no telemetry, and no auto-updater wired today. The single carve-out is the **optional weekly dependency review, which is off by default**: when you turn it on and nominate repositories, it spawns `bun outdated` / `pnpm outdated` as child processes in those repositories, and those child processes read the npm registry. The app itself still opens no sockets; the network access belongs to the tools you already run by hand.

- **Local state.** `~/.pingmybell/` holds your config and a SQLite database of session history — agent activity summaries and session titles derived from your work. Anyone with read access to your home directory can read it. It is outside any checkout, so it is never committed by accident.

Out of scope:

- Malicious code or a malicious agent already running as your user (see above: it can read the token and act as you).
- Compromised LLM providers or agent runtimes.
- The agents themselves. PingMyBell observes them; it does not sandbox them, and enabling approval gating does not make an untrusted agent safe to run.
