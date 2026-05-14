<script lang="ts">
  // Two compact toggles next to the active preset name:
  //   • Audio   — gates blocks marked `when: { audio: true }`.
  //               Hidden when the preset declares no audio chain.
  //   • Demod   — flips placement of the block tagged
  //               `placement_role: "demod"` between node and browser.
  //               Hidden when the preset has no such block.
  //
  // The chips read the active doc from `pipeline.flowgraph`, so they
  // self-hide on presets that don't declare the corresponding tags
  // (e.g. ADS-B/AIS show neither — those modes have no audio path
  // and no togglable demod). Clicking either chip PATCHes the server's
  // profile and re-composes the live pipeline.
  import { pipeline } from '$lib/pipeline.svelte';

  // Derived availability: which axes does the active preset support?
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

  // Show the row only when at least one chip would render — otherwise
  // even the row's spacing is dead pixels on a no-toggle preset.
  let visible = $derived(hasAudioToggle || hasDemodToggle);

  function toggleAudio(): void {
    void pipeline.setProfile({
      ...pipeline.profile,
      audio: !pipeline.profile.audio,
    });
  }

  function pickDemod(side: 'node' | 'browser' | null): void {
    void pipeline.setProfile({
      ...pipeline.profile,
      demod_placement: side,
    });
  }

  // "auto" means the preset's authored placement wins. Surfaced
  // explicitly so the user can see when they're overriding and reset
  // back to "follow preset author".
  let demodValue = $derived(pipeline.profile.demod_placement ?? 'auto');
</script>

{#if visible}
  <div class="flex flex-wrap items-center gap-2 text-[10px]">
    {#if hasAudioToggle}
      <button
        type="button"
        class="chip"
        class:chip-on={pipeline.profile.audio}
        class:chip-off={!pipeline.profile.audio}
        onclick={toggleAudio}
        title="Toggle the audio chain. When off, the audio_resamp / audio_nr / audio blocks (and their WS bridge) are pruned before the pipeline starts."
      >
        <span class="chip-label">Audio</span>
        <span class="chip-value">{pipeline.profile.audio ? 'on' : 'off'}</span>
      </button>
    {/if}

    {#if hasDemodToggle}
      <div class="chip-group" role="group" aria-label="Demod placement">
        <span class="chip-label pl-1">Demod</span>
        {#each ['auto', 'node', 'browser'] as const as opt}
          <button
            type="button"
            class="chip-segment"
            class:chip-segment-active={demodValue === opt}
            onclick={() => pickDemod(opt === 'auto' ? null : opt)}
            title={opt === 'auto'
              ? 'Follow the preset author (default).'
              : opt === 'node'
                ? 'Run the demod server-side; stream real audio across the WS.'
                : 'Stream IQ across the WS and demod in the browser.'}
          >
            {opt}
          </button>
        {/each}
      </div>
    {/if}
  </div>
{/if}

<style>
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.15rem 0.5rem;
    border: 1px solid rgb(51 65 85);
    border-radius: 9999px;
    background: rgb(15 23 42);
    color: rgb(148 163 184);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    cursor: pointer;
    transition:
      background 0.1s,
      color 0.1s,
      border-color 0.1s;
  }
  .chip:hover {
    background: rgb(30 41 59);
    color: rgb(226 232 240);
  }
  .chip-on {
    border-color: rgb(56 189 248);
    color: rgb(125 211 252);
  }
  .chip-off {
    border-color: rgb(71 85 105);
    color: rgb(100 116 139);
  }
  .chip-label {
    font-weight: 600;
    color: rgb(148 163 184);
  }
  .chip-value {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  .chip-group {
    display: inline-flex;
    align-items: center;
    border: 1px solid rgb(51 65 85);
    border-radius: 9999px;
    background: rgb(15 23 42);
    overflow: hidden;
  }
  .chip-segment {
    padding: 0.15rem 0.5rem;
    color: rgb(148 163 184);
    font-size: 10px;
    text-transform: lowercase;
    cursor: pointer;
    transition:
      background 0.1s,
      color 0.1s;
  }
  .chip-segment:hover {
    background: rgb(30 41 59);
    color: rgb(226 232 240);
  }
  .chip-segment-active {
    background: rgb(30 58 138);
    color: rgb(191 219 254);
  }
</style>
