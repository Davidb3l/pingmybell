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
  // Type-to-confirm delete: `confirmId` is the card showing the panel, and
  // `typed` has to match that session's title exactly before Delete arms.
  let confirmId = $state<string | null>(null);
  let typed = $state("");
  let deleting = $state(false);
  let deleteError = $state("");
  let history = $state<HistoryEvent[]>([]);
  let now = $state(Math.floor(Date.now() / 1000));
  let showSettings = $state(false);
  type VoiceOption = {
    name: string;
    language: string;
    quality: "premium" | "enhanced" | "standard";
    family: "siri" | "standard" | "eloquence" | "novelty";
    english: boolean;
  };
  let voices = $state<string[]>([]);
  let voiceOptions = $state<VoiceOption[]>([]);
  // Which agent's list is expanded. Only one at a time: the two lists are
  // long, and the point is comparing a candidate against what you have.
  let picking = $state<"claude-code" | "codex" | null>(null);
  let voiceQuery = $state("");
  let previewing = $state<string | null>(null);
  let voiceClaude = $state("");
  let voiceCodex = $state("");
  let gate = $state(false);

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
        // Never leave a confirm panel pointing at a row that is gone.
        if (confirmId && !sessions[confirmId]) closeConfirm();
      })
      .catch(() => {});
  }

  onMount(() => {
    refresh();
    // Re-pulling the whole snapshot keeps last summaries fresh without
    // duplicating merge logic here — but closing the board HIDES it, so this
    // webview outlives every close and would otherwise answer every hook
    // event with a snapshot plus one query per session for the life of the
    // process. Nothing on screen, nothing worth computing; the
    // visibilitychange handler below re-pulls on the way back in, which is
    // the only moment staleness can be seen.
    const unlisten = listen<Session>("session-updated", (e) => {
      if (document.hidden) return;
      refresh();
      if (openId === e.payload.id) loadHistory(e.payload.id);
    });
    // A once-a-minute tick only to refresh the "Xm ago" labels while the
    // window is open (the board is on demand; this is not app-idle load).
    const tick = setInterval(() => {
      if (document.hidden) return;
      now = Math.floor(Date.now() / 1000);
    }, 60_000);
    const onVisibility = () => {
      if (document.hidden) return;
      now = Math.floor(Date.now() / 1000);
      refresh();
      if (openId) loadHistory(openId);
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      unlisten.then((f) => f());
      clearInterval(tick);
      document.removeEventListener("visibilitychange", onVisibility);
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

  function askDelete(e: MouseEvent, id: string) {
    e.stopPropagation();
    if (confirmId === id) {
      closeConfirm();
      return;
    }
    confirmId = id;
    typed = "";
    deleteError = "";
  }

  function closeConfirm() {
    confirmId = null;
    typed = "";
    deleteError = "";
  }

  // Exact match, the way GitHub does it: the point of the gate is that it
  // cannot be cleared without reading the name you are about to destroy. An
  // empty (or whitespace-only) title fails closed for the same reason: there
  // is nothing to read, so Delete would arm on an empty box the instant the
  // panel opens — on the one irreversible action in the app.
  const armed = $derived.by(() => {
    if (confirmId === null) return false;
    const title = sessions[confirmId]?.title ?? "";
    return title.trim() !== "" && typed.trim() === title;
  });

  function confirmDelete(id: string) {
    if (!armed || deleting) return;
    deleting = true;
    deleteError = "";
    invoke<boolean>("delete_session", { sessionId: id })
      .then(() => {
        if (openId === id) {
          openId = null;
          history = [];
        }
        closeConfirm();
        refresh();
      })
      .catch((err) => {
        deleteError = String(err);
      })
      .finally(() => {
        deleting = false;
      });
  }

  function autofocus(node: HTMLInputElement) {
    node.focus();
  }

  function toggleSettings() {
    showSettings = !showSettings;
    if (showSettings) {
      invoke<{ gate_tool_calls: boolean; voice_claude: string | null; voice_codex: string | null }>(
        "get_settings",
      )
        .then((s) => {
          gate = s.gate_tool_calls;
          voiceClaude = s.voice_claude ?? "";
          voiceCodex = s.voice_codex ?? "";
        })
        .catch(() => {});
      if (voiceOptions.length === 0) {
        invoke<VoiceOption[]>("list_voice_options")
          .then((v) => (voiceOptions = v))
          .catch(() => {});
      }
    } else {
      picking = null;
    }
  }

  function togglePicker(agent: "claude-code" | "codex") {
    picking = picking === agent ? null : agent;
    voiceQuery = "";
  }

  /// Audition without committing — the whole reason this exists is that a
  /// name tells you nothing about how a voice sounds.
  function preview(agent: "claude-code" | "codex", voice: string) {
    previewing = voice;
    invoke("preview_voice", { agent, voice })
      .catch(() => {})
      .finally(() => setTimeout(() => (previewing = null), 1600));
  }

  function pickVoice(agent: "claude-code" | "codex", voice: string) {
    if (!voice) return;
    if (agent === "codex") voiceCodex = voice;
    else voiceClaude = voice;
    invoke("set_voice", { agent, voice }).catch(() => {});
  }

  function currentVoice(agent: "claude-code" | "codex") {
    return agent === "codex" ? voiceCodex : voiceClaude;
  }

  // Novelty voices (Bells, Zarvox, Boing) are useless for an announcement
  // and would otherwise pad the list; they stay available via search.
  const shownVoices = $derived.by(() => {
    const q = voiceQuery.trim().toLowerCase();
    return voiceOptions.filter((v) => {
      if (q) return v.name.toLowerCase().includes(q) || v.language.toLowerCase().includes(q);
      return v.family !== "novelty" && v.family !== "eloquence" && v.english;
    });
  });

  function setGate(enabled: boolean) {
    gate = enabled;
    invoke("set_gate", { enabled }).catch(() => {});
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
    <button class="gear" class:active={showSettings} onclick={toggleSettings} title="Settings">
      ⚙
    </button>
  </header>

  {#if showSettings}
    <section class="settings">
      {#each [{ key: "claude-code" as const, label: "Claude" }, { key: "codex" as const, label: "Codex" }] as agent (agent.key)}
        <div class="setting voice-row">
          <span class="setting-label">{agent.label} voice</span>
          <button class="voice-current" onclick={() => togglePicker(agent.key)}>
            <span class="vc-name">{currentVoice(agent.key) || "system default"}</span>
            {#if currentVoice(agent.key)}
              {@const q = voiceOptions.find((v) => v.name === currentVoice(agent.key))?.quality}
              {#if q && q !== "standard"}<span class="q-badge {q}">{q}</span>{/if}
            {/if}
            <span class="chev" class:open={picking === agent.key}>›</span>
          </button>
        </div>
        {#if picking === agent.key}
          <div class="voice-picker">
            <input
              class="voice-search"
              type="text"
              placeholder="Search {voiceOptions.length} voices…"
              bind:value={voiceQuery}
            />
            <div class="voice-list">
              {#each shownVoices as v (v.name + v.language)}
                <div class="voice-item" class:chosen={currentVoice(agent.key) === v.name}>
                  <button
                    class="vi-play"
                    class:playing={previewing === v.name}
                    title="Hear {v.name}"
                    aria-label="Hear {v.name}"
                    onclick={() => preview(agent.key, v.name)}>▶</button
                  >
                  <button class="vi-pick" onclick={() => pickVoice(agent.key, v.name)}>
                    <span class="vi-name">{v.name}</span>
                    {#if v.quality !== "standard"}
                      <span class="q-badge {v.quality}">{v.quality}</span>
                    {/if}
                    {#if v.family === "siri"}<span class="q-badge siri">siri</span>{/if}
                    <span class="vi-lang">{v.language}</span>
                  </button>
                </div>
              {:else}
                <div class="voice-empty">no voices match</div>
              {/each}
            </div>
            <p class="voice-hint">
              ▶ hears the real announcement. Higher tiers need a one-time download in System
              Settings → Accessibility → Spoken Content → ⓘ beside System voice.
            </p>
          </div>
        {/if}
      {/each}
      <label class="setting checkbox">
        <input
          type="checkbox"
          checked={gate}
          onchange={(e) => setGate(e.currentTarget.checked)}
        />
        <span class="setting-label">Approve tool calls from the overlay</span>
      </label>
      <p class="setting-note">
        Mute and launch-at-login live in the menu bar.
      </p>
    </section>
  {/if}

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
              // The ↗ and ✕ buttons live INSIDE this row, and their click
              // handlers can only stop a click: an Enter on ↗ would jump and
              // then toggle the drawer on the way up.
              if (e.target !== e.currentTarget) return;
              if (e.key !== "Enter" && e.key !== " ") return;
              e.preventDefault(); // Space on a div scrolls the board
              toggle(s.id);
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
            <button
              class="kill"
              class:armed={confirmId === s.id}
              title="Delete this session"
              aria-label="Delete {s.title}"
              onclick={(e) => askDelete(e, s.id)}
            >
              ✕
            </button>
          </div>
          {#if confirmId === s.id}
            <div class="confirm">
              <p class="confirm-lead">
                Deletes <strong>{s.title}</strong> and everything recorded against it. There is no
                undo.
              </p>
              {#if s.state !== "done" && s.state !== "ended"}
                <p class="confirm-note">
                  This session is still live — its next event will put it straight back.
                </p>
              {/if}
              <label class="confirm-label" for="confirm-{s.id}">
                Type <span class="literal">{s.title}</span> to confirm
              </label>
              <div class="confirm-actions">
                <input
                  id="confirm-{s.id}"
                  class="confirm-input"
                  type="text"
                  autocomplete="off"
                  spellcheck="false"
                  placeholder={s.title}
                  bind:value={typed}
                  use:autofocus
                  onkeydown={(e) => {
                    if (e.key === "Escape") closeConfirm();
                    if (e.key === "Enter") confirmDelete(s.id);
                  }}
                />
                <button class="btn cancel" onclick={closeConfirm}>Cancel</button>
                <button class="btn danger" disabled={!armed || deleting} onclick={() => confirmDelete(s.id)}>
                  {deleting ? "Deleting…" : "Delete"}
                </button>
              </div>
              {#if deleteError}
                <p class="confirm-err">{deleteError}</p>
              {/if}
            </div>
          {/if}
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
  .gear {
    background: none;
    border: none;
    color: var(--dim);
    font-size: 14px;
    cursor: pointer;
    padding: 2px 4px;
    border-radius: 6px;
    transition: color 120ms ease;
  }
  .gear:hover,
  .gear.active {
    color: var(--amber);
  }

  .settings {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin: 14px 16px 0;
    padding: 14px;
    background: #131316;
    border: 1px solid rgba(255, 179, 46, 0.2);
    border-radius: 12px;
  }
  .setting {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .setting-label {
    font-size: 12px;
    font-weight: 600;
    min-width: 110px;
  }
  .voice-row {
    align-items: center;
  }
  /* The current pick doubles as the disclosure control: one target, and the
     value you are about to change is the thing you click. */
  .voice-current {
    flex: 1;
    max-width: 260px;
    display: flex;
    align-items: center;
    gap: 6px;
    background: #1c1c1f;
    color: var(--text);
    border: 1px solid var(--hairline);
    border-radius: 7px;
    padding: 4px 8px;
    font-family: var(--font-ui);
    font-size: 12px;
    cursor: pointer;
    text-align: left;
    transition: border-color 120ms ease;
  }
  .voice-current:hover {
    border-color: rgba(255, 179, 46, 0.4);
  }
  .vc-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chev {
    color: var(--dim);
    transition: transform 140ms ease;
  }
  .chev.open {
    transform: rotate(90deg);
  }
  .q-badge {
    flex: none;
    font-size: 9px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    padding: 1px 5px;
    border-radius: 4px;
    border: 1px solid transparent;
  }
  .q-badge.premium {
    color: #ffd479;
    border-color: rgba(255, 212, 121, 0.4);
  }
  .q-badge.enhanced {
    color: var(--green);
    border-color: rgba(48, 209, 88, 0.35);
  }
  .q-badge.siri {
    color: var(--blue);
    border-color: rgba(10, 132, 255, 0.35);
  }
  .voice-picker {
    margin: -2px 0 6px;
    border: 1px solid var(--hairline);
    border-radius: 9px;
    background: #101013;
    overflow: hidden;
  }
  .voice-search {
    width: 100%;
    box-sizing: border-box;
    background: transparent;
    border: none;
    border-bottom: 1px solid var(--hairline);
    color: var(--text);
    font-family: var(--font-ui);
    font-size: 12px;
    padding: 7px 10px;
  }
  .voice-search:focus {
    outline: none;
    border-bottom-color: rgba(255, 179, 46, 0.4);
  }
  .voice-list {
    max-height: 220px;
    overflow-y: auto;
    overscroll-behavior: contain;
  }
  .voice-item {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 0 6px;
  }
  .voice-item:hover {
    background: rgba(255, 255, 255, 0.04);
  }
  .voice-item.chosen {
    background: rgba(255, 179, 46, 0.09);
  }
  .vi-play {
    flex: none;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    color: var(--dim);
    font-size: 10px;
    padding: 2px 6px;
    cursor: pointer;
    transition:
      color 120ms ease,
      border-color 120ms ease;
  }
  .voice-item:hover .vi-play,
  .vi-play:focus-visible {
    color: var(--amber);
    border-color: rgba(255, 179, 46, 0.35);
  }
  .vi-play.playing {
    color: var(--green);
    border-color: rgba(48, 209, 88, 0.45);
  }
  .vi-pick {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 6px;
    background: transparent;
    border: none;
    color: var(--text);
    font-family: var(--font-ui);
    font-size: 12px;
    padding: 6px 2px;
    cursor: pointer;
    text-align: left;
  }
  .vi-name {
    flex: none;
  }
  .vi-lang {
    margin-left: auto;
    color: var(--dim);
    font-family: var(--font-mono);
    font-size: 10px;
  }
  .voice-empty,
  .voice-hint {
    color: var(--dim);
    font-size: 11px;
    padding: 8px 10px;
    margin: 0;
  }
  .voice-hint {
    border-top: 1px solid var(--hairline);
    line-height: 1.4;
  }
  .setting.checkbox {
    cursor: pointer;
  }
  .setting.checkbox .setting-label {
    min-width: 0;
  }
  .setting input[type="checkbox"] {
    accent-color: var(--amber);
  }
  .setting-note {
    margin: 0;
    font-size: 10px;
    color: var(--dim);
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

  .kill {
    flex: none;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid var(--hairline);
    border-radius: 7px;
    color: var(--dim);
    font-size: 11px;
    line-height: 1;
    padding: 4px 8px;
    cursor: pointer;
    opacity: 0;
    transition:
      color 120ms ease,
      border-color 120ms ease,
      opacity 120ms ease;
  }
  /* Destructive, so it stays out of the way until the row is under the
     pointer — but never hides once its panel is open, or from the keyboard. */
  .card:hover .kill,
  .kill:focus-visible,
  .kill.armed {
    opacity: 1;
  }
  .kill:hover,
  .kill.armed {
    color: #ff6961;
    border-color: rgba(255, 105, 97, 0.45);
  }
  .confirm {
    border-top: 1px solid rgba(255, 105, 97, 0.2);
    background: rgba(255, 105, 97, 0.04);
    padding: 10px 14px 12px 31px;
  }
  .confirm-lead {
    margin: 0 0 6px;
    font-size: 12px;
    color: var(--text);
  }
  .confirm-lead strong {
    font-weight: 600;
  }
  .confirm-note {
    margin: 0 0 6px;
    font-size: 11px;
    color: var(--amber);
  }
  .confirm-label {
    display: block;
    margin-bottom: 6px;
    font-size: 11px;
    color: var(--dim);
  }
  .literal {
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--text);
    background: rgba(255, 255, 255, 0.06);
    border-radius: 4px;
    padding: 1px 5px;
  }
  .confirm-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .confirm-input {
    flex: 1 1 auto;
    min-width: 0;
    background: #0b0b0d;
    border: 1px solid var(--hairline);
    border-radius: 7px;
    color: var(--text);
    font-family: var(--font-ui);
    font-size: 12px;
    padding: 5px 9px;
  }
  .confirm-input::placeholder {
    color: rgba(142, 142, 147, 0.5);
  }
  .confirm-input:focus {
    outline: none;
    border-color: rgba(255, 105, 97, 0.5);
  }
  .btn {
    flex: none;
    border-radius: 7px;
    font-size: 12px;
    padding: 5px 12px;
    cursor: pointer;
    border: 1px solid var(--hairline);
    background: rgba(255, 255, 255, 0.06);
    color: var(--dim);
    transition:
      color 120ms ease,
      background 120ms ease,
      border-color 120ms ease;
  }
  .cancel:hover {
    color: var(--text);
  }
  .danger {
    color: #ff6961;
    border-color: rgba(255, 105, 97, 0.4);
  }
  .danger:hover:not(:disabled) {
    background: rgba(255, 105, 97, 0.16);
  }
  .danger:disabled {
    color: rgba(142, 142, 147, 0.6);
    border-color: var(--hairline);
    background: transparent;
    cursor: not-allowed;
  }
  .confirm-err {
    margin: 8px 0 0;
    font-size: 11px;
    color: #ff6961;
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
