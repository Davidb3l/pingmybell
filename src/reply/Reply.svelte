<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { invoke } from "@tauri-apps/api/core";

  // The one focusable surface in PingMyBell. Same dumb-renderer contract as
  // the overlay: Rust hands it a prompt, it hands back text or a cancel.
  type Prompt = {
    id: string;
    header: string;
    question: string;
    question_index: number;
    agent: string;
    title: string;
  };

  let prompt = $state<Prompt | null>(null);
  let text = $state("");
  let sending = $state(false);
  let box: HTMLTextAreaElement | null = $state(null);

  function load(p: Prompt | null) {
    if (!p) return;
    // A new question replaces whatever was half-typed: the old one is gone
    // from the agent's side anyway.
    if (p.id !== prompt?.id) text = "";
    prompt = p;
    sending = false;
    queueMicrotask(() => box?.focus());
  }

  onMount(() => {
    const unlisten = listen<Prompt>("reply-prompt", (e) => load(e.payload));
    // Cold-load path: the webview may have missed the emit entirely.
    invoke<Prompt | null>("pending_reply").then(load).catch(() => {});
    return () => {
      unlisten.then((f) => f());
    };
  });

  async function send() {
    const body = text.trim();
    if (!prompt || sending || !body) return;
    sending = true;
    try {
      await invoke("submit_reply", {
        id: prompt.id,
        questionIndex: prompt.question_index,
        text: body,
      });
    } catch {
      // Rust closes the window on success; on failure let the user retry
      // rather than silently eating what they typed.
      sending = false;
    }
  }

  async function cancel() {
    if (!prompt) return;
    const id = prompt.id;
    sending = true;
    try {
      await invoke("cancel_reply", { id });
    } catch {
      sending = false;
    }
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      cancel();
      return;
    }
    // Enter sends; Shift+Enter is a newline. Answers are usually one line,
    // and reaching for the mouse defeats the point of typing here.
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }
</script>

<div class="shell">
  <div class="head">
    <span class="tag">{prompt?.header || "answer"}</span>
    <span class="crumb">{prompt?.agent || ""}{prompt?.title ? ` · ${prompt.title}` : ""}</span>
  </div>

  <p class="question">{prompt?.question || "Waiting for a question…"}</p>

  <textarea
    bind:this={box}
    bind:value={text}
    {onkeydown}
    disabled={!prompt || sending}
    placeholder="Type your answer…"
    spellcheck="false"
  ></textarea>

  <div class="actions">
    <span class="hint">↵ send · ⇧↵ newline · esc cancel</span>
    <button class="ghost" disabled={sending} onclick={cancel}>Cancel</button>
    <button class="primary" disabled={sending || !text.trim()} onclick={send}>Send</button>
  </div>
</div>

<style>
  .shell {
    --amber: #ffb32e;
    --text: #f5f5f7;
    --dim: #8e8e93;
    --hairline: rgba(255, 255, 255, 0.09);
    --well: #131315;
    --font-ui:
      -apple-system, BlinkMacSystemFont, "Segoe UI Variable", system-ui, sans-serif;
    --font-mono: ui-monospace, "SF Mono", "Cascadia Code", monospace;

    box-sizing: border-box;
    width: 100vw;
    height: 100vh;
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px 14px 12px;
    background: #000;
    border: 1px solid var(--hairline);
    border-radius: 14px;
    box-shadow:
      inset 0 1px 0 rgba(255, 255, 255, 0.06),
      0 18px 44px rgba(0, 0, 0, 0.6);
    color: var(--text);
    font-family: var(--font-ui);
    overflow: hidden;
  }

  .head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    min-width: 0;
  }
  .tag {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--amber);
    white-space: nowrap;
  }
  .crumb {
    font-size: 11px;
    color: var(--dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .question {
    margin: 0;
    font-size: 13px;
    line-height: 1.35;
    max-height: 3.6em;
    overflow: hidden;
  }

  textarea {
    flex: 1;
    box-sizing: border-box;
    resize: none;
    background: var(--well);
    border: 1px solid var(--hairline);
    border-radius: 8px;
    padding: 8px 10px;
    color: var(--text);
    font-family: var(--font-ui);
    font-size: 13px;
    line-height: 1.4;
    outline: none;
    transition: border-color 120ms ease;
  }
  textarea::placeholder {
    color: #5a5a60;
  }
  textarea:focus {
    border-color: rgba(255, 179, 46, 0.55);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .hint {
    flex: 1;
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: #5a5a60;
    white-space: nowrap;
    overflow: hidden;
  }

  button {
    font-family: var(--font-ui);
    font-size: 12px;
    font-weight: 500;
    padding: 5px 14px;
    border-radius: 7px;
    border: 1px solid var(--hairline);
    background: transparent;
    color: var(--text);
    cursor: pointer;
    transition:
      background 120ms ease,
      border-color 120ms ease,
      opacity 120ms ease;
  }
  button:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .ghost:hover:not(:disabled) {
    background: rgba(255, 255, 255, 0.06);
  }
  .primary {
    background: var(--amber);
    border-color: var(--amber);
    color: #1a1206;
    font-weight: 600;
  }
  .primary:hover:not(:disabled) {
    background: #ffc153;
  }
</style>
