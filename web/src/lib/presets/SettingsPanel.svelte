<script lang="ts">
  import { pipeline } from '$lib/pipeline.svelte';
  import BlockParams from '$lib/controls/BlockParams.svelte';
  import InputControls from '$lib/controls/InputControls.svelte';
  import AudioPanel from './AudioPanel.svelte';
  import ProfileChips from './ProfileChips.svelte';
  import RssiMeter from '$lib/rssi/RssiMeter.svelte';
  import RdsReadout from '$lib/rds/RdsReadout.svelte';

  // Container for everything the user reaches for after the source is
  // open. Order from top to bottom: Receiver (the live demod knobs you
  // tweak per-station), Audio (volume + meters), then Input (SDR-side
  // settings — typically configured once and forgotten). The Input
  // section starts collapsed so the surface stays focused on the
  // controls actively used while listening.
  //
  // Switching demodulator (FM ↔ AM ↔ SSB) used to be a Mode dropdown
  // here; that's now a Catalog-tab choice — picking a different preset
  // loads the matching flowgraph in one click and is the single source
  // of truth for "what mode am I in".
  const DEMOD_ID = 'demod';
  // Noise-reduction section looks up the single audio_nr block that
  // ships in every analog-mode preset (AudioNrMono or AudioNrStereo,
  // same id so the UI binds once). If a preset doesn't have an NR
  // block (e.g. digital-mode / recording flowgraphs) the section
  // quietly hides itself.
  const AUDIO_NR_ID = 'audio_nr';

  let demodBlock = $derived(pipeline.blocks[DEMOD_ID]);
  let audioNrBlock = $derived(pipeline.blocks[AUDIO_NR_ID]);
  let presetLabel = $derived(pipeline.flowgraph?.label ?? pipeline.flowgraph?.name ?? '—');
</script>

<div
  class="flex h-full w-full flex-col border-r border-slate-800 bg-[color:var(--color-bg)] text-xs"
>
  <div class="flex items-center justify-between border-b border-slate-800 px-2 py-1">
    <span class="font-semibold text-[color:var(--color-muted)]">Settings</span>
  </div>

  <div class="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3">
    <details open class="settings-section">
      <summary
        class="cursor-pointer select-none text-[10px] font-semibold uppercase tracking-wide text-[color:var(--color-fg)]"
      >
        Receiver
      </summary>
      <div class="mt-2 flex flex-col gap-3 pl-1">
        <div class="flex items-baseline justify-between text-[10px]">
          <span class="text-[color:var(--color-muted)]">preset</span>
          <span class="font-mono text-slate-300">{presetLabel}</span>
        </div>
        <ProfileChips />
        {#if demodBlock}
          <section class="flex flex-col gap-1">
            <header class="flex items-baseline justify-between">
              <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
                >demod</span
              >
              <span class="font-mono text-[10px] text-slate-500">{demodBlock.type_name}</span>
            </header>
            {#if demodBlock.spec.params.length === 0}
              <p class="text-[10px] text-slate-600">no params</p>
            {:else}
              <BlockParams block={demodBlock} hideSourceRestart />
            {/if}
          </section>
        {:else}
          <p class="text-[10px] text-slate-600">
            no demod block — pick a preset from the Catalog tab.
          </p>
        {/if}
        <RdsReadout />
      </div>
    </details>

    <details open class="settings-section">
      <summary
        class="cursor-pointer select-none text-[10px] font-semibold uppercase tracking-wide text-[color:var(--color-fg)]"
      >
        Audio
      </summary>
      <div class="mt-2 pl-1">
        <AudioPanel />
      </div>
    </details>

    {#if audioNrBlock}
      <details class="settings-section">
        <summary
          class="cursor-pointer select-none text-[10px] font-semibold uppercase tracking-wide text-[color:var(--color-fg)]"
        >
          Noise Reduction
        </summary>
        <div class="mt-2 flex flex-col gap-1 pl-1">
          <div class="flex items-baseline justify-between text-[10px]">
            <span class="text-[color:var(--color-muted)]">block</span>
            <span class="font-mono text-slate-500">{audioNrBlock.type_name}</span>
          </div>
          <BlockParams block={audioNrBlock} hideSourceRestart />
        </div>
      </details>
    {/if}

    <details class="settings-section">
      <summary
        class="cursor-pointer select-none text-[10px] font-semibold uppercase tracking-wide text-[color:var(--color-fg)]"
      >
        Input
      </summary>
      <div class="mt-2 flex flex-col gap-3 pl-1">
        <InputControls />
        <RssiMeter />
      </div>
    </details>

    {#if pipeline.errorMessage}
      <p class="text-rose-400">{pipeline.errorMessage}</p>
    {/if}
  </div>
</div>

<style>
  .settings-section {
    border-top: 1px solid rgb(30 41 59);
    padding-top: 0.5rem;
  }
  .settings-section:first-child {
    border-top: none;
    padding-top: 0;
  }
</style>
