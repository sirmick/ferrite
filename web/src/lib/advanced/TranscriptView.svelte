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
  import { transcript, segmentLine } from '$lib/transcribe/store.svelte';
  import { decoders } from '$lib/decoders/store.svelte';
  import { DEFAULT_HAM_PROMPT } from '$lib/transcribe/hamPrompt';
  // Whisper model + prompt are now block params on `VoiceTranscribe`
  // (see `blocks/src/voice_transcribe.rs`); the SettingsPanel's
  // Decoder section is the canonical UI. This view focuses on output.
  import TranscriptFlow from '$lib/transcript/TranscriptFlow.svelte';
  import TranscriptLegend from '$lib/transcript/TranscriptLegend.svelte';
  import { buildFlow, flowToText } from '$lib/transcript/flow';

  // Server-side transcription (sherpa, node-whisper) flows through the
  // unified decoder store: each segment is a record under kind
  // `transcribe`, mirrored to every browser over the event bus. Drive the
  // transcript store from that mirror (deduped by the store's monotonic
  // `seq`). Browser-placed whisper feeds the same `transcript` store via
  // the Worker's `push`, so the panel renders both regardless of which
  // side ran. Referencing `.recent.length` keeps this effect reactive to
  // new records.
  $effect(() => {
    const recent = decoders.kind('transcribe').recent;
    if (recent.length > 0) transcript.ingestRecords(recent);
  });

  // Raw / clean toggle for the per-segment log. Clean (ham-post-
  // processed) by default; raw shows whisper's original output — only
  // meaningful when an entry carries it (node path always does; the
  // browser worker currently omits rawText).
  let showRaw = $state(false);
  // Per-line time · frequency · confidence (what the old left "log"
  // pane showed), now a density toggle on the one transcript.
  let details = $state(false);
  // Speaker colouring: by radio (default) or by talkgroup; the legend
  // owns both, and a clicked chip highlights that speaker.
  let colourBy = $state<'speaker' | 'group'>('speaker');
  let focusKey = $state<string | undefined>(undefined);
  let lines = $derived(transcript.segments.map(segmentLine));

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
    return flowToText(buildFlow(lines), fmtTime);
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
        class="rounded border border-slate-700 px-2 py-0.5 text-[11px] font-normal normal-case tracking-normal hover:border-slate-500 hover:text-slate-100"
        class:text-sky-400={showRaw}
        class:text-slate-300={!showRaw}
        onclick={() => (showRaw = !showRaw)}
        title="Toggle raw recogniser output vs ham-cleaned text">{showRaw ? 'Raw' : 'Clean'}</button
      >
      <button
        type="button"
        class="rounded border border-slate-700 px-2 py-0.5 text-[11px] font-normal normal-case tracking-normal hover:border-slate-500 hover:text-slate-100"
        class:text-sky-400={details}
        class:text-slate-300={!details}
        onclick={() => (details = !details)}
        title="Show time · frequency · confidence on every line">Details</button
      >
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

  <TranscriptLegend {lines} bind:colourBy bind:focusKey />

  <div class="min-h-0 flex-1">
    <TranscriptFlow
      {lines}
      {colourBy}
      {details}
      {showRaw}
      {focusKey}
      placeholder="Nothing yet — set the receiver's Audio control to transcribe and tune a voice signal."
    />
  </div>
</div>

<style></style>
