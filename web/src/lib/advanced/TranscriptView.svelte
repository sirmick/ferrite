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

  <!-- Segment log — chronological, auto-pinned to newest. -->
  <div
    bind:this={listEl}
    onscroll={onScroll}
    class="min-h-0 flex-1 overflow-y-auto p-2 text-xs leading-relaxed"
  >
    {#if transcript.segments.length === 0}
      <p class="text-slate-600">
        Nothing yet — set the receiver's Audio control to
        <span class="text-slate-400">transcribe</span> and tune a voice signal. Segments appear here when
        speech is detected.
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
              {#each seg.tokens as tk, i (i)}<span class={tokClass(tk.p)}>{tk.text}</span>{/each}
            {:else}
              <span class="text-slate-300">{seg.text}</span>
            {/if}
          </div>
          {#if seg.tokens.length > 0 && seg.text}
            <div class="text-[10px] text-slate-500">↳ {seg.text}</div>
          {/if}
        </div>
      {/each}
    {/if}
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
