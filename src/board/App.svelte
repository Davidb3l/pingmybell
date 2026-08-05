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
    /** Seconds this session has been waiting on the user right now (§11.4);
     * null when it is not waiting. Rust decides what counts as waiting. */
    waiting_secs?: number | null;
    waiting_week_secs?: number;
    /** Live tool label while this session is working (§12.1). Rust sends it
     * only when it is worth showing; the card prefers it over the summary. */
    activity?: string | null;
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
  let voiceOptions = $state<VoiceOption[]>([]);
  // Which agent's list is expanded. Only one at a time: the two lists are
  // long, and the point is comparing a candidate against what you have.
  let picking = $state<"claude-code" | "codex" | null>(null);
  let voiceQuery = $state("");
  let previewing = $state<string | null>(null);
  // A picker that silently shows nothing is indistinguishable from a machine
  // with no voices. Surface the reason instead of swallowing it.
  let voiceError = $state("");
  let voiceClaude = $state("");
  let voiceCodex = $state("");
  let gate = $state(false);
  // The triage chord (§12.2) and, when it could not be registered, why. A
  // hotkey that silently does nothing is indistinguishable from a broken app,
  // so the one place it can be explained says it out loud.
  // "you kept agents waiting 47m this week" — one number for the whole
  // board, computed in Rust over the events table (§11.4).
  let weekWaiting = $state(0);
  // The morning digest (§12.5). Rust decides whether there is one to show and
  // writes the sentence; this draws it and offers the two things you can do
  // about it — dismiss it for today, or turn it off for good.
  type DigestCard = { lead: string; body: string };
  let digest = $state<DigestCard | null>(null);
  let digestEnabled = $state(true);
  let hotkey = $state<string | null>(null);
  let hotkeyError = $state<string | null>(null);
  // Callout shape (AC-4.3) plus per-agent rate and volume (AC-4.2). Rust owns
  // clamping and the wording; these are the current values, echoed back.
  type Style = "terse" | "conversational" | "status_only";
  let speechStyle = $state<Style>("terse");
  let rate = $state<Record<string, number>>({ "claude-code": 1, codex: 1 });
  let volume = $state<Record<string, number>>({ "claude-code": 1, codex: 1 });

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

  function loadDigest() {
    invoke<DigestCard | null>("digest_card")
      .then((card) => (digest = card))
      .catch(() => {});
  }

  function dismissDigest() {
    digest = null;
    invoke("dismiss_digest").catch(() => {});
  }

  function setDigestEnabled(enabled: boolean) {
    digestEnabled = enabled;
    if (!enabled) digest = null;
    invoke("set_digest_enabled", { enabled })
      .then(() => {
        if (enabled) loadDigest();
      })
      .catch(() => {});
  }

  function refresh() {
    invoke<{ rows: Session[]; waiting_week_secs: number }>("board_snapshot")
      .then(({ rows, waiting_week_secs }) => {
        weekWaiting = waiting_week_secs;
        sessions = Object.fromEntries(rows.map((r) => [r.id, r]));
        if (openId && !sessions[openId]) openId = null;
        // Never leave a confirm panel pointing at a row that is gone.
        if (confirmId && !sessions[confirmId]) closeConfirm();
      })
      .catch(() => {});
  }

  onMount(() => {
    refresh();
    loadDigest();
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
    // The activity ticker (§12.1) rides its own event and merges in place:
    // it beats twice a second per working session and writes no history, so
    // answering it with a full snapshot plus a 50-row drawer query would be
    // the most expensive no-op in the app. Rust decides what the label is —
    // including `null`, which is how a session that just finished clears it.
    const unlistenActivity = listen<{ id: string; activity: string | null }>(
      "session-activity",
      (e) => {
        if (document.hidden) return;
        const session = sessions[e.payload.id];
        if (!session) return;
        session.activity = e.payload.activity;
      },
    );
    // A once-a-minute tick only to refresh the "Xm ago" labels while the
    // window is open (the board is on demand; this is not app-idle load).
    const tick = setInterval(() => {
      if (document.hidden) return;
      now = Math.floor(Date.now() / 1000);
      // A waiting row's number comes from Rust (§11.4) and nothing else will
      // move it: a session that is waiting is by definition sending no
      // events. Re-pull so "waiting 4m" is not frozen at whatever it said
      // when the wait began.
      if (list.some((s) => s.waiting_secs != null)) refresh();
    }, 60_000);
    const onVisibility = () => {
      if (document.hidden) return;
      now = Math.floor(Date.now() / 1000);
      refresh();
      loadDigest();
      if (openId) loadHistory(openId);
    };
    // Spoken and drawn at the same moment: the card must not wait for the
    // next time the window is opened.
    const unlistenDigest = listen("digest-ready", () => loadDigest());
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      unlisten.then((f) => f());
      unlistenActivity.then((f) => f());
      unlistenDigest.then((f) => f());
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
    if (!showSettings) flushSpeech();
    if (showSettings) {
      invoke<{
        gate_tool_calls: boolean;
        voice_claude: string | null;
        voice_codex: string | null;
        hotkey_next: string | null;
        hotkey_error: string | null;
        speech_style: Style;
        digest_enabled: boolean;
        speech_examples: { key: Style; example: string }[];
        rate_claude: number;
        rate_codex: number;
        volume_claude: number;
        volume_codex: number;
      }>("get_settings")
        .then((s) => {
          gate = s.gate_tool_calls;
          voiceClaude = s.voice_claude ?? "";
          voiceCodex = s.voice_codex ?? "";
          hotkey = s.hotkey_next;
          hotkeyError = s.hotkey_error;
          speechStyle = s.speech_style;
          digestEnabled = s.digest_enabled;
          styleExamples = s.speech_examples;
          rate = { "claude-code": s.rate_claude, codex: s.rate_codex };
          volume = { "claude-code": s.volume_claude, codex: s.volume_codex };
        })
        .catch(() => {});
      if (voiceOptions.length === 0) {
        invoke<VoiceOption[]>("list_voice_options")
          .then((v) => {
            voiceOptions = v;
            voiceError = v.length === 0 ? "the speech engine reported no voices" : "";
          })
          .catch((err) => (voiceError = String(err)));
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

  // Only the human-readable NAME lives here; the example sentence is
  // rendered by Rust with the same function that speaks it, so the panel can
  // never claim a wording the app does not use.
  const STYLE_LABELS: Record<Style, string> = {
    terse: "Terse",
    conversational: "Conversational",
    status_only: "Status only",
  };
  let styleExamples = $state<{ key: Style; example: string }[]>([]);

  function pickStyle(style: Style) {
    speechStyle = style;
    // Rust speaks a sample in the new shape; hearing it is the whole point.
    invoke("set_speech_style", { style }).catch(() => {});
  }

  // Dragging a slider fires a sample per settled value, not per pixel: the
  // speaker would otherwise queue dozens of utterances for one gesture. The
  // timers are keyed PER SLIDER — one shared timer meant that touching a
  // second slider within the window cancelled the first one's write, so a
  // setting the panel still displayed had never reached disk.
  const speechTimers = new Map<string, ReturnType<typeof setTimeout>>();
  function setSpeech(kind: "rate" | "volume", agent: string, value: number) {
    if (kind === "rate") rate[agent] = value;
    else volume[agent] = value;
    const key = `${kind}:${agent}`;
    const pending = speechTimers.get(key);
    if (pending) clearTimeout(pending);
    speechTimers.set(
      key,
      setTimeout(() => {
        speechTimers.delete(key);
        const command = kind === "rate" ? "set_speech_rate" : "set_speech_volume";
        invoke(command, { agent, [kind]: value }).catch(() => {});
      }, 220),
    );
  }

  // Nothing in flight when the panel closes: reopening it re-reads the
  // config, and a pending write would land after that read and leave the
  // panel showing the value it just overwrote.
  function flushSpeech() {
    for (const [key, timer] of speechTimers) {
      clearTimeout(timer);
      speechTimers.delete(key);
      const [kind, agent] = key.split(":") as ["rate" | "volume", string];
      const value = kind === "rate" ? rate[agent] : volume[agent];
      const command = kind === "rate" ? "set_speech_rate" : "set_speech_volume";
      invoke(command, { agent, [kind]: value }).catch(() => {});
    }
  }

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

  // Spoken-language durations for the waiting numbers: "40s", "4m", "1h 12m".
  function duration(secs: number): string {
    if (secs < 60) return `${Math.max(0, Math.round(secs))}s`;
    const minutes = Math.floor(secs / 60);
    if (minutes < 60) return `${minutes}m`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24) {
      const rest = minutes % 60;
      return rest === 0 ? `${hours}h` : `${hours}h ${rest}m`;
    }
    // A week's worth of waiting reads as "7d 3h", not "171h".
    const rest = hours % 24;
    return rest === 0 ? `${Math.floor(hours / 24)}d` : `${Math.floor(hours / 24)}d ${rest}h`;
  }

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
      {#if weekWaiting >= 60}
        <span class="hc week" title="Total time agents spent waiting on you in the last 7 days"
          >you kept agents waiting {duration(weekWaiting)} this week</span
        >
      {/if}
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
                <div class="voice-empty">
                  {voiceError || (voiceQuery ? "no voices match" : "no voices")}
                </div>
              {/each}
            </div>
            <p class="voice-hint">
              ▶ hears the real announcement. Higher tiers need a one-time download in System
              Settings → Accessibility → Spoken Content → ⓘ beside System voice.
            </p>
          </div>
        {/if}
        <div class="setting slider-row">
          <span class="setting-label">{agent.label} speed</span>
          <input
            class="slider"
            type="range"
            min="0.5"
            max="2"
            step="0.1"
            value={rate[agent.key]}
            oninput={(e) => setSpeech("rate", agent.key, Number(e.currentTarget.value))}
          />
          <span class="slider-value">{rate[agent.key].toFixed(1)}×</span>
        </div>
        <div class="setting slider-row">
          <span class="setting-label">{agent.label} volume</span>
          <input
            class="slider"
            type="range"
            min="0"
            max="1"
            step="0.05"
            value={volume[agent.key]}
            oninput={(e) => setSpeech("volume", agent.key, Number(e.currentTarget.value))}
          />
          <span class="slider-value">{Math.round(volume[agent.key] * 100)}%</span>
        </div>
      {/each}
      <div class="setting style-row">
        <span class="setting-label">Callout style</span>
        <div class="styles">
          {#each styleExamples as s (s.key)}
            <button
              class="style-pick"
              class:chosen={speechStyle === s.key}
              title={s.example}
              onclick={() => pickStyle(s.key)}>{STYLE_LABELS[s.key]}</button
            >
          {/each}
        </div>
      </div>
      <p class="setting-note quiet">
        {styleExamples.find((s) => s.key === speechStyle)?.example ?? ""}
      </p>
      <label class="setting checkbox">
        <input
          type="checkbox"
          checked={digestEnabled}
          onchange={(e) => setDigestEnabled(e.currentTarget.checked)}
        />
        <span class="setting-label">Speak a morning summary of yesterday</span>
      </label>
      <label class="setting checkbox">
        <input
          type="checkbox"
          checked={gate}
          onchange={(e) => setGate(e.currentTarget.checked)}
        />
        <span class="setting-label">Approve tool calls from the overlay</span>
      </label>
      <div class="setting">
        <span class="setting-label">Jump to who's waiting</span>
        {#if hotkeyError}
          <span class="chord broken" title={hotkeyError}>{hotkey ?? "—"} unavailable</span>
        {:else if hotkey}
          <span class="chord">{hotkey}</span>
        {/if}
      </div>
      {#if hotkeyError}
        <p class="setting-note warn">
          {hotkeyError}. Set another chord as <code>hotkey.next</code> in
          <code>~/.pingmybell/config.json</code> and restart PingMyBell.
        </p>
      {/if}
      <p class="setting-note">
        Mute and launch-at-login live in the menu bar.
      </p>
    </section>
  {/if}

  {#if digest}
    <section class="digest">
      <span class="digest-lead">{digest.lead}</span>
      <p class="digest-body">{digest.body}</p>
      <div class="digest-actions">
        <button class="digest-ok" onclick={dismissDigest}>Got it</button>
        <button
          class="digest-off"
          onclick={() => setDigestEnabled(false)}
          title="No more daily summaries — turn it back on in settings">Turn off</button
        >
      </div>
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
              {#if s.activity}
                <span class="ticker"
                  ><i class="tick-live"></i><span class="tick-text">{s.activity}</span></span
                >
              {:else if s.last_summary}
                <span class="summary">{s.last_summary}</span>
              {/if}
            </div>
            <span class="state {s.state}">{STATE_LABEL[s.state] ?? s.state}</span>
            {#if s.waiting_secs != null}
              <!-- While a session is waiting, how long it has been waiting IS
                   its age — and it is the number that should feel urgent. -->
              <span
                class="age waiting"
                title="Waiting on you{s.waiting_week_secs
                  ? ` · ${duration(s.waiting_week_secs)} this week`
                  : ''}">{duration(s.waiting_secs)}</span
              >
            {:else}
              <span class="age">{age(s.last_event_at)}</span>
            {/if}
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

  /* The one card that is about the user's day rather than an agent's. Amber
     hairline rather than a filled panel: it is a note, not an alert. */
  .digest {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 14px;
    margin-bottom: 10px;
    border: 1px solid rgba(255, 176, 32, 0.22);
    border-radius: 10px;
    background: rgba(255, 176, 32, 0.04);
  }
  .digest-lead {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--amber);
    flex: none;
  }
  .digest-body {
    flex: 1;
    margin: 0;
    font-size: 12px;
    color: var(--text);
    line-height: 1.45;
  }
  .digest-actions {
    display: flex;
    gap: 6px;
    flex: none;
  }
  .digest-ok,
  .digest-off {
    font: inherit;
    font-size: 10px;
    background: none;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 6px;
    padding: 3px 9px;
    color: var(--dim);
    cursor: pointer;
    transition:
      color 120ms ease,
      border-color 120ms ease;
  }
  .digest-ok:hover {
    color: #000;
    background: var(--amber);
    border-color: var(--amber);
  }
  .digest-off:hover {
    color: var(--text);
  }
  .age.waiting {
    color: var(--amber);
  }
  .hc.week {
    color: var(--dim);
    opacity: 0.75;
  }
  .slider-row {
    gap: 10px;
  }
  .slider {
    flex: 1;
    accent-color: var(--amber);
    height: 2px;
    cursor: pointer;
  }
  .slider-value {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--dim);
    min-width: 34px;
    text-align: right;
  }
  .styles {
    display: flex;
    gap: 4px;
  }
  .style-pick {
    font: inherit;
    font-size: 10px;
    color: var(--dim);
    background: none;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 6px;
    padding: 3px 8px;
    cursor: pointer;
    transition:
      color 120ms ease,
      border-color 120ms ease;
  }
  .style-pick:hover {
    color: var(--text);
  }
  .style-pick.chosen {
    color: #000;
    background: var(--amber);
    border-color: var(--amber);
  }
  /* What that style will actually say, in its own voice: a label alone does
     not tell you whether you want it. */
  .setting-note.quiet {
    font-style: italic;
    opacity: 0.75;
  }
  /* The chord, rendered the way a keyboard shortcut should read: mono, quiet,
     and unmistakably not prose. */
  .chord {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--dim);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 5px;
    padding: 2px 6px;
  }
  .chord.broken {
    color: var(--amber);
    border-color: rgba(255, 176, 32, 0.35);
  }
  .setting-note.warn {
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
  /* What the agent is doing right now, in place of the last summary — mono,
     because it names commands and files, with a breathing dot so a row that
     is moving reads as alive at a glance. */
  .ticker {
    display: flex;
    align-items: center;
    gap: 6px;
    /* Same size as `.summary`, which this replaces: a different one makes the
       card resize by a pixel every time a turn starts and finishes. */
    font-size: 11px;
    color: #8e8e93;
    min-width: 0;
  }
  /* The truncation lives on the TEXT, not on the flex row: `text-overflow`
     applies to block containers only, so an ellipsis declared on the flex
     parent silently does nothing and a long label is cut mid-glyph. */
  .tick-text {
    font-family: var(--font-mono);
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tick-live {
    flex: none;
    width: 4px;
    height: 4px;
    border-radius: 50%;
    background: var(--blue);
    box-shadow: 0 0 5px var(--blue);
    animation: tick-breathe 1.6s ease-in-out infinite;
  }
  @keyframes tick-breathe {
    0%,
    100% {
      opacity: 0.25;
    }
    50% {
      opacity: 1;
    }
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
