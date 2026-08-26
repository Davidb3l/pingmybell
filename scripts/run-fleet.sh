#!/usr/bin/env bash
# Launch the Sirius Forester fleet on this repo's `todo` queue.
#
# Run this in GIT BASH, not PowerShell: the gate's test command is a compound
# `cargo test ... && bun run check`, which needs a POSIX shell on PATH. From
# PowerShell every worker's gate dies with "program not found" (Sirius-Forester
# SF-15).
set -u

# The suite CLIs and the Claude Code binary are installed but not on the
# system PATH. The versioned claude path moves when the desktop app updates —
# if this stops working, look for a newer folder under claude-code/.
CLAUDE_DIR="$(ls -d "$APPDATA/Claude/claude-code"/*/ 2>/dev/null | sort -V | tail -1)"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:${CLAUDE_DIR%/}:$PATH"

cd "$(dirname "$0")/.." || exit 1

for tool in sirius amt hayven claude; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing: $tool"; exit 1; }
done

echo "== preflight"
sirius doctor || { echo "doctor is unhappy — the loop's guarantees do not hold; stopping."; exit 1; }

echo
echo "== queue"
amt issue list --status todo

echo
echo "== launching 2 workers (Ctrl-C to stop; the loop exits on its own when"
echo "   a full round finds no claimable work)"
echo

# --dangerously-skip-permissions: workers are headless, so there is no human
# to approve each edit. They are scoped to this repo and everything is
# committed and pushed, so `git reset --hard` undoes anything unwelcome.
sirius run \
  --workers 2 \
  --from todo \
  --agent-cmd 'claude -p --dangerously-skip-permissions "work the claimed issue the Sirius way"'

status=$?
echo
echo "== fleet exited with $status  (0 ok, 1 failure, 2 usage, 3 gate blocked)"
amt issue list
exit $status
