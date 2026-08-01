<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { invoke } from "@tauri-apps/api/core";
  import Bell from "./Bell.svelte";
  import AgentMark from "./AgentMark.svelte";

  // Dumb renderer: Rust owns the island state machine and window sizing;
  // this component draws the current OverlayView and reports pointer
  // presence + button presses back.
  type Counts = { working: number; attention: number; done: number };
  type Toast = {
    session_id: string;
    agent: string;
    title: string;
    state: string;
    summary: string;
  };
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
  type QuestionOption = { label: string; description: string };
  type QuestionSpec = {
    question: string;
    header: string;
    options: QuestionOption[];
    multiSelect: boolean;
  };
  type Question = {
    id: string;
    session_id: string;
    agent: string;
    title: string;
    tool_use_id: string | null;
    questions: QuestionSpec[];
    queued: number;
  };
  type Row = { id: string; agent: string; title: string; state: string; minutes: number };
  type View = {
    mode: "idle" | "toast" | "attention" | "approval" | "question" | "expanded";
    has_notch: boolean;
    shell: [number, number];
    /** Max height of the session scroller (Rust owns sizing). */
    list_max: number;
    counts: Counts;
    toast: Toast | null;
    attention: Attention | null;
    approval: Approval | null;
    question: Question | null;
    sessions: Row[] | null;
  };

  let view = $state<View>({
    mode: "idle",
    has_notch: false,
    shell: [179, 48],
    list_max: 272,
    counts: { working: 0, attention: 0, done: 0 },
    toast: null,
    attention: null,
    approval: null,
    question: null,
    sessions: null,
  });
  let deciding = $state(false);

  const total = $derived(view.counts.working + view.counts.attention + view.counts.done);

  onMount(() => {
    const unlisten = listen<View>("overlay-state", (e) => {
      const wasCard = view.approval?.id ?? view.question?.id ?? null;
      const nowCard = e.payload.approval?.id ?? e.payload.question?.id ?? null;
      view = e.payload;
      // Only release the buttons when the pinned card actually changed. An
      // unrelated toast/hover emit used to re-enable them mid-submit, undoing
      // the back-pressure that keeps a rejected answer from being double-sent.
      if (wasCard !== nowCard) deciding = false;
    });
    // Typed answers come back from the reply window rather than going
    // straight to the broker, so the card stays the single submitter.
    const unlistenReply = listen<{
      question_id: string;
      question_index: number;
      text: string;
    }>("reply-answer", (e) => {
      if (!view.question || e.payload.question_id !== view.question.id) return;
      typed[e.payload.question_index] = e.payload.text;
      if (e.payload.question_index === qIndex) advance();
    });
    return () => {
      unlisten.then((f) => f());
      unlistenReply.then((f) => f());
    };
  });

  function dots(n: number): number[] {
    return Array.from({ length: Math.min(n, 4) }, (_, i) => i);
  }

  function hover(hovering: boolean) {
    invoke("overlay_hover", { hovering }).catch(() => {});
  }

  // Edge feathering for the session scroller: the list only fades on the side
  // that has more content, so the first row is never dimmed at rest.
  let listEl = $state<HTMLElement | null>(null);
  let moreAbove = $state(false);
  let moreBelow = $state(false);

  function measure() {
    if (!listEl) return;
    moreAbove = listEl.scrollTop > 2;
    moreBelow = listEl.scrollTop + listEl.clientHeight < listEl.scrollHeight - 2;
  }

  $effect(() => {
    view.sessions; // re-measure whenever the row set changes
    measure();
  });

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

  // ---------- question card ----------
  // One AskUserQuestion call can carry several questions. Selections are
  // gathered here and submitted as ONE answer, so the agent is unparked once.
  // This is ephemeral input state (like text in the reply box); every state
  // transition still happens in Rust.
  let qIndex = $state(0);
  let picks = $state<Record<number, string[]>>({});
  let typed = $state<Record<number, string>>({});
  let answeringId = $state<string | null>(null);

  const spec = $derived(view.question?.questions[qIndex] ?? null);
  const isLastQuestion = $derived(
    !!view.question && qIndex >= view.question.questions.length - 1,
  );
  const hasAnswerHere = $derived((picks[qIndex]?.length ?? 0) > 0 || !!typed[qIndex]);

  // A different question arriving resets the card; the old selections belong
  // to a call that is no longer parked.
  $effect(() => {
    const id = view.question?.id ?? null;
    if (id !== answeringId) {
      answeringId = id;
      qIndex = 0;
      picks = {};
      typed = {};
    }
  });

  function choose(label: string) {
    if (!spec) return;
    if (spec.multiSelect) {
      const current = picks[qIndex] ?? [];
      picks[qIndex] = current.includes(label)
        ? current.filter((l) => l !== label)
        : [...current, label];
      return; // multi-select needs an explicit Send
    }
    picks[qIndex] = [label];
    advance();
  }

  function advance() {
    if (!view.question) return;
    if (isLastQuestion) submitAnswers();
    else qIndex += 1;
  }

  async function submitAnswers() {
    if (!view.question || deciding) return;
    const answers = view.question.questions
      .map((_, i) => ({
        question_index: i,
        labels: picks[i] ?? [],
        free_text: typed[i] ?? null,
      }))
      .filter((a) => a.labels.length > 0 || a.free_text);
    if (answers.length === 0) return;
    deciding = true;
    try {
      await invoke("answer_question", { questionId: view.question.id, answers });
    } catch (err) {
      // Rejected server-side (nothing usable) — the question is still parked,
      // so let the user try again rather than leaving a dead card.
      console.error("answer failed", err);
      deciding = false;
    }
  }

  function typeAnswer() {
    if (!view.question || !spec) return;
    invoke("open_reply", {
      prompt: {
        id: view.question.id,
        header: spec.header || "answer",
        question: spec.question,
        question_index: qIndex,
        agent: view.question.agent,
        title: view.question.title,
      },
    }).catch(() => {});
  }

  function deferQuestion() {
    if (!view.question) return;
    invoke("defer_question", { questionId: view.question.id }).catch(() => {});
  }

  function jump(sessionId: string) {
    invoke("focus_session", { sessionId }).catch(() => {});
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
  class="stage"
  role="status"
  onmouseenter={() => hover(true)}
  onmouseleave={() => hover(false)}
  onclick={() => hover(true)}
>
<div
  class="shell"
  class:notch={view.has_notch}
  style="width:{view.shell[0]}px;height:{view.shell[1]}px"
>
  {#key view.mode}
    {#if view.mode === "idle"}
      <div class="idle">
        <span class="mark"><Bell size={12} /></span>
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
        <div class="header" style="animation-delay: 40ms">
          <span class="mark header-mark"><Bell size={15} /></span>
          <span class="tag">Ping My Bell</span>
          <span class="header-counts">
            {#if view.counts.attention > 0}<span class="hc amber-text"
                >{view.counts.attention} waiting</span
              >{/if}
            {#if view.counts.working > 0}<span class="hc">{view.counts.working} working</span>{/if}
            {#if view.counts.done > 0}<span class="hc green-text">{view.counts.done} done</span
              >{/if}
          </span>
        </div>
        {#if view.sessions && view.sessions.length > 0}
          <div
            class="list"
            class:more-above={moreAbove}
            class:more-below={moreBelow}
            style="max-height:{view.list_max}px"
            bind:this={listEl}
            onscroll={measure}
          >
            {#each view.sessions as s, i (s.id)}
              <button
                class="row jumpable"
                style="animation-delay: {80 + Math.min(i, 5) * 45}ms"
                title="Jump to this session"
                onclick={(e) => {
                  e.stopPropagation();
                  jump(s.id);
                }}
              >
                <span
                  class="light {s.state === 'needs_attention'
                    ? 'amber pulse'
                    : s.state === 'done'
                      ? 'green'
                      : 'blue'}"
                ></span>
                <span class="tag">{s.agent}</span>
                <span class="mark"><AgentMark agent={s.agent} /></span>
                <span class="row-title">{s.title}</span>
                <span class="row-state {s.state}">{STATE_LABEL[s.state] ?? s.state}</span>
                <span class="row-age">{age(s.minutes)}</span>
                <span class="row-go">↗</span>
              </button>
            {/each}
          </div>
        {:else}
          <div class="row empty-row" style="animation-delay: 80ms">
            <span class="light off"></span>
            <span class="row-title dim">no live sessions</span>
          </div>
        {/if}
      </div>
    {:else if view.mode === "toast" && view.toast}
      <!-- Clickable: a toast announces a session that just moved, so the
           obvious gesture is "take me there". -->
      <button
        class="panel toast jumpable"
        title="Jump to this session"
        onclick={() => jump(view.toast!.session_id)}
      >
        <span class="light {view.toast.state === 'done' ? 'green' : 'blue'} beacon"></span>
        <div class="stack">
          <div class="micro">
            <span class="tag">{view.toast.agent}</span>
            <span class="mark"><AgentMark agent={view.toast.agent} size={9} /></span>
            <span class="crumb">{view.toast.title}</span>
          </div>
          <div class="line">{view.toast.summary || "finished"}</div>
        </div>
        <span class="row-go">↗</span>
      </button>
    {:else if view.mode === "attention" && view.attention}
      <div class="panel attention">
        <span class="light amber pulse beacon"></span>
        <!-- The body jumps; the ✕ dismisses. An ask-moment we cannot answer
             inline is exactly when you want to be taken to the session. -->
        <button
          class="stack bare jumpable"
          title="Jump to this session"
          onclick={() => jump(view.attention!.session_id)}
        >
          <div class="micro">
            <span class="tag amber-text">{view.attention.agent} needs you</span>
            <span class="crumb">{view.attention.title}</span>
          </div>
          <div class="line">{view.attention.summary || "waiting for your input"}</div>
        </button>
        <button class="ghost dismiss" onclick={dismiss} aria-label="Dismiss">✕</button>
      </div>
    {:else if view.mode === "question" && view.question && spec}
      <div class="panel question">
        <div class="micro">
          <span class="tag amber-text">{spec.header || "question"}</span>
          <span class="crumb">{view.question.agent} · {view.question.title}</span>
          {#if view.question.questions.length > 1}
            <span class="queued">{qIndex + 1}/{view.question.questions.length}</span>
          {/if}
          {#if view.question.queued > 0}
            <span class="queued">+{view.question.queued}</span>
          {/if}
          <button class="ghost dismiss" onclick={deferQuestion} aria-label="Answer in terminal"
            >✕</button
          >
        </div>
        <p class="q-text">{spec.question}</p>
        <div class="options">
          <!-- Keyed by index, not label: nothing upstream dedupes option
               labels, and two long labels sharing a 200-char prefix collide
               after truncation — a duplicate key would fail the whole card
               and leave the question unanswerable until it timed out. -->
          {#each spec.options as o, oi (oi)}
            <button
              class="option"
              class:picked={(picks[qIndex] ?? []).includes(o.label)}
              disabled={deciding}
              onclick={() => choose(o.label)}
            >
              <span class="o-label">{o.label}</span>
              {#if o.description}<span class="o-desc">{o.description}</span>{/if}
            </button>
          {/each}
        </div>
        <div class="actions">
          <button class="ghost" disabled={deciding} onclick={typeAnswer}>Type answer</button>
          {#if spec.multiSelect || (hasAnswerHere && !isLastQuestion)}
            <button class="primary" disabled={deciding || !hasAnswerHere} onclick={advance}>
              {isLastQuestion ? "Send" : "Next"}
            </button>
          {/if}
        </div>
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
</div>

<style>
  .stage {
    width: 100vw;
    height: 100vh;
    display: flex;
    justify-content: center;
    align-items: flex-start;
    overflow: hidden;
  }
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
    background: #000;
    border-radius: 0 0 18px 18px;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    overflow: hidden;
    font-family: var(--font-ui);
    /* The island morph: the window snaps invisibly, the shell springs. */
    transition:
      width 260ms cubic-bezier(0.34, 1.28, 0.42, 1),
      height 260ms cubic-bezier(0.34, 1.28, 0.42, 1);
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
    gap: 7px;
    padding: 0 12px 4px;
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
  .mark {
    display: inline-flex;
    color: #6e6e73;
  }
  .header-mark {
    color: var(--amber);
    filter: drop-shadow(0 0 6px rgba(255, 179, 46, 0.4));
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
  /* Panels/stacks that are themselves buttons: strip the browser chrome but
     keep their layout, so a clickable toast looks identical to the old
     non-clickable one until you hover it. */
  button.panel,
  .stack.bare {
    appearance: none;
    -webkit-appearance: none;
    background: none;
    border: none;
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .stack.bare {
    padding: 0;
  }
  /* The ↗ only appears on hover — the toast should read as information
     first and a target second. */
  button.panel .row-go {
    opacity: 0;
    transition: opacity 140ms ease;
  }
  button.panel:hover .row-go,
  button.panel:focus-visible .row-go {
    opacity: 0.75;
  }
  button.panel:hover .line,
  .stack.bare:hover .line {
    color: var(--text);
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

  /* ---------- question card ---------- */
  .question {
    flex-direction: column;
    align-items: stretch;
    gap: 7px;
  }
  .q-text {
    margin: 0;
    font-size: 13px;
    line-height: 1.35;
    color: var(--text);
    /* Long questions truncate rather than pushing the options off a card
       whose height Rust already committed to. */
    max-height: 2.7em;
    overflow: hidden;
  }
  .options {
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  /* Each option is one press: label leads, description trails in dim text so
     the choice is scannable without reading the whole line. */
  .option {
    display: flex;
    align-items: baseline;
    gap: 8px;
    text-align: left;
    padding: 7px 10px;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    background: var(--well);
    color: var(--text);
    font-family: var(--font-ui);
    font-size: 12px;
    cursor: pointer;
    min-width: 0;
    transition:
      background 120ms ease,
      border-color 120ms ease;
  }
  .option:hover:not(:disabled) {
    background: #1b1b1e;
    border-color: rgba(255, 179, 46, 0.45);
  }
  .option.picked {
    border-color: var(--amber);
    box-shadow: inset 0 0 0 1px rgba(255, 179, 46, 0.35);
  }
  .option:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .o-label {
    font-weight: 550;
    white-space: nowrap;
  }
  .o-desc {
    color: var(--dim);
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
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
  .header {
    display: flex;
    align-items: center;
    gap: 8px;
    height: 26px;
    animation: row-in 300ms cubic-bezier(0.3, 1.3, 0.45, 1) backwards;
  }
  .header .tag {
    color: #d1d1d6;
    font-size: 10px;
  }
  .header-counts {
    margin-left: auto;
    display: flex;
    gap: 10px;
  }
  .hc {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
  }
  .green-text {
    color: var(--green);
  }
  /* Scroll viewport: every live session is delivered, but only
     EXPANDED_VISIBLE_ROWS of them fit the island (max-height comes from Rust,
     which sizes the window to match). The rows keep their 6px bleed, so the
     list carries the same padding/negative-margin pair — nothing is clipped
     sideways. */
  .list {
    overflow-y: auto;
    overflow-x: hidden;
    overscroll-behavior: contain;
    /* Styling the scrollbar makes it consume layout width; reserving the
       gutter keeps the rows from jumping 3px the moment the list overflows. */
    scrollbar-gutter: stable;
    padding: 0 6px;
    margin: 0 -6px;
  }
  /* Hairline rail, not a browser scrollbar: invisible until there is
     something to scroll, amber under the pointer. */
  .list::-webkit-scrollbar {
    width: 3px;
  }
  .list::-webkit-scrollbar-track {
    background: transparent;
  }
  .list::-webkit-scrollbar-thumb {
    background: rgba(255, 255, 255, 0.13);
    border-radius: 99px;
    transition: background 140ms ease;
  }
  .list:hover::-webkit-scrollbar-thumb {
    background: rgba(255, 179, 46, 0.6);
    box-shadow: 0 0 6px rgba(255, 179, 46, 0.35);
  }
  /* Feather only the edge that has more behind it, so the top row is never
     dimmed at rest — the fade IS the "there is more" signal. */
  .list.more-below {
    -webkit-mask-image: linear-gradient(to bottom, #000 calc(100% - 24px), transparent);
    mask-image: linear-gradient(to bottom, #000 calc(100% - 24px), transparent);
  }
  .list.more-above {
    -webkit-mask-image: linear-gradient(to bottom, transparent 0, #000 20px);
    mask-image: linear-gradient(to bottom, transparent 0, #000 20px);
  }
  .list.more-above.more-below {
    -webkit-mask-image: linear-gradient(
      to bottom,
      transparent 0,
      #000 20px,
      #000 calc(100% - 24px),
      transparent
    );
    mask-image: linear-gradient(
      to bottom,
      transparent 0,
      #000 20px,
      #000 calc(100% - 24px),
      transparent
    );
  }
  .row {
    display: flex;
    align-items: center;
    gap: 9px;
    height: 34px;
    flex: none;
    border-top: 1px solid rgba(255, 255, 255, 0.05);
    animation: row-in 300ms cubic-bezier(0.3, 1.3, 0.45, 1) backwards;
  }
  button.row {
    width: 100%;
    background: none;
    border-radius: 8px;
    padding: 0 6px;
    margin: 0 -6px;
    font: inherit;
    color: inherit;
    text-align: left;
    cursor: pointer;
    transition: background 120ms ease;
  }
  button.row:hover {
    background: rgba(255, 255, 255, 0.06);
  }
  button.row:active {
    background: rgba(255, 255, 255, 0.1);
  }
  .row-go {
    flex: none;
    color: #48484a;
    font-size: 11px;
    opacity: 0;
    transition: opacity 120ms ease;
  }
  button.row:hover .row-go {
    opacity: 1;
    color: var(--amber);
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
  /* On the question card the ✕ ("answer in the terminal instead") belongs at
     the far edge, not butted against the project crumb. */
  .question .dismiss {
    margin-left: auto;
  }
</style>
