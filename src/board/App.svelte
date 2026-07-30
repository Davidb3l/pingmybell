<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import type { Session } from "../lib/types";

  let sessions = $state<Record<string, Session>>({});

  const list = $derived(
    Object.values(sessions).sort((a, b) => b.last_event_at - a.last_event_at),
  );

  onMount(() => {
    const unlisten = listen<Session>("session-updated", (e) => {
      if (e.payload.state === "ended") {
        delete sessions[e.payload.id];
      } else {
        sessions[e.payload.id] = e.payload;
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  });
</script>

<main>
  <h1>PingMyBell</h1>
  {#if list.length === 0}
    <p class="empty">No live sessions. Events from agent hooks will appear here.</p>
  {:else}
    <ul>
      {#each list as s (s.id)}
        <li>
          <span class="agent">{s.agent}</span>
          <span class="title">{s.title}</span>
          <span class="state state-{s.state}">{s.state}</span>
        </li>
      {/each}
    </ul>
  {/if}
</main>

<style>
  main {
    font-family: system-ui, sans-serif;
    padding: 1rem 1.5rem;
  }
  h1 {
    font-size: 1.1rem;
  }
  .empty {
    color: #888;
  }
  ul {
    list-style: none;
    padding: 0;
  }
  li {
    display: flex;
    gap: 0.75rem;
    padding: 0.4rem 0;
    border-bottom: 1px solid #eee;
  }
  .agent {
    color: #888;
  }
  .title {
    font-weight: 600;
  }
  .state {
    margin-left: auto;
  }
  .state-needs_attention {
    color: #c60;
  }
  .state-done {
    color: #2a7;
  }
</style>
