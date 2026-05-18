<script lang="ts">
  // Advanced view for in-browser speech-to-text.
  //
  // Renders the VAD-gated transcription segments the Worker produces,
  // stamped with wall-clock + VFO frequency so it doubles as a band
  // log ("set it and leave it"). Low-confidence tokens are dimmed so
  // the operator's eye does the disambiguation — the right UX for
  // noisy SSB review. Mounted (via the registry) only when the browser
  // runtime has a VoiceTranscribe tap, i.e. the receiver's Audio
  // control is set to "transcribe".
  //
  // Pure view: engage (the receiver's Audio chip → "transcribe") is a
  // build-time profile axis; all inference is in the Worker. This only
  // reads the reactive `transcript` store and the whisper prompt.

  import { applyControl } from '$lib/control/dispatch';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { transcript, type TranscriptSegment } from '$lib/transcribe/store.svelte';
  import { DEFAULT_HAM_PROMPT } from '$lib/transcribe/hamPrompt';
  import Split from '$lib/layout/Split.svelte';

  // whisper initial_prompt — the single biggest accuracy lever for ham
  // voice. Stored empty = "use the built-in dense corpus"; the textarea
  // always shows the *effective* text so it's editable from the
  // default. Writes fan out live to the worker via applyControl.
  let storedPrompt = $derived(clientControls.get('client.transcribe.prompt') as string);
  let promptText = $derived(storedPrompt || DEFAULT_HAM_PROMPT);
  let promptDirty = $derived(storedPrompt.length > 0);
  function setPrompt(text: string): void {
    // Treat "same as default" as empty so we don't persist the whole
    // corpus into localStorage when the user hasn't really changed it.
    void applyControl(
      'client.transcribe.prompt',
      text.trim() === DEFAULT_HAM_PROMPT.trim() ? '' : text,
    );
  }
  function resetPrompt(): void {
    void applyControl('client.transcribe.prompt', '');
  }

  function fmtTime(ms: number): string {
    return new Date(ms).toLocaleTimeString([], { hour12: false });
  }
  function fmtFreq(hz: number | null): string {
    if (hz === null) return '—';
    return `${(hz / 1e6).toFixed(4)} MHz`;
  }
  /** Dim tokens whose model probability is shaky so the eye lands on
   *  the confident words. Threshold is intentionally generous. */
  function tokClass(p: number): string {
    if (p >= 0.7) return 'text-slate-200';
    if (p >= 0.45) return 'text-slate-400';
    return 'text-amber-500/70 underline decoration-dotted';
  }

  const statusTone: Record<string, string> = {
    listening: 'text-emerald-400',
    transcribing: 'text-sky-400',
    'loading-model': 'text-amber-400',
    unavailable: 'text-amber-500',
    error: 'text-red-400',
    idle: 'text-slate-500',
  };

  // Map a linear RMS amplitude to a 0..1 meter fraction over a
  // -60..0 dBFS window — same visual scale as the audio level meter.
  function dbFrac(x: number): number {
    if (!(x > 0)) return 0;
    const db = 20 * Math.log10(x);
    return Math.max(0, Math.min(1, (db + 60) / 60));
  }
  let armed = $derived(transcript.status === 'listening' || transcript.status === 'transcribing');
  let lvlFrac = $derived(dbFrac(transcript.level));
  let thrFrac = $derived(dbFrac(transcript.threshold));

  function exportText(): string {
    return transcript.segments
      .map((s: TranscriptSegment) => `[${fmtTime(s.atMs)}] ${fmtFreq(s.vfoHz)}  ${s.text}`)
      .join('\n');
  }
  function copyAll(): void {
    void navigator.clipboard?.writeText(exportText());
  }
  function download(): void {
    const blob = new Blob([exportText()], { type: 'text/plain' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = `ferrite-transcript-${Date.now()}.txt`;
    a.click();
    URL.revokeObjectURL(a.href);
  }

  // Auto-scroll to the newest segment unless the user scrolled up.
  let listEl: HTMLDivElement | undefined = $state();
  let pinned = $state(true);
  $effect(() => {
    // Reference `.length` so the effect re-runs when segments change.
    const n = transcript.segments.length;
    if (pinned && listEl && n >= 0) listEl.scrollTop = listEl.scrollHeight;
  });
  function onScroll(): void {
    if (!listEl) return;
    pinned = listEl.scrollHeight - listEl.scrollTop - listEl.clientHeight < 40;
  }

  // Right pane: Tier-0 rolling transcript — segments joined into
  // flowing prose. A paragraph break only when the silence before an
  // utterance was *sufficient* (≥ PARA_SILENCE_MS) — a real over/turn,
  // not every short sentence pause or an arbitrary 10 s max-cut.
  // Whisper supplies the punctuation/casing; no model.
  const PARA_SILENCE_MS = 1500;
  let cleaned = $derived.by(() => {
    const segs = transcript.segments;
    let out = '';
    for (let i = 0; i < segs.length; i++) {
      if (i > 0) out += segs[i].gapMs >= PARA_SILENCE_MS ? '\n\n' : ' ';
      out += segs[i].text;
    }
    return out;
  });
  let cleanEl: HTMLDivElement | undefined = $state();
  let cleanPinned = $state(true);
  $effect(() => {
    const n = transcript.segments.length;
    if (cleanPinned && cleanEl && n >= 0) cleanEl.scrollTop = cleanEl.scrollHeight;
  });
  function onCleanScroll(): void {
    if (!cleanEl) return;
    cleanPinned = cleanEl.scrollHeight - cleanEl.scrollTop - cleanEl.clientHeight < 40;
  }
</script>

<div class="flex h-full w-full min-h-0 flex-col bg-[color:var(--color-bg)]">
  <header class="panel-head">
    <span class="flex items-baseline gap-2">
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">transcript</span>
      <span
        class="font-normal normal-case tracking-normal {statusTone[transcript.status] ??
          'text-[color:var(--color-muted)]'}"
      >
        ● {transcript.status}
      </span>
      {#if transcript.modelName}
        <span class="font-normal normal-case tracking-normal text-[10px] text-slate-500"
          >{transcript.modelName}</span
        >
      {/if}
      {#if transcript.droppedSamples > 0}
        <span class="font-normal normal-case tracking-normal text-[10px] text-amber-500/70"
          >dropped {transcript.droppedSamples}</span
        >
      {/if}
    </span>
    <span class="flex gap-2">
      <button
        type="button"
        class="rounded border border-slate-700 px-2 py-0.5 text-[11px] font-normal normal-case tracking-normal text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
        onclick={copyAll}
        disabled={transcript.segments.length === 0}>Copy</button
      >
      <button
        type="button"
        class="rounded border border-slate-700 px-2 py-0.5 text-[11px] font-normal normal-case tracking-normal text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
        onclick={download}
        disabled={transcript.segments.length === 0}>Export</button
      >
      <button
        type="button"
        class="rounded border border-slate-700 px-2 py-0.5 text-[11px] font-normal normal-case tracking-normal text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
        onclick={() => transcript.clear()}
        disabled={transcript.segments.length === 0}>Clear</button
      >
    </span>
  </header>

  <!-- Live behaviour strip: VAD gate/level meter + backlog. Only while
       armed, so it never shows stale values when idle/unavailable. -->
  {#if armed}
    <div
      class="flex items-center gap-2 border-b border-slate-800 px-2 py-1 font-mono text-[10px] text-slate-500"
    >
      <span class="text-[color:var(--color-muted)]">gate</span>
      <div
        class="relative h-1.5 w-24 overflow-hidden rounded-sm border border-slate-800 bg-slate-950"
        title="Input level vs the adaptive VAD threshold. Bar lights when the gate is open (capturing speech)."
      >
        <div
          class="absolute inset-y-0 left-0 transition-[width] duration-100"
          class:bg-emerald-500={transcript.gateOpen}
          class:bg-slate-600={!transcript.gateOpen}
          style:width="{lvlFrac * 100}%"
        ></div>
        <div class="absolute inset-y-0 w-px bg-slate-400" style:left="{thrFrac * 100}%"></div>
      </div>
      <span class={transcript.gateOpen ? 'text-emerald-400' : 'text-slate-600'}>
        {transcript.gateOpen ? 'capturing' : 'listening'}
      </span>
      <span class="ml-auto">
        {#if transcript.queued > 0 || transcript.lagMs > 800}
          <span class={transcript.lagMs > 4000 ? 'text-amber-500' : 'text-slate-500'}>
            queue {transcript.queued} · ~{(transcript.lagMs / 1000).toFixed(1)}s behind
          </span>
        {:else}
          <span class="text-slate-600">live</span>
        {/if}
      </span>
    </div>
  {/if}

  <!-- whisper initial_prompt — vocabulary bias (collapsible). -->
  <details class="border-b border-slate-800">
    <summary
      class="cursor-pointer select-none px-2 py-1 text-[10px] uppercase tracking-wider text-slate-500 hover:text-slate-300"
    >
      Vocabulary prompt {promptDirty ? '(edited)' : '(default ham corpus)'}
    </summary>
    <div class="flex flex-col gap-1 p-2">
      <p class="text-[10px] text-slate-600">
        Seeds whisper toward ham lingo — callsigns, Q-codes, RST, on-air phrasing. Biggest single
        accuracy lever. Recently-heard callsigns are appended automatically. Applies live.
      </p>
      <textarea
        class="h-40 w-full resize-y rounded border border-slate-800 bg-slate-950 p-2 font-mono text-[11px] text-slate-300"
        spellcheck="false"
        value={promptText}
        oninput={(e) => setPrompt((e.currentTarget as HTMLTextAreaElement).value)}
      ></textarea>
      <div class="flex items-center justify-between">
        <span class="text-[10px] text-slate-600">{promptText.length} chars</span>
        <button
          type="button"
          class="rounded border border-slate-700 px-2 py-0.5 text-[11px] text-slate-400 hover:border-slate-500 disabled:opacity-40"
          onclick={resetPrompt}
          disabled={!promptDirty}>reset to default</button
        >
      </div>
    </div>
  </details>

  {#if transcript.statusDetail}
    <p class="border-b border-slate-800 px-2 py-1 text-[10px] text-slate-500">
      {transcript.statusDetail}
    </p>
  {/if}

  <!-- Split body: left = accurate timestamped/probability log;
       right = Tier-0 rolling cleaned transcript. -->
  <div class="min-h-0 flex-1">
    <Split direction="row" defaultFraction={0.5} storageKey="ferrite.split.transcript-cols">
      {#snippet a()}
        <section class="flex h-full min-h-0 flex-col">
          <header
            class="border-b border-slate-800 px-2 py-1 text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
          >
            log · time · freq · confidence
          </header>
          <div
            bind:this={listEl}
            onscroll={onScroll}
            class="min-h-0 flex-1 overflow-y-auto p-2 text-xs leading-relaxed"
          >
            {#if transcript.segments.length === 0}
              <p class="text-slate-600">
                Nothing yet — set the receiver's Audio control to
                <span class="text-slate-400">transcribe</span> and tune a voice signal.
              </p>
            {:else}
              {#each transcript.segments as seg (seg.id)}
                <div class="border-b border-slate-900/60 py-1">
                  <div class="flex gap-2 font-mono text-[10px] text-slate-600">
                    <span>{fmtTime(seg.atMs)}</span>
                    <span>{fmtFreq(seg.vfoHz)}</span>
                    <span class="ml-auto">{Math.round(seg.confidence * 100)}%</span>
                  </div>
                  <div>
                    {#if seg.tokens.length > 0}
                      {#each seg.tokens as tk, i (i)}<span class={tokClass(tk.p)}>{tk.text}</span
                        >{/each}
                    {:else}
                      <span class="text-slate-300">{seg.text}</span>
                    {/if}
                  </div>
                </div>
              {/each}
            {/if}
          </div>
        </section>
      {/snippet}
      {#snippet b()}
        <section class="flex h-full min-h-0 flex-col border-l border-slate-800">
          <header
            class="border-b border-slate-800 px-2 py-1 text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
          >
            transcript · rolling
          </header>
          <div
            bind:this={cleanEl}
            onscroll={onCleanScroll}
            class="min-h-0 flex-1 overflow-y-auto whitespace-pre-wrap p-2 text-xs leading-relaxed text-slate-300"
          >
            {#if cleaned}
              {cleaned}
            {:else}
              <span class="text-slate-600">cleaned transcript appears here</span>
            {/if}
          </div>
        </section>
      {/snippet}
    </Split>
  </div>
</div>

<style>
  .panel-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 3px 8px;
    border-bottom: 1px solid rgb(30 41 59);
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--color-muted);
  }
</style>
