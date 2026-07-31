<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { invoke } from "@tauri-apps/api/core";
  import Bell from "../overlay/Bell.svelte";
  import AgentMark from "../overlay/AgentMark.svelte";

  // Same contract as the island: Rust owns state; this window renders
  // snapshots (board_snapshot + session-updated) and sends commands back.
  type Session = {
    id: string;
    agent: string;
    cwd: string;
    title: string;
    state: string;
    started_at: number;
    last_event_at: number;
    last_summary?: string | null;
  };
  type HistoryEvent = {
    kind: string;
    summary: string | null;
    decision: string | null;
    created_at: number;
  };

  let sessions = $state<Record<string, Session>>({});
  let openId = $state<string | null>(null);
  let history = $state<HistoryEvent[]>([]);
  let now = $state(Math.floor(Date.now() / 1000));

  const list = $derived(
    Object.values(sessions).sort((a, b) => {
      const rank = (s: Session) =>
        s.state === "needs_attention" ? 0 : s.state === "working" || s.state === "unknown" ? 1 : 2;
      return rank(a) - rank(b) || b.last_event_at - a.last_event_at;
    }),
  );
  const counts = $derived({
    attention: list.filter((s) => s.state === "needs_attention").length,
    working: list.filter((s) => s.state === "working" || s.state === "unknown").length,
    done: list.filter((s) => s.state === "done").length,
  });

  function refresh() {
    invoke<Session[]>("board_snapshot")
      .then((rows) => {
        sessions = Object.fromEntries(rows.map((r) => [r.id, r]));
        if (openId && !sessions[openId]) openId = null;
      })
      .catch(() => {});
  }

  onMount(() => {
    refresh();
    // Events are rare and the snapshot query is trivial: re-pulling keeps
    // last summaries fresh without duplicating merge logic here.
    const unlisten = listen<Session>("session-updated", (e) => {
      refresh();
      if (openId === e.payload.id) loadHistory(e.payload.id);
    });
    // A once-a-minute tick only to refresh the "Xm ago" labels while the
    // window is open (the board is on demand; this is not app-idle load).
    const tick = setInterval(() => (now = Math.floor(Date.now() / 1000)), 60_000);
    return () => {
      unlisten.then((f) => f());
      clearInterval(tick);
    };
  });

  function loadHistory(id: string) {
    invoke<HistoryEvent[]>("session_history", { sessionId: id })
      .then((events) => {
        if (openId === id) history = events;
      })
      .catch(() => {});
  }

  function toggle(id: string) {
    if (openId === id) {
      openId = null;
      history = [];
    } else {
      openId = id;
      history = [];
      loadHistory(id);
    }
  }

  function jump(e: MouseEvent, id: string) {
    e.stopPropagation();
    invoke("focus_session", { sessionId: id }).catch(() => {});
  }

  const STATE_LABEL: Record<string, string> = {
    working: "working",
    needs_attention: "needs you",
    done: "done",
    unknown: "…",
    ended: "ended",
  };
  const KIND_LABEL: Record<string, string> = {
    session_start: "start",
    turn_complete: "done",
    needs_attention: "needs you",
    permission_request: "approval",
    session_end: "end",
  };

  function age(ts: number): string {
    const s = Math.max(0, now - ts);
    if (s < 60) return "now";
    if (s < 3600) return `${Math.floor(s / 60)}m`;
    if (s < 86400) return `${Math.floor(s / 3600)}h`;
    return `${Math.floor(s / 86400)}d`;
  }

  function clock(ts: number): string {
    return new Date(ts * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
</script>

<main>
  <header>
    <span class="brand-mark"><Bell size={16} /></span>
    <span class="brand">Ping My Bell</span>
    <span class="header-counts">
      {#if counts.attention > 0}<span class="hc amber">{counts.attention} waiting</span>{/if}
      {#if counts.working > 0}<span class="hc">{counts.working} working</span>{/if}
      {#if counts.done > 0}<span class="hc green">{counts.done} done</span>{/if}
    </span>
  </header>

  {#if list.length === 0}
    <div class="empty">
      <span class="empty-mark"><Bell size={26} /></span>
      <p>No live sessions</p>
      <p class="dim">Agent activity will appear here the moment a hook fires.</p>
    </div>
  {:else}
    <div class="rows">
      {#each list as s (s.id)}
        <div class="card" class:open={openId === s.id}>
          <div
            class="row"
            role="button"
            tabindex="0"
            onclick={() => toggle(s.id)}
            onkeydown={(e) => {
              if (e.key === "Enter" || e.key === " ") toggle(s.id);
            }}
          >
            <span
              class="light {s.state === 'needs_attention'
                ? 'amber-l pulse'
                : s.state === 'done'
                  ? 'green-l'
                  : 'blue-l'}"
            ></span>
            <span class="tag">{s.agent}</span>
            <span class="mark"><AgentMark agent={s.agent} size={11} /></span>
            <div class="titles">
              <span class="title">{s.title}</span>
              {#if s.last_summary}
                <span class="summary">{s.last_summary}</span>
              {/if}
            </div>
            <span class="state {s.state}">{STATE_LABEL[s.state] ?? s.state}</span>
            <span class="age">{age(s.last_event_at)}</span>
            <button class="go" title="Jump to this session" onclick={(e) => jump(e, s.id)}>
              ↗
            </button>
          </div>
          {#if openId === s.id}
            <div class="drawer">
              {#if history.length === 0}
                <div class="hist-row dim">loading…</div>
              {:else}
                {#each history as h, i (i)}
                  <div class="hist-row" style="animation-delay: {i * 25}ms">
                    <span class="hist-time">{clock(h.created_at)}</span>
                    <span class="kind kind-{h.kind}">{KIND_LABEL[h.kind] ?? h.kind}</span>
                    {#if h.decision}
                      <span class="decision d-{h.decision}">{h.decision}</span>
                    {/if}
                    <span class="hist-summary">{h.summary ?? ""}</span>
                  </div>
                {/each}
              {/if}
            </div>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</main>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    background: #0b0b0d;
    height: 100%;
  }
  main {
    --amber: #ffb32e;
    --green: #30d158;
    --blue: #0a84ff;
    --text: #f5f5f7;
    --dim: #8e8e93;
    --hairline: rgba(255, 255, 255, 0.08);
    --font-ui: -apple-system, BlinkMacSystemFont, "Segoe UI Variable", system-ui, sans-serif;
    --font-mono: ui-monospace, "SF Mono", "Cascadia Code", monospace;

    font-family: var(--font-ui);
    color: var(--text);
    min-height: 100vh;
    box-sizing: border-box;
    padding: 0 0 24px;
  }

  header {
    position: sticky;
    top: 0;
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 14px 20px 12px;
    background: rgba(11, 11, 13, 0.92);
    backdrop-filter: blur(8px);
    border-bottom: 1px solid var(--hairline);
    z-index: 2;
  }
  .brand-mark {
    display: inline-flex;
    color: var(--amber);
    filter: drop-shadow(0 0 8px rgba(255, 179, 46, 0.4));
  }
  .brand {
    font-family: var(--font-mono);
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: #d1d1d6;
  }
  .header-counts {
    margin-left: auto;
    display: flex;
    gap: 12px;
  }
  .hc {
    font-family: var(--font-mono);
    font-size: 10px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
  }
  .hc.amber {
    color: var(--amber);
  }
  .hc.green {
    color: var(--green);
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    padding: 80px 20px;
    text-align: center;
  }
  .empty-mark {
    color: #3a3a3c;
    margin-bottom: 6px;
  }
  .empty p {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
  }
  .empty .dim {
    font-size: 12px;
    font-weight: 400;
  }
  .dim {
    color: var(--dim);
  }

  .rows {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 14px 16px 0;
  }
  .card {
    background: #131316;
    border: 1px solid rgba(255, 255, 255, 0.05);
    border-radius: 12px;
    overflow: hidden;
    transition: border-color 140ms ease;
  }
  .card.open {
    border-color: rgba(255, 179, 46, 0.25);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    box-sizing: border-box;
    padding: 11px 14px;
    cursor: pointer;
    transition: background 120ms ease;
    outline: none;
  }
  .row:focus-visible {
    background: rgba(255, 255, 255, 0.05);
  }
  .row:hover {
    background: rgba(255, 255, 255, 0.04);
  }

  .light {
    flex: none;
    width: 7px;
    height: 7px;
    border-radius: 50%;
  }
  .blue-l {
    background: var(--blue);
    box-shadow: 0 0 7px rgba(10, 132, 255, 0.8);
  }
  .green-l {
    background: var(--green);
    box-shadow: 0 0 7px rgba(48, 209, 88, 0.7);
  }
  .amber-l {
    background: var(--amber);
    box-shadow: 0 0 9px rgba(255, 179, 46, 0.75);
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

  .tag {
    font-family: var(--font-mono);
    font-size: 9px;
    font-weight: 700;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--dim);
    flex: none;
  }
  .mark {
    display: inline-flex;
    flex: none;
  }
  .titles {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
    flex: 1;
  }
  .title {
    font-size: 13px;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .summary {
    font-size: 11px;
    color: var(--dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .state {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--dim);
    flex: none;
  }
  .state.needs_attention {
    color: var(--amber);
  }
  .state.done {
    color: var(--green);
  }
  .state.working {
    color: var(--blue);
  }
  .age {
    font-family: var(--font-mono);
    font-size: 10px;
    color: #48484a;
    flex: none;
    min-width: 26px;
    text-align: right;
  }
  .go {
    flex: none;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid var(--hairline);
    border-radius: 7px;
    color: var(--dim);
    font-size: 12px;
    padding: 2px 9px;
    cursor: pointer;
    transition:
      color 120ms ease,
      background 120ms ease;
  }
  .go:hover {
    color: var(--amber);
    background: rgba(255, 179, 46, 0.1);
  }

  .drawer {
    border-top: 1px solid rgba(255, 255, 255, 0.05);
    padding: 6px 14px 10px 31px;
    max-height: 300px;
    overflow-y: auto;
  }
  .hist-row {
    display: flex;
    align-items: baseline;
    gap: 9px;
    padding: 4px 0;
    animation: hist-in 220ms ease-out backwards;
  }
  @keyframes hist-in {
    from {
      opacity: 0;
      transform: translateY(-3px);
    }
  }
  .hist-time {
    font-family: var(--font-mono);
    font-size: 9px;
    color: #48484a;
    flex: none;
    min-width: 46px;
  }
  .kind {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    flex: none;
    color: var(--dim);
  }
  .kind-turn_complete {
    color: var(--green);
  }
  .kind-needs_attention,
  .kind-permission_request {
    color: var(--amber);
  }
  .decision {
    font-family: var(--font-mono);
    font-size: 9px;
    text-transform: uppercase;
    border-radius: 99px;
    padding: 0 7px;
    flex: none;
  }
  .d-allow {
    color: var(--green);
    border: 1px solid rgba(48, 209, 88, 0.4);
  }
  .d-deny {
    color: #ff6961;
    border: 1px solid rgba(255, 105, 97, 0.4);
  }
  .d-ask {
    color: var(--dim);
    border: 1px solid var(--hairline);
  }
  .hist-summary {
    font-size: 11px;
    color: #c7c7cc;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
