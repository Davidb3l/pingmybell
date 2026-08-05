// Session snapshot as emitted by the Rust registry via the `session-updated`
// Tauri event. Mirrors `registry::Session` in src-tauri.
export type Session = {
  id: string;
  // "suite" is not an agent: it is the Sothis fleet speaking through the
  // spine bridge (see src-tauri/src/spine.rs).
  agent: "claude-code" | "codex" | "suite";
  cwd: string;
  title: string;
  state: "working" | "needs_attention" | "done" | "ended" | "unknown";
  started_at: number;
  last_event_at: number;
};
