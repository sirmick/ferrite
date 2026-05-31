<script lang="ts">
  // Compact segmented chip-groups next to the active preset name:
  //   • Audio  — off | on | transcribe  (build-time server profile)
  //   • NR     — auto | off | voice | ssb | am | fm  (live audio_nr
  //              preset; "transcribe" auto-selects voice)
  //   • Demod  — auto | server | browser
  //
  // Audio/Demod self-hide on presets that don't declare the matching
  // tag (`when: { audio }`, `placement_role: "demod"`); NR rides with
  // the audio chain. Audio/Demod PATCH the server profile + re-compose;
  // NR is a client knob pushed live to the browser `audio_nr` block.
  import { pipeline } from '$lib/pipeline.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { NR_PRESETS } from '$lib/presets/nrPresets';

  let hasAudioToggle = $derived.by(() => {
    const blocks = pipeline.flowgraph?.blocks ?? {};
    for (const b of Object.values(blocks)) {
      if (b.when && 'audio' in b.when) return true;
    }
    return false;
  });

  let hasDemodToggle = $derived.by(() => {
    const blocks = pipeline.flowgraph?.blocks ?? {};
    for (const b of Object.values(blocks)) {
      if (b.placement_role === 'demod') return true;
    }
    return false;
  });

  let hasNrToggle = $derived.by(() => {
    const blocks = pipeline.flowgraph?.blocks ?? {};
    for (const b of Object.values(blocks)) {
      if (b.placement_role === 'nr') return true;
    }
    return false;
  });

  // The VoiceTranscribe tap is injected server-side only when
  // transcription is engaged, so its placement chip rides the
  // `transcribe` profile bit rather than a preset block tag.
  let hasTranscribeToggle = $derived(pipeline.profile.transcribe);

  let visible = $derived(hasAudioToggle || hasDemodToggle || hasNrToggle || hasTranscribeToggle);

  // Off | On | Transcribe — one tri-state over two profile bits.
  // `transcribe` implies `audio` (the tap sits on the audio chain), so
  // we never set transcribe without audio. Selecting transcribe also
  // switches NR to `voice` (best signal for whisper); the post-load
  // re-apply in browserRuntime makes it stick across the re-compose.
  function pickAudioMode(mode: 'off' | 'on' | 'transcribe'): void {
    // Transcribe ⇒ voice-NR coupling lives in +page.svelte's profile
    // effect now, so the CLI (`ferrite-ctl transcribe on`) converges
    // through the same path. Just flip the profile axes here.
    void pipeline.setProfile({
      ...pipeline.profile,
      audio: mode !== 'off',
      transcribe: mode === 'transcribe',
    });
  }

  let nrPreset = $derived(clientControls.get('client.audio.nrPreset'));
  function pickNr(id: string): void {
    // dispatch fans out to the live audio_nr block (see dispatch.ts).
    void applyControl('client.audio.nrPreset', id);
  }

  function pickDemod(side: 'node' | 'browser' | null): void {
    void pipeline.setProfile({ ...pipeline.profile, demod_placement: side });
  }

  function pickNrPlacement(side: 'node' | 'browser' | null): void {
    void pipeline.setProfile({ ...pipeline.profile, nr_placement: side });
  }

  function pickTranscribePlacement(side: 'node' | 'browser' | null): void {
    void pipeline.setProfile({ ...pipeline.profile, transcribe_placement: side });
  }

  // Reads each placement override (`'node' | 'browser' | null`) and
  // translates to the UI's tri-state. `null` = "auto" (preset author's
  // authored placement wins). The API-side `node` is shown as "server"
  // in the UI — same value, less jargon.
  let demodValue = $derived(pipeline.profile.demod_placement ?? 'auto');
  let nrValue = $derived(pipeline.profile.nr_placement ?? 'auto');
  let transcribeValue = $derived(pipeline.profile.transcribe_placement ?? 'auto');
</script>

{#if visible}
  <div class="flex flex-wrap items-center gap-2 text-[10px]">
    {#if hasAudioToggle}
      <div class="chip-group" role="group" aria-label="Audio">
        <span class="chip-label">Audio</span>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={!pipeline.profile.audio}
          onclick={() => pickAudioMode('off')}
          title="Strip the audio chain — saves WS bandwidth on decoder-only listening."
        >
          off
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={pipeline.profile.audio && !pipeline.profile.transcribe}
          onclick={() => pickAudioMode('on')}
          title="Enable the audio chain (default)."
        >
          on
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={pipeline.profile.audio && pipeline.profile.transcribe}
          onclick={() => pickAudioMode('transcribe')}
          title="Enable audio and splice a VoiceTranscribe tap — in-browser speech-to-text (Transcript advanced view)."
        >
          transcribe
        </button>
      </div>
    {/if}

    {#if hasAudioToggle}
      <div class="chip-group" role="group" aria-label="Noise reduction">
        <span class="chip-label">NR</span>
        {#each NR_PRESETS as p (p.id)}
          <button
            type="button"
            class="chip-segment"
            class:chip-segment-active={nrPreset === p.id}
            onclick={() => pickNr(p.id)}
            title={p.title}
          >
            {p.label}
          </button>
        {/each}
      </div>
    {/if}

    {#if hasDemodToggle}
      <div class="chip-group" role="group" aria-label="Demod placement">
        <span class="chip-label">Demod</span>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={demodValue === 'auto'}
          onclick={() => pickDemod(null)}
          title="Follow the preset author's placement (default)."
        >
          auto
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={demodValue === 'node'}
          onclick={() => pickDemod('node')}
          title="Run the demod on the server; stream real audio across the WS."
        >
          server
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={demodValue === 'browser'}
          onclick={() => pickDemod('browser')}
          title="Stream IQ across the WS and demod in the browser."
        >
          browser
        </button>
      </div>
    {/if}

    {#if hasNrToggle}
      <div class="chip-group" role="group" aria-label="Noise-reduction placement">
        <span class="chip-label">NR run</span>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={nrValue === 'auto'}
          onclick={() => pickNrPlacement(null)}
          title="Follow the preset author's placement (default)."
        >
          auto
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={nrValue === 'node'}
          onclick={() => pickNrPlacement('node')}
          title="Run noise reduction on the server (cleans audio for node-side consumers, e.g. headless transcribe)."
        >
          server
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={nrValue === 'browser'}
          onclick={() => pickNrPlacement('browser')}
          title="Run noise reduction in the browser (keeps the server lighter)."
        >
          browser
        </button>
      </div>
    {/if}

    {#if hasTranscribeToggle}
      <div class="chip-group" role="group" aria-label="Transcription placement">
        <span class="chip-label">STT run</span>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={transcribeValue === 'auto'}
          onclick={() => pickTranscribePlacement(null)}
          title="Default placement (in-browser whisper)."
        >
          auto
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={transcribeValue === 'node'}
          onclick={() => pickTranscribePlacement('node')}
          title="Headless: whisper runs on the server (ferrited), no browser needed. Transcript on the decoder feed."
        >
          server
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={transcribeValue === 'browser'}
          onclick={() => pickTranscribePlacement('browser')}
          title="Run whisper in the browser (the Transcript advanced view)."
        >
          browser
        </button>
      </div>
    {/if}
  </div>
{/if}

<style>
  /* Palette matches the leftTab tabs in `routes/+page.svelte`:
     muted text + slate-800 borders + an `--color-accent` (#7dd3fc)
     tint for the active segment, same `rgba(125,211,252,0.08)` bg
     used by `tab-active`. */
  .chip-group {
    display: inline-flex;
    align-items: stretch;
    border: 1px solid rgb(30 41 59); /* slate-800 */
    border-radius: 9999px;
    background: transparent;
    overflow: hidden;
    line-height: 1;
  }
  .chip-label,
  .chip-segment {
    /* Flex-center every cell so the label baseline lines up with the
       segment text regardless of font metrics. line-height: 1 on the
       group kills the descender drift that pushed labels slightly low. */
    display: inline-flex;
    align-items: center;
    height: 1.25rem;
    font-size: 10px;
    line-height: 1;
  }
  .chip-label {
    padding: 0 0.6rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--color-muted);
    border-right: 1px solid rgb(30 41 59);
  }
  .chip-segment {
    padding: 0 0.55rem;
    color: var(--color-muted);
    text-transform: lowercase;
    cursor: pointer;
    border: 0;
    background: transparent;
    transition:
      background 0.1s,
      color 0.1s;
  }
  .chip-segment:hover {
    color: var(--color-fg);
    background: rgba(125, 211, 252, 0.04);
  }
  .chip-segment-active {
    color: var(--color-accent);
    background: rgba(125, 211, 252, 0.08);
  }
</style>
