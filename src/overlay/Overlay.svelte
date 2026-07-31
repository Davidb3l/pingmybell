<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";

  // Dumb renderer: Rust owns the overlay state machine and window sizing;
  // this component only draws the current OverlayView payload.
  type Counts = { working: number; attention: number; done: number };
  type Toast = { agent: string; title: string; state: string; summary: string };
  type View = {
    mode: "idle" | "toast";
    has_notch: boolean;
    counts: Counts;
    toast: Toast | null;
  };

  let view = $state<View>({
    mode: "idle",
    has_notch: false,
    counts: { working: 0, attention: 0, done: 0 },
    toast: null,
  });

  const total = $derived(view.counts.working + view.counts.attention + view.counts.done);

  onMount(() => {
    const unlisten = listen<View>("overlay-state", (e) => {
      view = e.payload;
    });
    return () => {
      unlisten.then((f) => f());
    };
  });

  function dots(n: number): number[] {
    return Array.from({ length: Math.min(n, 5) }, (_, i) => i);
  }
</script>

<div class="shell" class:notch={view.has_notch} class:toast-mode={view.mode === "toast"}>
  {#if view.mode === "idle"}
    <div class="idle-strip">
      {#if total > 0}
        {#each dots(view.counts.attention) as i (`a${i}`)}<span class="dot attention"></span>{/each}
        {#each dots(view.counts.working) as i (`w${i}`)}<span class="dot working"></span>{/each}
        {#each dots(view.counts.done) as i (`d${i}`)}<span class="dot done"></span>{/each}
      {/if}
    </div>
  {:else if view.toast}
    <div class="toast">
      <span class="badge state-{view.toast.state}"></span>
      <span class="title">{view.toast.title}</span>
      <span class="summary">{view.toast.summary}</span>
    </div>
  {/if}
</div>

<style>
  .shell {
    box-sizing: border-box;
    width: 100vw;
    height: 100vh;
    background: #000;
    border-radius: 0 0 14px 14px;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    overflow: hidden;
    font-family:
      -apple-system,
      system-ui,
      sans-serif;
  }
  /* Floating pill (non-notch / Windows): fully rounded. */
  .shell:not(.notch) {
    border-radius: 15px;
    align-items: center;
  }

  .idle-strip {
    display: flex;
    gap: 5px;
    align-items: center;
    padding: 0 10px 5px;
  }
  .shell:not(.notch) .idle-strip {
    padding: 0 10px;
  }
  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
  }
  .dot.working {
    background: #4a9eff;
  }
  .dot.attention {
    background: #ffb02e;
    animation: pulse 1.2s ease-in-out infinite;
  }
  .dot.done {
    background: #34c759;
  }
  @keyframes pulse {
    50% {
      opacity: 0.35;
    }
  }

  .toast {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 0 14px 8px;
    color: #eee;
    font-size: 13px;
    line-height: 1.2;
    animation: fade-in 180ms ease-out;
  }
  .shell:not(.notch) .toast {
    padding: 0 14px;
  }
  @keyframes fade-in {
    from {
      opacity: 0;
      transform: translateY(-4px);
    }
  }
  .badge {
    flex: none;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #4a9eff;
  }
  .badge.state-done {
    background: #34c759;
  }
  .badge.state-needs_attention {
    background: #ffb02e;
  }
  .title {
    flex: none;
    font-weight: 600;
    color: #fff;
  }
  .summary {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: #b8b8b8;
  }
</style>
