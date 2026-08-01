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
  let expired = $state(false);
  let copied = $state(false);
  // Seconds left on the park, per the last heartbeat. Rust caps extensions at
  // a hard ceiling, so past a point this stops growing and starts counting
  // down — better to warn than to let the banner arrive mid-sentence.
  let remaining = $state<number | null>(null);
  let box: HTMLTextAreaElement | null = $state(null);

  const RUNNING_OUT_S = 75;
  const runningOut = $derived(
    !expired && remaining !== null && remaining <= RUNNING_OUT_S,
  );

  // The agent's question is parked on a deadline. Being here with the window
  // open is proof a human is composing an answer, so we say so periodically:
  // each heartbeat pushes that deadline out, bounded by a ceiling Rust owns.
  // Without this the question times out mid-sentence and the answer is lost —
  // which is exactly what used to happen.
  const HEARTBEAT_MS = 20_000;
  // Typing already proves liveness; one call per this window is plenty.
  const TYPING_THROTTLE_MS = 5_000;
  // An OPEN window is not by itself a present human — one left up on a locked
  // screen must not hold the agent for the whole ceiling. So the interval beat
  // stops once there has been no interaction for this long. Kept at parity
  // with Rust's TYPING_EXTENSION (120 s): any longer and there would be a gap
  // where a pause-to-think loses the question after all, which is the bug.
  const IDLE_MS = 120_000;
  let lastBeat = 0;
  let lastActive = Date.now();
  let timer: ReturnType<typeof setInterval> | null = null;

  function markActive() {
    lastActive = Date.now();
  }

  async function beat() {
    if (!prompt || expired) return;
    // Capture the id: the window is only HIDDEN between questions, never
    // destroyed, so a beat issued for the previous question can land after a
    // new one has loaded. Applying its answer would mark a perfectly live
    // question expired and lock the user out of answering it.
    const id = prompt.id;
    lastBeat = Date.now();
    try {
      const left = await invoke<number | null>("keep_question_alive", {
        questionId: id,
      });
      if (prompt?.id !== id) return;
      // Rust says the park is over. The `reply-expired` event normally gets
      // here first; this is the belt to its braces.
      if (left === null || left === undefined) expired = true;
      else remaining = left;
    } catch {
      // A failed heartbeat is not evidence the question died — say nothing
      // and try again on the next tick.
    }
  }

  function onTyping() {
    markActive();
    // The clipboard holds the text as it was at copy time; editing makes the
    // "Copied" confirmation a lie.
    copied = false;
    if (Date.now() - lastBeat >= TYPING_THROTTLE_MS) void beat();
  }

  function load(p: Prompt | null) {
    if (!p) return;
    // A new question replaces whatever was half-typed: the old one is gone
    // from the agent's side anyway.
    if (p.id !== prompt?.id) text = "";
    prompt = p;
    sending = false;
    expired = false;
    copied = false;
    remaining = null;
    markActive();
    void beat();
    queueMicrotask(() => box?.focus());
  }

  onMount(() => {
    const unlisten = listen<Prompt>("reply-prompt", (e) => load(e.payload));
    // The question ran out of time (or its agent went away) while this window
    // was open. Rust deliberately leaves the window and the draft alone — we
    // only stop pretending Send will work.
    const unexpire = listen<{ id: string }>("reply-expired", (e) => {
      if (e.payload?.id && e.payload.id === prompt?.id) expired = true;
    });
    // Cold-load path: the webview may have missed the emit entirely.
    invoke<Prompt | null>("pending_reply").then(load).catch(() => {});

    // Presence, not just an open window. Pointer and focus count too: someone
    // re-reading the question with their hands off the keys is still here.
    const events = ["pointermove", "keydown", "focus"] as const;
    for (const e of events) window.addEventListener(e, markActive);

    timer = setInterval(() => {
      if (Date.now() - lastActive < IDLE_MS) void beat();
    }, HEARTBEAT_MS);

    return () => {
      unlisten.then((f) => f());
      unexpire.then((f) => f());
      for (const e of events) window.removeEventListener(e, markActive);
      if (timer) clearInterval(timer);
    };
  });

  // The one affordance the whole "keep the draft" design hangs on, so it does
  // NOT rely on navigator.clipboard: this webview is served from a custom
  // scheme, which WKWebView does not treat as a secure context, and the async
  // clipboard API is unavailable there. Select-and-execCommand always works.
  async function copyDraft() {
    if (!text) return;
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
        copied = true;
        return;
      }
    } catch {
      // Fall through to the selection-based path.
    }
    if (box) {
      box.focus();
      box.select();
      copied = document.execCommand("copy");
    }
  }

  async function send() {
    const body = text.trim();
    if (!prompt || sending || expired || !body) return;
    sending = true;
    try {
      await invoke("submit_reply", {
        id: prompt.id,
        questionIndex: prompt.question_index,
        text: body,
      });
      // Rust hides the window; drop the prompt so the heartbeat stops holding
      // a question the user has already answered.
      prompt = null;
      remaining = null;
    } catch {
      // On failure let the user retry rather than silently eating what they
      // typed.
      sending = false;
    }
  }

  async function cancel() {
    if (!prompt) return;
    const id = prompt.id;
    sending = true;
    try {
      await invoke("cancel_reply", { id });
      // Same reason as `send`: a cancelled question is back to its base park
      // and must not keep being extended by a hidden window.
      prompt = null;
      remaining = null;
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
      return;
    }
    onTyping();
  }
</script>

<div class="shell">
  <div class="head">
    <span class="tag">{prompt?.header || "answer"}</span>
    <span class="crumb">{prompt?.agent || ""}{prompt?.title ? ` · ${prompt.title}` : ""}</span>
  </div>

  <p class="question">{prompt?.question || "Waiting for a question…"}</p>

  {#if expired}
    <p class="notice">
      This question timed out — {prompt?.agent || "the agent"} is asking in the terminal
      instead. Your text is still here; copy it before closing.
    </p>
  {:else if runningOut}
    <p class="notice">
      About {remaining}s left before {prompt?.agent || "the agent"} gives up and asks
      in the terminal.
    </p>
  {/if}

  <textarea
    bind:this={box}
    bind:value={text}
    {onkeydown}
    oninput={onTyping}
    disabled={!prompt || sending}
    placeholder="Type your answer…"
    spellcheck="false"
  ></textarea>

  <div class="actions">
    <span class="hint">
      {#if expired}
        copy your answer before closing
      {:else}
        ↵ send · ⇧↵ newline · esc cancel
      {/if}
    </span>
    {#if expired}
      <button class="ghost" disabled={!text.trim()} onclick={copyDraft}>
        {copied ? "Copied" : "Copy"}
      </button>
      <button class="primary" onclick={cancel}>Close</button>
    {:else}
      <button class="ghost" disabled={sending} onclick={cancel}>Cancel</button>
      <button class="primary" disabled={sending || !text.trim()} onclick={send}>Send</button>
    {/if}
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
    /* Translucent on purpose: an NSVisualEffectView sits behind this webview
       and blurs the question card underneath, so the panel reads as a layer
       above the island rather than a slab pasted onto it. Still dark enough
       to stay legible if the blur is ever unavailable. */
    background: rgba(8, 8, 10, 0.66);
    border: 1px solid rgba(255, 255, 255, 0.14);
    border-radius: 14px;
    box-shadow:
      inset 0 1px 0 rgba(255, 255, 255, 0.09),
      0 2px 8px rgba(0, 0, 0, 0.45),
      0 24px 60px rgba(0, 0, 0, 0.7);
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

  /* Amber, not red: nothing was lost — the answer just has to go to the
     terminal now, and the draft is still sitting right below this line. */
  .notice {
    margin: 0;
    padding: 6px 8px;
    border-radius: 7px;
    border: 1px solid rgba(255, 179, 46, 0.32);
    background: rgba(255, 179, 46, 0.09);
    color: #ffd693;
    font-size: 11.5px;
    line-height: 1.35;
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
