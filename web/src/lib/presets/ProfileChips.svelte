<script lang="ts">
  // Compact segmented chip-groups next to the active preset name:
  //   • Audio  — off | on | transcribe  (build-time server profile)
  //   • NR     — off | voice | ssb | am | fm  (live audio_nr preset)
  //   • Run    — browser | balanced | server  (where the audio spine is
  //              cut between daemon and browser — one ordered setting)
  //
  // Audio/Run self-hide on presets without an audio chain. Audio + Run
  // PATCH the server profile + re-compose; NR is a client knob pushed
  // live to the browser `audio_nr` block.
  import type { AudioSplit } from '$lib/api/profile';
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

  // The "Run" split is meaningful whenever the preset has an audio
  // spine — any block carrying one of the spine roles.
  let hasSplit = $derived.by(() => {
    const blocks = pipeline.flowgraph?.blocks ?? {};
    for (const b of Object.values(blocks)) {
      const r = b.placement_role;
      if (r === 'demod' || r === 'audio' || r === 'nr') return true;
    }
    return false;
  });

  let visible = $derived(hasAudioToggle || hasSplit);

  // Off | On | Transcribe — one tri-state over two profile bits.
  // `transcribe` implies `audio` (the tap sits on the audio chain), so
  // we never set transcribe without audio.
  function pickAudioMode(mode: 'off' | 'on' | 'transcribe'): void {
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

  // The single audio-split selector. `server` is the headless path:
  // demod → resample → NR → transcribe all run in `ferrited`, so a
  // VoiceTranscribe tap reads node-side audio and whisper runs with no
  // browser. `browser` pushes the whole spine into the tab; `balanced`
  // (default) leaves each block where the preset authored it.
  function pickSplit(split: AudioSplit): void {
    void pipeline.setProfile({ ...pipeline.profile, audio_split: split });
  }
  let splitValue = $derived(pipeline.profile.audio_split);
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

    {#if hasSplit}
      <div class="chip-group" role="group" aria-label="Audio split">
        <span class="chip-label">Run</span>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={splitValue === 'browser'}
          onclick={() => pickSplit('browser')}
          title="Thin server: stream IQ across the WS and run the whole audio chain (demod → NR → transcribe) in the browser."
        >
          browser
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={splitValue === 'balanced'}
          onclick={() => pickSplit('balanced')}
          title="Default: each block runs where the preset authored it — typically demod on the server, audio render + NR in the browser."
        >
          balanced
        </button>
        <button
          type="button"
          class="chip-segment"
          class:chip-segment-active={splitValue === 'server'}
          onclick={() => pickSplit('server')}
          title="Headless: run the whole audio chain on the server (ferrited). Transcription runs there too — no browser needed; transcript on the decoder feed + Transcript tab."
        >
          server
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
