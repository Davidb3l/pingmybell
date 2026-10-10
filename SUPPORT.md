# Getting help

- **Questions, "how do I...", "is this expected?"**: open an issue with the
  **Question** template. Search [existing issues](https://github.com/Davidb3l/pingmybell/issues?q=is%3Aissue)
  first; someone may have asked already.
- **Bugs**: use the **Bug report** template.
- **Security vulnerabilities**: do **not** open a public issue. Follow
  [SECURITY.md](SECURITY.md) and report privately through GitHub.
- **Docs**: the [README](README.md) (what it does, install from source, the
  agent integrations), [PRD.md](PRD.md) for scope and acceptance criteria, and
  [ARCHITECTURE.md](ARCHITECTURE.md) for the technical design — the ingest
  server, the hook shims, the overlay, the voice engine, and the build order.
- **The wider suite** (Sirius Forester, Ametrite, Hayvenhurst, Catryna
  Wikinelli): see [Sothis](https://github.com/Davidb3l/Sothis). Questions about
  how the tools work together are welcome here too.

PingMyBell is maintained by one person, and it is pre-alpha and built from
source — expect rough edges, and a reply within a few days. When you report a
problem, include your OS and version, the commit you built from, whether you
were running a dev build (`bun run tauri dev`) or a bundled app, and which
agent integration was involved.
