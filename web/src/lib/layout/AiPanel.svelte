<script lang="ts">
  import { ai, AI_MODES, type AiMode } from '$lib/ai/store.svelte';
  import { logs } from '$lib/logs/store.svelte';
  import { tick } from 'svelte';

  let scroller: HTMLDivElement | undefined = $state();
  let input = $state('');
  let stickToBottom = $state(true);

  // The activity panel below the chat is just the existing log store
  // filtered to the `ai::activity` target. Every ferrite-ctl call sets
  // X-Ferrite-Command, so the AI's tool calls (and the user's manual
  // CLI invocations) all surface here.
  const ACTIVITY_TARGET = 'ai::activity';
  let activity = $derived(logs.entries.filter((e) => e.category === ACTIVITY_TARGET).slice(-50));

  $effect(() => {
    void ai.turns.length;
    if (!stickToBottom || !scroller) return;
    void tick().then(() => {
      if (scroller) scroller.scrollTop = scroller.scrollHeight;
    });
  });

  function onScroll() {
    if (!scroller) return;
    const d = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    stickToBottom = d < 32;
  }

  function submit() {
    const text = input.trim();
    if (!text) return;
    ai.send(text);
    input = '';
  }

  function onKeydown(e: KeyboardEvent) {
    // Plain Enter submits; Shift-Enter inserts a newline. Matches the
    // muscle memory of every chat UI built since 2010.
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  }

  function fmtTime(t: number): string {
    const d = new Date(t);
    return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}:${String(d.getSeconds()).padStart(2, '0')}`;
  }

  // Pretty-print a tool input JSON when it parses; otherwise show the
  // raw partial string so a still-streaming tool call still has
  // something to render.
  function fmtToolInput(input: string): string {
    if (!input) return '';
    try {
      return JSON.stringify(JSON.parse(input), null, 2);
    } catch {
      return input;
    }
  }

  let connectionLabel = $derived.by(() => {
    switch (ai.connection) {
      case 'connected':
        return { dot: 'bg-emerald-500', text: 'connected' };
      case 'connecting':
        return { dot: 'bg-amber-400 animate-pulse', text: 'connecting…' };
      case 'closed':
        return { dot: 'bg-slate-500', text: 'closed' };
      case 'error':
        return { dot: 'bg-rose-500', text: ai.lastError ?? 'error' };
      default:
        return { dot: 'bg-slate-600', text: 'idle' };
    }
  });
</script>

<div
  class="flex h-full w-full flex-col border-r border-slate-800 bg-[color:var(--color-bg)] text-xs"
>
  <!-- Header: mode picker + connection dot -->
  <div class="flex items-center justify-between border-b border-slate-800 px-2 py-1">
    <div class="flex items-center gap-2">
      <span class="font-semibold text-[color:var(--color-muted)]">AI</span>
      <select
        class="rounded border border-slate-700 bg-slate-900 px-1 py-0.5 text-[10px] focus:border-slate-500 focus:outline-none"
        value={ai.mode}
        onchange={(e) => ai.setMode((e.currentTarget as HTMLSelectElement).value as AiMode)}
        title="AI mode (different system prompt + tool allow-list)"
      >
        {#each ai.availableModes as m (m)}
          <option value={m}>{m}</option>
        {/each}
      </select>
    </div>
    <div class="flex items-center gap-2">
      <span
        class="inline-block h-2 w-2 rounded-full {connectionLabel.dot}"
        title={connectionLabel.text}
      ></span>
      <span class="text-[10px] text-[color:var(--color-muted)]">{connectionLabel.text}</span>
      <button
        type="button"
        class="rounded border border-slate-700 px-1.5 py-0.5 text-[10px] hover:border-slate-500"
        onclick={() => ai.clear()}
        title="clear transcript"
      >
        clear
      </button>
    </div>
  </div>

  <!-- Transcript -->
  <div class="min-h-0 flex-1 overflow-y-auto px-2 py-2" bind:this={scroller} onscroll={onScroll}>
    {#if ai.turns.length === 0}
      <div class="px-2 py-4 text-[11px] text-[color:var(--color-muted)]">
        Ask the AI to scan a band, identify a signal, or load a preset. Mode picker above swaps the
        system prompt; modes:
        <ul class="mt-1 ml-4 list-disc space-y-0.5">
          {#each AI_MODES as m (m)}
            <li><span class="font-mono text-slate-300">{m}</span></li>
          {/each}
        </ul>
      </div>
    {/if}
    {#each ai.turns as turn (turn.id)}
      {#if turn.role === 'user'}
        <div class="mb-2 ml-6 rounded border border-slate-700 bg-slate-900/60 px-2 py-1">
          <div
            class="mb-0.5 flex items-center justify-between text-[10px] text-[color:var(--color-muted)]"
          >
            <span>you</span>
            <span class="font-mono">{fmtTime(turn.t)}</span>
          </div>
          <div class="whitespace-pre-wrap text-slate-100">{turn.text}</div>
        </div>
      {:else}
        <div class="mb-2 mr-6 rounded border border-slate-800 bg-slate-950/40 px-2 py-1">
          <div
            class="mb-0.5 flex items-center justify-between text-[10px] text-[color:var(--color-muted)]"
          >
            <span>
              ai
              {#if turn.status === 'streaming'}
                <span class="ml-1 animate-pulse">▌</span>
              {:else if turn.status === 'error'}
                <span class="ml-1 text-rose-400">error</span>
              {/if}
            </span>
            <span class="font-mono">{fmtTime(turn.t)}</span>
          </div>
          {#if turn.status === 'error' && turn.errorMessage}
            <div class="text-rose-400">{turn.errorMessage}</div>
          {/if}
          {#each turn.chunks as chunk, idx (idx)}
            {#if chunk.kind === 'text'}
              <div class="whitespace-pre-wrap text-slate-100">{chunk.text}</div>
            {:else if chunk.kind === 'tool'}
              <div
                class="my-1 rounded border px-1.5 py-1 font-mono text-[10px]"
                class:border-slate-700={!chunk.error}
                class:border-rose-700={chunk.error}
                class:bg-slate-900={!chunk.error}
                class:bg-rose-950={chunk.error}
              >
                <div class="flex items-center justify-between text-[color:var(--color-muted)]">
                  <span class="text-sky-400">⚙ {chunk.name}</span>
                  {#if chunk.result === undefined}
                    <span class="text-amber-400">running…</span>
                  {:else if chunk.error}
                    <span class="text-rose-400">error</span>
                  {:else}
                    <span class="text-emerald-400">done</span>
                  {/if}
                </div>
                {#if chunk.input}
                  <pre class="mt-0.5 whitespace-pre-wrap text-slate-300">{fmtToolInput(
                      chunk.input,
                    )}</pre>
                {/if}
                {#if chunk.result !== undefined}
                  <pre
                    class="mt-0.5 max-h-48 overflow-auto whitespace-pre-wrap text-slate-400">{chunk.result}</pre>
                {/if}
              </div>
            {:else}
              <div class="text-[10px] italic text-[color:var(--color-muted)]">— {chunk.label}</div>
            {/if}
          {/each}
        </div>
      {/if}
    {/each}
  </div>

  <!-- Input -->
  <div class="border-t border-slate-800 px-2 py-1">
    <textarea
      class="min-h-[2.5rem] w-full resize-none rounded border border-slate-700 bg-slate-900 px-2 py-1 text-[12px] focus:border-slate-500 focus:outline-none disabled:opacity-50"
      placeholder={ai.connection === 'connected'
        ? 'Ask the AI… (Enter to send, Shift+Enter for newline)'
        : ai.connection === 'connecting'
          ? 'connecting to ferrite-ai…'
          : 'ferrite-ai is offline — start `npm run dev` in tools/ferrite-ai/'}
      rows="2"
      bind:value={input}
      onkeydown={onKeydown}
      disabled={ai.connection !== 'connected'}
    ></textarea>
  </div>

  <!-- Activity transcript: filtered ai::activity log -->
  <div class="flex max-h-[35%] min-h-[6rem] flex-col border-t border-slate-800 bg-slate-950/50">
    <div class="flex items-center justify-between border-b border-slate-800 px-2 py-1">
      <span class="font-semibold text-[color:var(--color-muted)]">Activity</span>
      <span class="text-[10px] text-[color:var(--color-muted)]">
        ai::activity ({activity.length})
      </span>
    </div>
    <div class="min-h-0 flex-1 overflow-y-auto px-2 py-1 font-mono text-[10px]">
      {#if activity.length === 0}
        <div class="text-[color:var(--color-muted)]">
          no activity yet — the AI's CLI calls will appear here
        </div>
      {/if}
      {#each activity as e (e.id)}
        <div class="flex items-baseline gap-2 leading-tight">
          <span class="text-slate-500">{fmtTime(e.t)}</span>
          <span class="flex-1 text-slate-300">{e.text}</span>
        </div>
      {/each}
    </div>
  </div>
</div>
