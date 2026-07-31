<script lang="ts">
  // Tiny agent glyphs: a coral starburst for Claude, a hex ring for Codex —
  // evocative, not reproductions of the trademarked marks.
  let { agent, size = 10 }: { agent: string; size?: number } = $props();
  const kind = $derived(agent.toLowerCase().includes("codex") ? "codex" : "claude");
</script>

{#if kind === "claude"}
  <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
    <g stroke="#da7756" stroke-width="3.4" stroke-linecap="round">
      <line x1="12" y1="2.5" x2="12" y2="21.5" />
      <line x1="3.8" y1="7.25" x2="20.2" y2="16.75" />
      <line x1="20.2" y1="7.25" x2="3.8" y2="16.75" />
    </g>
  </svg>
{:else}
  <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
    <polygon
      points="12,2.8 20.2,7.4 20.2,16.6 12,21.2 3.8,16.6 3.8,7.4"
      fill="none"
      stroke="#e8e8ed"
      stroke-width="2.6"
      stroke-linejoin="round"
    />
    <circle cx="12" cy="12" r="2.2" fill="#e8e8ed" />
  </svg>
{/if}
