<script lang="ts">
  // Tiny agent glyphs: a coral starburst for Claude, a hex ring for Codex, a
  // blue-white sparkle for the fleet — evocative, not reproductions of the
  // trademarked marks.
  //
  // The fleet mark matters as much as the other two: a `gate.failed` row is
  // work no agent in this list did, and wearing Claude's glyph would say
  // otherwise.
  let { agent, size = 10 }: { agent: string; size?: number } = $props();
  const kind = $derived(
    agent.toLowerCase().includes("suite")
      ? "suite"
      : agent.toLowerCase().includes("codex")
        ? "codex"
        : "claude",
  );
</script>

{#if kind === "claude"}
  <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
    <g stroke="#da7756" stroke-width="3.4" stroke-linecap="round">
      <line x1="12" y1="2.5" x2="12" y2="21.5" />
      <line x1="3.8" y1="7.25" x2="20.2" y2="16.75" />
      <line x1="20.2" y1="7.25" x2="3.8" y2="16.75" />
    </g>
  </svg>
{:else if kind === "suite"}
  <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
    <polygon
      points="12,1.6 14.1,9.9 22.4,12 14.1,14.1 12,22.4 9.9,14.1 1.6,12 9.9,9.9"
      fill="#9fd3ff"
    />
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
