<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { invoke } from "@tauri-apps/api/core";

  // Dumb renderer: Rust owns the island state machine and window sizing;
  // this component draws the current OverlayView and reports pointer
  // presence + button presses back.
  type Counts = { working: number; attention: number; done: number };
  type Toast = { agent: string; title: string; state: string; summary: string };
  type Attention = { session_id: string; agent: string; title: string; summary: string };
  type Approval = {
    id: string;
    session_id: string;
    agent: string;
    title: string;
    tool_name: string;
    tool_summary: string;
    queued: number;
  };
  type Row = { agent: string; title: string; state: string; minutes: number };
  type View = {
    mode: "idle" | "toast" | "attention" | "approval" | "expanded";
    has_notch: boolean;
    counts: Counts;
    toast: Toast | null;
    attention: Attention | null;
    approval: Approval | null;
    sessions: Row[] | null;
  };

  let view = $state<View>({
    mode: "idle",
    has_notch: false,
    counts: { working: 0, attention: 0, done: 0 },
    toast: null,
    attention: null,
    approval: null,
    sessions: null,
  });
  let deciding = $state(false);

  const total = $derived(view.counts.working + view.counts.attention + view.counts.done);

  onMount(() => {
    const unlisten = listen<View>("overlay-state", (e) => {
      view = e.payload;
      deciding = false;
    });
    return () => {
      unlisten.then((f) => f());
    };
  });

  function dots(n: number): number[] {
    return Array.from({ length: Math.min(n, 4) }, (_, i) => i);
  }

  function hover(hovering: boolean) {
    invoke("overlay_hover", { hovering }).catch(() => {});
  }

  async function decide(decision: "allow" | "deny" | "ask") {
    if (!view.approval || deciding) return;
    deciding = true;
    try {
      await invoke("decide", { approvalId: view.approval.id, decision });
    } catch (err) {
      console.error("decide failed", err);
      deciding = false;
    }
  }

  function dismiss() {
    if (!view.attention) return;
    invoke("dismiss_attention", { sessionId: view.attention.session_id }).catch(() => {});
  }

  const STATE_LABEL: Record<string, string> = {
    working: "working",
    needs_attention: "needs you",
    done: "done",
    unknown: "…",
    ended: "ended",
  };

  function age(minutes: number): string {
    if (minutes < 1) return "now";
    if (minutes < 60) return `${minutes}m`;
    return `${Math.floor(minutes / 60)}h`;
  }
</script>

<div
  class="shell"
  class:notch={view.has_notch}
  role="status"
  onmouseenter={() => hover(true)}
  onmouseleave={() => hover(false)}
>
  {#key view.mode}
    {#if view.mode === "idle"}
      <div class="idle">
        {#if total > 0}
          <div class="lights">
            {#each dots(view.counts.attention) as i (`a${i}`)}<span
                class="light amber pulse"
              ></span>{/each}
            {#each dots(view.counts.working) as i (`w${i}`)}<span class="light blue"></span>{/each}
            {#each dots(view.counts.done) as i (`d${i}`)}<span class="light green"></span>{/each}
          </div>
        {:else}
          <span class="light off"></span>
        {/if}
      </div>
    {:else if view.mode === "expanded"}
      <div class="panel expanded">
        {#if view.sessions && view.sessions.length > 0}
          {#each view.sessions as s, i (s.title + i)}
            <div class="row" style="animation-delay: {60 + i * 45}ms">
              <span
                class="light {s.state === 'needs_attention'
                  ? 'amber pulse'
                  : s.state === 'done'
                    ? 'green'
                    : 'blue'}"
              ></span>
              <span class="tag">{s.agent}</span>
              <span class="row-title">{s.title}</span>
              <span class="row-state {s.state}">{STATE_LABEL[s.state] ?? s.state}</span>
              <span class="row-age">{age(s.minutes)}</span>
            </div>
          {/each}
        {:else}
          <div class="row empty-row" style="animation-delay: 60ms">
            <span class="light off"></span>
            <span class="row-title dim">no live sessions</span>
          </div>
        {/if}
      </div>
    {:else if view.mode === "toast" && view.toast}
      <div class="panel toast">
        <span class="light {view.toast.state === 'done' ? 'green' : 'blue'} beacon"></span>
        <div class="stack">
          <div class="micro">
            <span class="tag">{view.toast.agent}</span>
            <span class="crumb">{view.toast.title}</span>
          </div>
          <div class="line">{view.toast.summary || "finished"}</div>
        </div>
      </div>
    {:else if view.mode === "attention" && view.attention}
      <div class="panel attention">
        <span class="light amber pulse beacon"></span>
        <div class="stack">
          <div class="micro">
            <span class="tag amber-text">{view.attention.agent} needs you</span>
            <span class="crumb">{view.attention.title}</span>
          </div>
          <div class="line">{view.attention.summary || "waiting for your input"}</div>
        </div>
        <button class="ghost dismiss" onclick={dismiss} aria-label="Dismiss">✕</button>
      </div>
    {:else if view.mode === "approval" && view.approval}
      <div class="panel approval">
        <div class="micro">
          <span class="tag amber-text">{view.approval.tool_name}</span>
          <span class="crumb">{view.approval.agent} · {view.approval.title}</span>
          {#if view.approval.queued > 0}
            <span class="queued">+{view.approval.queued}</span>
          {/if}
        </div>
        <code class="well">{view.approval.tool_summary}</code>
        <div class="actions">
          <button class="primary" disabled={deciding} onclick={() => decide("allow")}>
            Approve
          </button>
          <button class="ghost danger" disabled={deciding} onclick={() => decide("deny")}>
            Deny
          </button>
          <button class="ghost" disabled={deciding} onclick={() => decide("ask")}>
            Terminal
          </button>
        </div>
      </div>
    {/if}
  {/key}
</div>

<style>
  .shell {
    --amber: #ffb32e;
    --green: #30d158;
    --blue: #0a84ff;
    --red: #ff453a;
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
    background: #000;
    border-radius: 0 0 18px 18px;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    overflow: hidden;
    font-family: var(--font-ui);
    /* bezel highlight: a whisper of an edge so the island reads as a
       surface, not a hole */
    box-shadow:
      inset 0 -1px 0 var(--hairline),
      inset 1px 0 0 rgba(255, 255, 255, 0.04),
      inset -1px 0 0 rgba(255, 255, 255, 0.04);
  }
  .shell:not(.notch) {
    border-radius: 17px;
    align-items: center;
    box-shadow:
      inset 0 0 0 1px var(--hairline),
      0 6px 24px rgba(0, 0, 0, 0.55);
  }

  /* ---------- status lights ---------- */
  .light {
    flex: none;
    width: 6px;
    height: 6px;
    border-radius: 50%;
  }
  .light.blue {
    background: var(--blue);
    box-shadow: 0 0 6px 0 rgba(10, 132, 255, 0.8);
  }
  .light.green {
    background: var(--green);
    box-shadow: 0 0 6px 0 rgba(48, 209, 88, 0.7);
  }
  .light.amber {
    background: var(--amber);
    box-shadow: 0 0 8px 1px rgba(255, 179, 46, 0.75);
  }
  .light.off {
    background: #2c2c2e;
  }
  .light.beacon {
    width: 8px;
    height: 8px;
  }
  .pulse {
    animation: pulse 1.4s ease-in-out infinite;
  }
  @keyframes pulse {
    50% {
      opacity: 0.3;
      box-shadow: none;
    }
  }

  /* ---------- idle sliver ---------- */
  .idle {
    display: flex;
    align-items: center;
    padding: 0 12px 5px;
    animation: rise 260ms cubic-bezier(0.32, 1.4, 0.4, 1);
  }
  .shell:not(.notch) .idle {
    padding: 0 12px;
  }
  .lights {
    display: flex;
    gap: 6px;
    align-items: center;
  }

  /* ---------- shared panel ---------- */
  .panel {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 0 16px 9px;
    animation: rise 280ms cubic-bezier(0.32, 1.55, 0.4, 1);
    transform-origin: top center;
  }
  .shell:not(.notch) .panel {
    padding: 8px 16px;
  }
  @keyframes rise {
    from {
      opacity: 0;
      transform: translateY(-5px) scale(0.975);
    }
  }

  .stack {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1;
  }
  .micro {
    display: flex;
    align-items: baseline;
    gap: 8px;
    min-width: 0;
  }
  .tag {
    font-family: var(--font-mono);
    font-size: 9px;
    font-weight: 700;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--dim);
    flex: none;
  }
  .amber-text {
    color: var(--amber);
    text-shadow: 0 0 12px rgba(255, 179, 46, 0.35);
  }
  .crumb {
    font-size: 11px;
    font-weight: 600;
    color: var(--text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .line {
    font-size: 12px;
    line-height: 1.25;
    color: #c7c7cc;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* ---------- expanded session list ---------- */
  .expanded {
    flex-direction: column;
    align-items: stretch;
    justify-content: flex-end;
    gap: 0;
    padding: 0 14px 8px;
  }
  .shell:not(.notch) .expanded {
    padding: 8px 14px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 9px;
    height: 34px;
    border-top: 1px solid transparent;
    animation: row-in 300ms cubic-bezier(0.3, 1.3, 0.45, 1) backwards;
  }
  .row + .row {
    border-top-color: rgba(255, 255, 255, 0.05);
  }
  @keyframes row-in {
    from {
      opacity: 0;
      transform: translateY(-6px);
    }
  }
  .row-title {
    font-size: 12px;
    font-weight: 600;
    color: var(--text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
  }
  .row-title.dim {
    color: var(--dim);
    font-weight: 500;
  }
  .row-state {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--dim);
    flex: none;
  }
  .row-state.needs_attention {
    color: var(--amber);
  }
  .row-state.done {
    color: var(--green);
  }
  .row-state.working {
    color: var(--blue);
  }
  .row-age {
    font-family: var(--font-mono);
    font-size: 9px;
    color: #48484a;
    flex: none;
    min-width: 22px;
    text-align: right;
  }
  .empty-row {
    justify-content: center;
  }

  /* ---------- approval ---------- */
  .approval {
    flex-direction: column;
    align-items: stretch;
    gap: 6px;
    padding: 0 16px 10px;
  }
  .shell:not(.notch) .approval {
    padding: 10px 16px;
  }
  .queued {
    margin-left: auto;
    font-family: var(--font-mono);
    font-size: 9px;
    color: var(--dim);
    border: 1px solid var(--hairline);
    border-radius: 99px;
    padding: 1px 6px;
  }
  .well {
    font-family: var(--font-mono);
    font-size: 11px;
    color: #d7d7dc;
    background: var(--well);
    border: 1px solid rgba(255, 255, 255, 0.05);
    border-radius: 7px;
    padding: 4px 9px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .actions {
    display: flex;
    gap: 7px;
    align-items: center;
  }
  button {
    font-family: var(--font-ui);
    font-size: 12px;
    font-weight: 650;
    border: none;
    border-radius: 8px;
    padding: 5px 15px;
    cursor: pointer;
    transition:
      transform 120ms ease,
      filter 120ms ease;
  }
  button:active {
    transform: scale(0.96);
  }
  button:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .primary {
    background: var(--amber);
    color: #1a1200;
    box-shadow: 0 0 16px rgba(255, 179, 46, 0.35);
  }
  .primary:hover:not(:disabled) {
    filter: brightness(1.08);
  }
  .ghost {
    background: rgba(255, 255, 255, 0.07);
    color: var(--text);
    border: 1px solid var(--hairline);
  }
  .ghost:hover:not(:disabled) {
    background: rgba(255, 255, 255, 0.12);
  }
  .ghost.danger {
    color: #ff6961;
  }
  .dismiss {
    flex: none;
    font-size: 11px;
    padding: 3px 9px;
    border-radius: 99px;
  }
</style>
