<script lang="ts">
  // Advanced transcript panel.
  //
  // Renders the VAD-gated speech-to-text segments the transcription
  // Worker produces, stamped with wall-clock + VFO frequency so it
  // doubles as a band log ("set it and leave it"). Low-confidence
  // tokens are dimmed so the operator's eye does the disambiguation —
  // the right UX for noisy SSB review. Also hosts the tri-state engage
  // control for the VoiceTranscribe block.
  //
  // Pure view: all inference is off in the Worker; this only reads the
  // reactive `transcript` store and dispatches the mode control.

  import { pipeline } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { transcript, type TranscriptSegment } from '$lib/transcribe/store.svelte';
  import { DEFAULT_HAM_PROMPT } from '$lib/transcribe/hamPrompt';

  // The VoiceTranscribe block is browser-only (env_split strips it
  // node-side) so it's not in the server pipeline mirror — find it in
  // the composed flowgraph doc instead.
  let vt = $derived.by(() => {
    const blocks = pipeline.flowgraph?.blocks ?? {};
    for (const [id, decl] of Object.entries(blocks)) {
      if (decl.type === 'VoiceTranscribe') return { id, decl };
    }
    return null;
  });
  let mode = $derived((vt?.decl.params?.mode as string | undefined) ?? 'off');

  const MODES: { v: string; label: string; hint: string }[] = [
    { v: 'off', label: 'Off', hint: 'passthrough only — no transcription (zero cost)' },
    { v: 'on', label: 'On', hint: 'audio plays and is transcribed' },
    { v: 'muted', label: 'Transcript', hint: 'transcribed only, audio silenced' },
  ];

  function setMode(v: string): void {
    if (!vt) return;
    void applyControl(`browser.${vt.id}.mode`, v);
  }

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
    const d = new Date(ms);
    return d.toLocaleTimeString([], { hour12: false });
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

<div class="flex h-full flex-col gap-2 p-2 text-xs">
  {#if !vt}
    <p class="text-[11px] text-slate-500">
      No <code>VoiceTranscribe</code> block in the current preset. Insert one after the audio
      noise-reduction block (<code>… → AudioNr → VoiceTranscribe → AudioSink</code>) to enable
      in-browser speech-to-text.
    </p>
  {:else}
    <!-- Engage control — tri-state, same shape as the audio toggle. -->
    <div class="flex items-center gap-1" role="group" aria-label="Transcription mode">
      {#each MODES as m (m.v)}
        <button
          type="button"
          title={m.hint}
          class="flex-1 rounded border px-2 py-1 text-[11px] font-bold uppercase tracking-wider"
          class:border-emerald-600={mode === m.v}
          class:text-emerald-300={mode === m.v}
          class:border-slate-800={mode !== m.v}
          class:text-slate-500={mode !== m.v}
          onclick={() => setMode(m.v)}>{m.label}</button
        >
      {/each}
    </div>

    <!-- whisper initial_prompt — vocabulary bias (collapsible). -->
    <details class="rounded border border-slate-800">
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
          <span class="text-[9px] text-slate-600">{promptText.length} chars</span>
          <button
            type="button"
            class="rounded border border-slate-700 px-2 py-0.5 text-[10px] text-slate-400 hover:border-slate-500 disabled:opacity-40"
            onclick={resetPrompt}
            disabled={!promptDirty}>reset to default</button
          >
        </div>
      </div>
    </details>

    <!-- Status header. -->
    <div class="flex flex-wrap items-baseline gap-x-3 gap-y-1 border-b border-slate-800 pb-1">
      <span class={statusTone[transcript.status] ?? 'text-slate-400'}>
        ● {transcript.status}
      </span>
      {#if transcript.modelName}
        <span class="text-[10px] text-slate-500">{transcript.modelName}</span>
      {/if}
      {#if transcript.droppedSamples > 0}
        <span class="text-[10px] text-amber-500/70">dropped {transcript.droppedSamples}</span>
      {/if}
      <span class="ml-auto flex gap-2">
        <button
          type="button"
          class="text-[10px] text-slate-500 hover:text-slate-300"
          onclick={copyAll}
          disabled={transcript.segments.length === 0}>copy</button
        >
        <button
          type="button"
          class="text-[10px] text-slate-500 hover:text-slate-300"
          onclick={download}
          disabled={transcript.segments.length === 0}>export</button
        >
        <button
          type="button"
          class="text-[10px] text-slate-500 hover:text-slate-300"
          onclick={() => transcript.clear()}
          disabled={transcript.segments.length === 0}>clear</button
        >
      </span>
    </div>

    {#if transcript.statusDetail}
      <p class="text-[10px] text-slate-500">{transcript.statusDetail}</p>
    {/if}

    <!-- Segment log — chronological, auto-pinned to newest. -->
    <div
      bind:this={listEl}
      onscroll={onScroll}
      class="min-h-0 flex-1 overflow-y-auto font-mono text-[11px] leading-relaxed"
    >
      {#if transcript.segments.length === 0}
        <p class="text-slate-600">
          {mode === 'off'
            ? 'Set the mode to On or Transcript to start listening.'
            : 'Listening… segments appear here when speech is detected.'}
        </p>
      {:else}
        {#each transcript.segments as seg (seg.id)}
          <div class="border-b border-slate-900/60 py-1">
            <div class="flex gap-2 text-[9px] text-slate-600">
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
              <div class="text-[9px] text-slate-500">↳ {seg.text}</div>
            {/if}
          </div>
        {/each}
      {/if}
    </div>
  {/if}
</div>
