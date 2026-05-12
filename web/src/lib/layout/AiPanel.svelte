<script lang="ts">
  import { ai, AI_MODES, type AiMode, renderMarkdown } from '$lib/ai/store.svelte';
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

  /** Short, tool-specific summary line for the collapsed view of a
   *  tool call. Bash shows `$ <command>`, Read/Glob/Grep show their
   *  primary arg, unknown tools fall back to a one-line JSON. Long
   *  results get truncated; the full text rides on a `title` tooltip
   *  for hover and the full JSON is still available in the
   *  "show details" expander. */
  function toolSummary(name: string, input: string): string {
    const obj = tryParse(input);
    if (obj && typeof obj === 'object') {
      const o = obj as Record<string, unknown>;
      if (name === 'Bash' && typeof o.command === 'string') return `$ ${o.command}`;
      if (name === 'Read' && typeof o.file_path === 'string') return `📖 ${o.file_path}`;
      if (name === 'Glob' && typeof o.pattern === 'string') return `🔍 ${o.pattern}`;
      if (name === 'Grep' && typeof o.pattern === 'string') {
        const where = typeof o.path === 'string' ? ` in ${o.path}` : '';
        return `🔍 /${o.pattern}/${where}`;
      }
    }
    return input ? input.replace(/\s+/g, ' ') : '';
  }

  /** Optional sub-line — used by Bash to show the `description` field
   *  below the command. Returns null when there's nothing extra to
   *  show. */
  function toolSubline(name: string, input: string): string | null {
    const obj = tryParse(input);
    if (!obj || typeof obj !== 'object') return null;
    const o = obj as Record<string, unknown>;
    if (name === 'Bash' && typeof o.description === 'string' && o.description) {
      return o.description;
    }
    return null;
  }

  /** Visible truncated form of the summary line; the full text lives
   *  on the `title` attribute for hover-to-see. */
  function truncate(s: string, max = 160): string {
    if (s.length <= max) return s;
    return s.slice(0, max - 1) + '…';
  }

  function tryParse(s: string): unknown {
    if (!s) return null;
    try {
      return JSON.parse(s);
    } catch {
      return null;
    }
  }

  // "Show full JSON" toggle per-chunk, keyed by chunk id. The default
  // is collapsed (clean summary only); expand reveals the raw input
  // JSON the way it used to render.
  let expandedTools = $state<Record<string, boolean>>({});
  function toggleTool(id: string) {
    expandedTools[id] = !expandedTools[id];
  }

  // Setup-description text-box is collapsed by default once filled —
  // keeps the chat real estate intact. Defaults to expanded when
  // empty so first-time users see the prompt.
  let setupOpen = $state(ai.setupDescription.trim() === '');
  let setupDraft = $state(ai.setupDescription);
  function saveSetup() {
    ai.setSetupDescription(setupDraft);
    setupOpen = false;
  }
  function cancelSetup() {
    setupDraft = ai.setupDescription;
    setupOpen = false;
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

  <!-- Operator-supplied radio setup. Sent to the sidecar on every
       turn so the AI knows which antenna is hooked to what, where
       known interferers sit, etc. Collapsed by default once filled. -->
  <div class="border-b border-slate-800 bg-slate-950/40 px-2 py-1">
    {#if setupOpen}
      <div class="mb-1 flex items-center justify-between">
        <span class="text-[10px] font-semibold text-[color:var(--color-muted)]">
          Radio setup (sent to AI on every turn)
        </span>
        <div class="flex gap-1">
          <button
            type="button"
            class="rounded border border-slate-700 px-1.5 py-0.5 text-[10px] hover:border-slate-500"
            onclick={cancelSetup}>cancel</button
          >
          <button
            type="button"
            class="rounded border border-emerald-700 bg-emerald-900/30 px-1.5 py-0.5 text-[10px] text-emerald-300 hover:border-emerald-500"
            onclick={saveSetup}>save</button
          >
        </div>
      </div>
      <textarea
        class="w-full resize-y rounded border border-slate-700 bg-slate-900 px-2 py-1 text-[11px] focus:border-slate-500 focus:outline-none"
        rows="4"
        bind:value={setupDraft}
        placeholder={'e.g. Antenna A: 1m vertical at window (good VHF/UHF).\nAntenna B: dipole in attic (40m/80m).\nAntenna C: nothing connected.\nKnown noise: switching PSU on south wall (~30 dB hash 1–8 MHz).'}
      ></textarea>
    {:else}
      <button
        type="button"
        class="flex w-full items-center justify-between gap-2 text-left text-[10px] text-[color:var(--color-muted)] hover:text-slate-200"
        onclick={() => (setupOpen = true)}
        title={ai.setupDescription || 'set up your radio context'}
      >
        <span class="flex-1 truncate">
          {#if ai.setupDescription.trim()}
            <span class="text-slate-300">setup:</span>
            {ai.setupDescription.replace(/\s+/g, ' ').slice(0, 96)}{ai.setupDescription.length > 96
              ? '…'
              : ''}
          {:else}
            <span class="italic">+ describe your radio setup (antennas, interferers, …)</span>
          {/if}
        </span>
        <span class="shrink-0">▾</span>
      </button>
    {/if}
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
              <!-- Render AI text as Markdown. The store's renderMarkdown
                   pipes through marked + DOMPurify so AI outputs like
                   `**bold**`, `# heading`, fenced code, lists, etc. show
                   correctly instead of as literal Markdown syntax. The
                   DOMPurify pass scrubs script tags / event handlers so
                   the {@html} below is safe — the eslint warning is a
                   false positive against our sanitized output. -->
              <!-- eslint-disable-next-line svelte/no-at-html-tags -->
              <div class="ai-markdown text-slate-100">{@html renderMarkdown(chunk.text)}</div>
            {:else if chunk.kind === 'tool'}
              {@const summary = toolSummary(chunk.name, chunk.input)}
              {@const sub = toolSubline(chunk.name, chunk.input)}
              <div
                class="my-1 rounded border px-1.5 py-1 font-mono text-[10px]"
                class:border-slate-700={!chunk.error}
                class:border-rose-700={chunk.error}
                class:bg-slate-900={!chunk.error}
                class:bg-rose-950={chunk.error}
              >
                <div
                  class="flex items-center justify-between gap-2 text-[color:var(--color-muted)]"
                >
                  <span class="flex min-w-0 flex-1 items-center gap-1.5">
                    <span class="shrink-0 text-sky-400">⚙ {chunk.name}</span>
                    <span class="truncate text-slate-300" title={summary}>{truncate(summary)}</span>
                  </span>
                  <span class="flex shrink-0 items-center gap-2">
                    {#if chunk.result === undefined}
                      <span class="text-amber-400">running…</span>
                    {:else if chunk.error}
                      <span class="text-rose-400">error</span>
                    {:else}
                      <span class="text-emerald-400">done</span>
                    {/if}
                    {#if chunk.input}
                      <button
                        type="button"
                        class="rounded border border-slate-700 px-1 text-[9px] hover:border-slate-500"
                        onclick={() => toggleTool(chunk.id)}
                        title="show full tool input + raw result"
                        >{expandedTools[chunk.id] ? '−' : '+'}</button
                      >
                    {/if}
                  </span>
                </div>
                {#if sub}
                  <div class="mt-0.5 truncate text-[10px] italic text-slate-400" title={sub}>
                    {sub}
                  </div>
                {/if}
                {#if expandedTools[chunk.id] && chunk.input}
                  <pre
                    class="mt-1 max-h-32 overflow-auto whitespace-pre-wrap rounded bg-slate-950/60 px-1.5 py-1 text-slate-300">{(() => {
                      try {
                        return JSON.stringify(JSON.parse(chunk.input), null, 2);
                      } catch {
                        return chunk.input;
                      }
                    })()}</pre>
                {/if}
                {#if chunk.resultImages && chunk.resultImages.length}
                  <div class="mt-1 flex flex-col gap-1">
                    {#each chunk.resultImages as src, i (i)}
                      <!-- Inline render of base64 data URLs returned by Read on a
                           PNG/JPEG. Capped width so a large image doesn't blow
                           out the chat column; click to open the data URL in
                           a new tab for full-size. -->
                      <a href={src} target="_blank" rel="noopener noreferrer">
                        <img
                          {src}
                          alt="tool result"
                          class="max-h-64 max-w-full rounded border border-slate-700"
                        />
                      </a>
                    {/each}
                  </div>
                {/if}
                {#if expandedTools[chunk.id] && chunk.result !== undefined && chunk.result}
                  <pre
                    class="mt-1 max-h-48 overflow-auto whitespace-pre-wrap text-slate-400">{chunk.result}</pre>
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

<!-- Markdown styling for AI text chunks. Svelte's scoped CSS doesn't
     reach `{@html ...}` children unless we use `:global(...)`, so each
     rule is explicit. Kept panel-compact (small headings, tight
     spacing) since the AI panel is a narrow side column. -->
<style>
  :global(.ai-markdown) {
    line-height: 1.45;
  }
  :global(.ai-markdown > *:first-child) {
    margin-top: 0;
  }
  :global(.ai-markdown > *:last-child) {
    margin-bottom: 0;
  }
  :global(.ai-markdown p) {
    margin: 0.35em 0;
  }
  :global(.ai-markdown h1),
  :global(.ai-markdown h2),
  :global(.ai-markdown h3),
  :global(.ai-markdown h4) {
    margin: 0.6em 0 0.25em;
    font-weight: 600;
    color: rgb(241, 245, 249);
  }
  :global(.ai-markdown h1) {
    font-size: 13px;
  }
  :global(.ai-markdown h2) {
    font-size: 12px;
  }
  :global(.ai-markdown h3),
  :global(.ai-markdown h4) {
    font-size: 12px;
    color: rgb(203, 213, 225);
  }
  :global(.ai-markdown strong) {
    font-weight: 600;
    color: rgb(241, 245, 249);
  }
  :global(.ai-markdown em) {
    font-style: italic;
  }
  :global(.ai-markdown ul),
  :global(.ai-markdown ol) {
    margin: 0.35em 0;
    padding-left: 1.25em;
  }
  :global(.ai-markdown ul) {
    list-style: disc;
  }
  :global(.ai-markdown ol) {
    list-style: decimal;
  }
  :global(.ai-markdown li) {
    margin: 0.15em 0;
  }
  :global(.ai-markdown code) {
    background: rgba(15, 23, 42, 0.7);
    border: 1px solid rgb(30, 41, 59);
    border-radius: 3px;
    padding: 0 0.25em;
    font-size: 0.92em;
    font-family:
      ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, 'Liberation Mono', 'Courier New',
      monospace;
  }
  :global(.ai-markdown pre) {
    margin: 0.4em 0;
    padding: 0.5em 0.6em;
    background: rgb(2, 6, 23);
    border: 1px solid rgb(30, 41, 59);
    border-radius: 4px;
    overflow-x: auto;
    font-size: 11px;
    line-height: 1.4;
  }
  :global(.ai-markdown pre code) {
    background: transparent;
    border: 0;
    padding: 0;
    font-size: inherit;
  }
  :global(.ai-markdown a) {
    color: rgb(125, 211, 252);
    text-decoration: underline;
  }
  :global(.ai-markdown blockquote) {
    margin: 0.35em 0;
    padding-left: 0.7em;
    border-left: 2px solid rgb(51, 65, 85);
    color: rgb(148, 163, 184);
  }
  :global(.ai-markdown table) {
    border-collapse: collapse;
    margin: 0.4em 0;
  }
  :global(.ai-markdown th),
  :global(.ai-markdown td) {
    border: 1px solid rgb(30, 41, 59);
    padding: 0.2em 0.5em;
    font-size: 11px;
  }
  :global(.ai-markdown th) {
    background: rgba(15, 23, 42, 0.6);
    font-weight: 600;
  }
  :global(.ai-markdown hr) {
    border: 0;
    border-top: 1px solid rgb(30, 41, 59);
    margin: 0.6em 0;
  }
</style>
