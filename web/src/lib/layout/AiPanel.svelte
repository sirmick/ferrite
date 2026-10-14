<script lang="ts">
  import { ai, AI_MODES, type AiMode, clearedAtMs, renderMarkdown } from '$lib/ai/store.svelte';
  import { decoders } from '$lib/decoders/store.svelte';
  import AnalysisCard from '$lib/analysis/AnalysisCard.svelte';
  import TranscriptFlow from '$lib/transcript/TranscriptFlow.svelte';
  import type { AnalysisRecord } from '$lib/analysis/types';
  import type { Turn } from '$lib/ai/store.svelte';
  import {
    fmtHz,
    isShell,
    resultSummary,
    signalsMetaOf,
    signalsOf,
    transcriptOf,
    toolLabel,
    toolSummary,
    toolWhy,
  } from '$lib/ai/toolView';
  import { tick } from 'svelte';

  let scroller: HTMLDivElement | undefined = $state();
  let input = $state('');
  let stickToBottom = $state(true);

  // One timeline, by time: conversation turns, analysis records (whoever
  // ran the measurement — the AI, the CLI, the UI), and activity — the
  // store's `activity` kind, one record per mutating request (a tune, a
  // preset load, an AI prompt over REST…), minus an agent's MCP calls
  // (its tool cards show those). Adjacent activity collapses into one
  // compact block.
  interface Act {
    seq: number;
    t: number;
    text: string;
    note: string;
    error: boolean;
  }
  type Item =
    | { k: 'turn'; key: string; t: number; turn: Turn }
    | { k: 'analysis'; key: string; t: number; rec: AnalysisRecord }
    | { k: 'activity'; key: string; t: number; acts: Act[] };
  /** Analysis records (graphics) by id, for tool cards to draw inline. */
  let analysisById = $derived.by(() => {
    const m = new Map<string, AnalysisRecord>();
    for (const r of decoders.kind('analysis').recent) {
      const d = r.data as unknown as AnalysisRecord;
      if (d?.id) m.set(d.id, d);
    }
    return m;
  });
  /** Ids a tool card in the conversation already shows. */
  let attached = $derived.by(() => {
    const ids = new Set<string>();
    for (const t of ai.turns) {
      if (t.role !== 'assistant') continue;
      for (const c of t.chunks) if (c.kind === 'tool' && c.analysisId) ids.add(c.analysisId);
    }
    return ids;
  });

  let timeline = $derived.by<Item[]>(() => {
    const items: Item[] = ai.turns.map((turn) => ({
      k: 'turn' as const,
      key: `t${turn.id}`,
      t: turn.t,
      turn,
    }));
    // Clear empties the `ai` kind and leaves a `clear` record; the
    // shared activity / analysis logs stay, but this timeline starts
    // from there.
    const clearedAt = clearedAtMs(decoders.kind('ai').recent);
    for (const r of decoders.kind('analysis').recent.slice(-40)) {
      if (r.at_ms < clearedAt) continue;
      // Drawn inside the tool card of the call that made it.
      if (attached.has(String((r.data as { id?: unknown })?.id ?? ''))) continue;
      items.push({
        k: 'analysis',
        key: `a${r.seq}`,
        t: r.at_ms,
        rec: r.data as unknown as AnalysisRecord,
      });
    }
    for (const r of decoders.kind('activity').recent.slice(-80)) {
      if (r.at_ms < clearedAt) continue;
      const d = (r.data ?? {}) as Record<string, unknown>;
      // An agent's MCP tool calls already show as its tool cards.
      if (d['via'] === 'mcp') continue;
      const status = Number(d['status'] ?? 200);
      items.push({
        k: 'activity',
        key: `v${r.seq}`,
        t: r.at_ms,
        acts: [
          {
            seq: r.seq,
            t: r.at_ms,
            text: String(d['summary'] ?? d['command'] ?? ''),
            note: String(d['note'] ?? ''),
            error: status >= 400,
          },
        ],
      });
    }
    // Stable: turns keep their own order; the rest slot in by time.
    const sorted = items
      .map((it, i) => ({ it, i }))
      .sort((a, b) => a.it.t - b.it.t || a.i - b.i)
      .map(({ it }) => it);
    const out: Item[] = [];
    for (const it of sorted) {
      const prev = out.at(-1);
      if (it.k === 'activity' && prev?.k === 'activity') {
        out[out.length - 1] = { ...prev, acts: [...prev.acts, ...it.acts] };
      } else out.push(it);
    }
    return out;
  });

  $effect(() => {
    void timeline.length;
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
    // Slash-command escape: anything starting with `/` is intercepted
    // locally and not sent to the AI. Keeps the chat input the single
    // entry point for both AI conversation and panel control without
    // adding a separate command bar.
    if (text.startsWith('/')) {
      handleSlashCommand(text);
      input = '';
      return;
    }
    ai.send(text);
    input = '';
  }

  /** Recognise built-in slash commands the user types into the chat
   *  input. Unknown commands are surfaced as a hint rather than sent
   *  to the AI so a typo (`/rseet`) doesn't blow a turn.
   *
   *  `/reset` (alias `/clear`, `/new`) drops the SDK session and the
   *  transcript. The next user message starts a fresh session whose
   *  system prompt the sidecar rebuilds from scratch — including the
   *  current driver_notes / setup_description / local_time / mode —
   *  so the AI sees an up-to-date snapshot of the operator's rig
   *  rather than a stale resumed view. */
  function handleSlashCommand(text: string) {
    const [cmd, ...rest] = text.slice(1).trim().split(/\s+/);
    switch (cmd.toLowerCase()) {
      case 'reset':
      case 'clear':
      case 'new':
        ai.resetSession({ reason: rest.join(' ').trim() || undefined });
        return;
      case 'help':
        ai.pushMeta('slash commands: /reset — drop session + reload context on next turn');
        return;
      default:
        ai.pushMeta(`unknown slash command: /${cmd} (try /help)`);
        return;
    }
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

  /** Visible truncated form of a line; the full text rides on `title`. */
  function truncate(s: string, max = 160): string {
    return s.length <= max ? s : s.slice(0, max - 1) + '…';
  }

  function pretty(s: string): string {
    try {
      return JSON.stringify(JSON.parse(s), null, 2);
    } catch {
      return s;
    }
  }

  /** `claude-opus-5-5` → `opus-5-5`. */
  function shortModel(m: string | undefined): string {
    return m ? m.replace(/^claude-/, '') : '';
  }

  // "Show full JSON" toggle per-chunk, keyed by chunk id. The default
  // is collapsed (clean summary only); expand reveals the raw input
  // JSON the way it used to render.
  let expandedTools = $state<Record<string, boolean>>({});
  function toggleTool(id: string) {
    expandedTools[id] = !expandedTools[id];
  }
  /** Thinking blocks the operator folded, keyed `turn:chunk` (open by default). */
  let collapsedThoughts = $state<Record<string, boolean>>({});

  // Setup-description text-box is collapsed by default once filled —
  // keeps the chat real estate intact. Defaults to expanded when
  // empty so first-time users see the prompt.
  let setupOpen = $state(false);
  let setupDraft = $state('');
  function openSetup() {
    setupDraft = ai.setupDescription;
    setupOpen = true;
  }
  function saveSetup() {
    void ai.setSetupDescription(setupDraft);
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
  <div class="flex items-center justify-between gap-2 border-b border-slate-800 px-2 py-1">
    <div class="flex min-w-0 items-center gap-2">
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
      <select
        class="rounded border border-slate-700 bg-slate-900 px-1 py-0.5 text-[10px] focus:border-slate-500 focus:outline-none"
        value={ai.model ?? ''}
        onchange={(e) => ai.setModel((e.currentTarget as HTMLSelectElement).value || null)}
        title="Claude model for this mode (blank = the sidecar's per-mode default)"
      >
        <option value=""
          >{shortModel(ai.defaultModelFor(ai.mode) ?? undefined) || 'default'} · default</option
        >
        {#each ai.modelOptions as id (id)}
          <option value={id}>{shortModel(id)}</option>
        {/each}
      </select>
    </div>
    <div class="flex shrink-0 items-center gap-2">
      <span
        class="inline-block h-2 w-2 rounded-full {connectionLabel.dot}"
        title={connectionLabel.text}
      ></span>
      {#if ai.connection !== 'connected'}
        <span class="max-w-40 truncate text-[10px] text-[color:var(--color-muted)]"
          >{connectionLabel.text}</span
        >
      {/if}
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
          Station notes — every AI conversation starts with these
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
        onclick={openSetup}
        title={ai.setupDescription || 'describe your station for the AI'}
      >
        <span class="flex-1 truncate">
          {#if ai.setupDescription.trim()}
            <span class="text-slate-300">station:</span>
            {ai.setupDescription.replace(/\s+/g, ' ').slice(0, 96)}{ai.setupDescription.length > 96
              ? '…'
              : ''}
          {:else}
            <span class="italic">+ station notes (antennas, interferers, …)</span>
          {/if}
        </span>
        <span class="shrink-0">▾</span>
      </button>
    {/if}
  </div>

  <!-- Transcript -->
  <div
    class="min-h-0 flex-1 overflow-x-hidden overflow-y-auto px-2 py-2"
    bind:this={scroller}
    onscroll={onScroll}
  >
    {#if timeline.length === 0}
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
    {#each timeline as item (item.key)}
      {#if item.k === 'analysis'}
        <AnalysisCard rec={item.rec} t={item.t} />
      {:else if item.k === 'activity'}
        <div
          class="mb-2 border-l-2 border-slate-700 pl-1.5 font-mono text-[10px] leading-tight text-slate-400"
          title="activity — mutating requests (ferrite/state/activity)"
        >
          {#each item.acts as e (e.seq)}
            <div class="flex items-baseline gap-2">
              <span class="text-slate-600">{fmtTime(e.t)}</span>
              <span class:text-rose-400={e.error}>⇢ {e.text}</span>
              {#if e.note}<span class="truncate italic text-slate-500">{e.note}</span>{/if}
            </div>
          {/each}
        </div>
      {:else}
        {@const turn = item.turn}
        {#if turn.role === 'user'}
          <div class="mb-2 ml-6 rounded border border-slate-700 bg-slate-900/60 px-2 py-1">
            <div
              class="mb-0.5 flex items-center justify-between text-[10px] text-[color:var(--color-muted)]"
            >
              <span>{turn.source && turn.source !== 'ferrite-ai' ? turn.source : 'you'}</span>
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
                ai{#if turn.source && turn.source !== 'ferrite-ai'}<span class="ml-1 opacity-70"
                    >· {turn.source}</span
                  >{/if}{#if turn.mode}<span class="ml-1 opacity-70">· {turn.mode}</span
                  >{/if}{#if turn.model}<span class="ml-1 opacity-50"
                    >· {shortModel(turn.model)}</span
                  >{/if}
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
              {:else if chunk.kind === 'thought'}
                <!-- The model's summarised thinking, open by default; folds
                   to one line. -->
                {@const key = `${turn.id}:${idx}`}
                {@const open = !collapsedThoughts[key]}
                <div
                  class="my-1 border-l-2 border-violet-800 pl-1.5 text-[10px] text-violet-300/80"
                >
                  <button
                    type="button"
                    class="flex w-full items-center gap-1 text-left hover:text-violet-200"
                    onclick={() => (collapsedThoughts[key] = open)}
                    title={open ? 'fold the thinking' : "show the model's thinking"}
                  >
                    <span class="shrink-0 whitespace-nowrap">{open ? '▾' : '▸'} thinking</span>
                    {#if !open}
                      <span class="truncate italic opacity-70">{chunk.text.slice(-160)}</span>
                    {:else}
                      <span class="opacity-60">· {chunk.text.length} chars</span>
                    {/if}
                  </button>
                  {#if open}
                    <!-- eslint-disable-next-line svelte/no-at-html-tags -->
                    <div class="ai-markdown italic">{@html renderMarkdown(chunk.text)}</div>
                  {/if}
                </div>
              {:else if chunk.kind === 'plan'}
                <div class="my-1 rounded border border-slate-800 px-1.5 py-1 text-[10px]">
                  <div class="text-[color:var(--color-muted)]">plan</div>
                  {#each chunk.entries as e, i (i)}
                    <div
                      class:text-emerald-400={e.status === 'completed'}
                      class:text-amber-300={e.status === 'in_progress'}
                      class:text-slate-400={e.status !== 'completed' && e.status !== 'in_progress'}
                    >
                      {e.status === 'completed' ? '☑' : e.status === 'in_progress' ? '◐' : '☐'}
                      {e.content}
                    </div>
                  {/each}
                </div>
              {:else if chunk.kind === 'tool'}
                {@const summary = toolSummary(chunk.name, chunk.input)}
                {@const why = toolWhy(chunk.input)}
                {@const outcome = resultSummary(chunk.result, !!chunk.error)}
                {@const shell = isShell(chunk.name)}
                {@const signals = signalsOf(chunk.name, chunk.result)}
                {@const signalsMeta = signalsMetaOf(chunk.name, chunk.result)}
                {@const spoken = transcriptOf(chunk.result)}
                {@const graphic = chunk.analysisId ? analysisById.get(chunk.analysisId) : undefined}
                <div
                  class="my-1 rounded border px-1.5 py-1 font-mono text-[10px]"
                  class:border-slate-700={!chunk.error}
                  class:border-rose-700={chunk.error}
                  class:bg-slate-900={!chunk.error}
                  class:bg-rose-950={chunk.error}
                >
                  <div class="flex items-center justify-between gap-2">
                    <span class="flex min-w-0 flex-1 items-center gap-1.5">
                      {#if shell}
                        <span class="truncate text-amber-200" title={summary}
                          >{truncate(summary)}</span
                        >
                      {:else}
                        <span class="shrink-0 text-sky-400">⚙ {toolLabel(chunk.name)}</span>
                        <span class="truncate text-slate-300" title={summary}
                          >{truncate(summary)}</span
                        >
                      {/if}
                    </span>
                    <span class="flex shrink-0 items-center gap-2">
                      {#if chunk.interrupted}
                        <span class="text-slate-500">interrupted</span>
                      {:else if chunk.result === undefined}
                        <span class="animate-pulse text-amber-400">running…</span>
                      {:else if chunk.error}
                        <span class="text-rose-400">failed</span>
                      {:else}
                        <span class="text-emerald-400">✓</span>
                      {/if}
                      <button
                        type="button"
                        class="rounded border border-slate-700 px-1 text-[9px] text-[color:var(--color-muted)] hover:border-slate-500"
                        onclick={() => toggleTool(chunk.id)}
                        title="show the full input and result"
                        >{expandedTools[chunk.id] ? '−' : '+'}</button
                      >
                    </span>
                  </div>
                  {#if why}
                    <div class="mt-0.5 truncate font-sans italic text-slate-400" title={why}>
                      {why}
                    </div>
                  {/if}
                  {#if outcome}
                    <div
                      class="mt-0.5 truncate"
                      class:text-rose-300={chunk.error}
                      class:text-slate-400={!chunk.error}
                      title={outcome}
                    >
                      → {outcome}
                    </div>
                  {/if}
                  {#if signals && signals.length}
                    <table class="mt-1 w-full text-left text-[10px]">
                      <thead class="text-[color:var(--color-muted)]">
                        <tr
                          ><th class="font-normal">freq</th><th class="font-normal">power</th><th
                            class="font-normal">SNR</th
                          ><th class="font-normal">width</th></tr
                        >
                      </thead>
                      <tbody class="text-slate-300">
                        {#each signals as r, i (i)}
                          <tr>
                            <td>{fmtHz(r.freq_hz)}</td>
                            <td>{r.power_db !== undefined ? `${r.power_db.toFixed(1)} dB` : ''}</td>
                            <td>{r.snr_db !== undefined ? `${r.snr_db.toFixed(1)} dB` : ''}</td>
                            <td>{r.bw_hz !== undefined ? fmtHz(r.bw_hz) : ''}</td>
                          </tr>
                        {/each}
                      </tbody>
                    </table>
                    {#if signalsMeta && (signalsMeta.listed > signals.length || signalsMeta.total > signalsMeta.listed)}
                      <div class="mt-0.5 text-[10px] text-[color:var(--color-muted)]">
                        {signals.length} of {signalsMeta.listed} listed · {signalsMeta.total} tracked
                      </div>
                    {/if}
                  {/if}
                  {#if spoken}
                    <!-- Transcript records in the result: the same widget,
                         labels and speaker colours as the main view. -->
                    <div class="mt-1 rounded border border-slate-800 bg-slate-950/40 p-1">
                      <TranscriptFlow lines={spoken} compact />
                    </div>
                  {/if}
                  {#if chunk.resultImages && chunk.resultImages.length}
                    <div class="mt-1 flex flex-col gap-1">
                      {#each chunk.resultImages as src, i (i)}
                        <!-- An image the tool returned (a view snapshot, a
                             reference picture); click for full size. -->
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
                  {#if graphic}
                    <!-- The graphic this call drew: spectrum, spectrogram,
                         envelope, sigwiki card, spot map. -->
                    <div class="mt-1 font-sans">
                      <AnalysisCard rec={graphic} t={turn.t} inline />
                    </div>
                  {/if}
                  {#if expandedTools[chunk.id]}
                    {#if chunk.input}
                      <pre
                        class="mt-1 max-h-32 overflow-auto whitespace-pre-wrap rounded bg-slate-950/60 px-1.5 py-1 text-slate-300">{pretty(
                          chunk.input,
                        )}</pre>
                    {/if}
                    {#if chunk.result}
                      <pre
                        class="mt-1 max-h-48 overflow-auto whitespace-pre-wrap text-slate-400">{pretty(
                          chunk.result,
                        )}</pre>
                    {/if}
                  {/if}
                </div>
              {:else}
                <div class="text-[10px] italic text-[color:var(--color-muted)]">
                  {chunk.label}
                </div>
              {/if}
            {/each}
          </div>
        {/if}
      {/if}
    {/each}
  </div>

  <!-- Input -->
  <div class="border-t border-slate-800 px-2 py-1">
    <div class="flex items-stretch gap-1">
      <textarea
        class="min-h-[2.5rem] flex-1 resize-none rounded border border-slate-700 bg-slate-900 px-2 py-1 text-[12px] focus:border-slate-500 focus:outline-none disabled:opacity-50"
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
      <!-- Stop button: enabled while the AI is mid-turn. Sends
           {type: "stop"} to the sidecar; the SDK's per-turn
           AbortController cancels the in-flight query, and the
           sidecar replies with `ferrite_ai_stopped` which the
           store renders as a `── stopped (user)` meta line. -->
      <button
        type="button"
        class="rounded border border-rose-700 bg-rose-900/30 px-2 text-[11px] font-medium text-rose-200 hover:border-rose-500 hover:bg-rose-900/50 disabled:cursor-not-allowed disabled:border-slate-700 disabled:bg-slate-900 disabled:text-slate-600"
        onclick={() => ai.stop()}
        disabled={!ai.isStreaming()}
        title="stop the AI mid-turn"
      >
        ◼ stop
      </button>
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
