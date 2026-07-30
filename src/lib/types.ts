// Session snapshot as emitted by the Rust registry via the `session-updated`
// Tauri event. Mirrors `registry::Session` in src-tauri.
export type Session = {
  id: string;
  agent: "claude-code" | "codex";
  cwd: string;
  title: string;
  state: "working" | "needs_attention" | "done" | "ended" | "unknown";
  started_at: number;
  last_event_at: number;
};
