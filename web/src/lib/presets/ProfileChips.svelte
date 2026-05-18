<script lang="ts">
  // Two compact segmented chip-groups next to the active preset name:
  //   • Audio  — off | on | transcribe
  //              (off strips the audio chain; on keeps it; transcribe
  //               additionally splices a VoiceTranscribe tap — all
  //               build-time, server profile, re-composed live)
  //   • Demod  — auto | server | browser
  //              (auto = follow the preset author's placement;
  //               server/browser override it. "server" maps to the
  //               `node` API value — kept terse for the UI.)
  //
  // Each chip self-hides on presets that don't declare the
  // corresponding tag (`when: { audio: ... }` for audio,
  // `placement_role: "demod"` for demod), so ADS-B/AIS show neither
  // and digital-with-decoder presets only show audio. Clicking a
  // segment PATCHes the server's profile and re-composes live.
  import { pipeline } from '$lib/pipeline.svelte';

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

  let visible = $derived(hasAudioToggle || hasDemodToggle);

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

  function pickDemod(side: 'node' | 'browser' | null): void {
    void pipeline.setProfile({ ...pipeline.profile, demod_placement: side });
  }

  // Reads `pipeline.profile.demod_placement` (`'node' | 'browser' | null`)
  // and translates to the UI's tri-state. `null` = "auto" (preset
  // author's authored placement wins). The API-side `node` is shown
  // as "server" in the UI — same value, less jargon.
  let demodValue = $derived(pipeline.profile.demod_placement ?? 'auto');
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
