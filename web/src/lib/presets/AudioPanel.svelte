<script lang="ts">
  // Audio panel — volume + live level meter. Browser-only; the server
  // AudioSink just emits unscaled samples. The worklet applies the gain
  // on read and reports peak/RMS back here for display.
  //
  // Lives in the Settings sidebar as a sibling of Input and Receiver.
  // Visible only when an AudioSink is instantiated and attached to the
  // audio context — bareband presets that don't demodulate will hide
  // it entirely.

  import { audioPanel, linearToDbfs } from '$lib/audio/audioPanel.svelte';
  import { browserRuntime } from '$lib/runner/browserRuntime.svelte';

  // Scale the linear [0, 1] peak/rms to a 0..1 meter fraction over a
  // -60..0 dBFS range. Clipped peaks saturate the right side of the
  // bar without overflowing.
  const METER_MIN_DB = -60;
  const METER_MAX_DB = 0;
  function dbfsToFraction(db: number): number {
    if (!Number.isFinite(db)) return 0;
    return Math.max(0, Math.min(1, (db - METER_MIN_DB) / (METER_MAX_DB - METER_MIN_DB)));
  }

  let peakDb = $derived(linearToDbfs(audioPanel.peak));
  let rmsDb = $derived(linearToDbfs(audioPanel.rms));
  let peakHoldDb = $derived(linearToDbfs(audioPanel.peakHold));

  let rmsFrac = $derived(dbfsToFraction(rmsDb));
  let peakFrac = $derived(dbfsToFraction(peakDb));
  let peakHoldFrac = $derived(dbfsToFraction(peakHoldDb));

  // Simple colour thresholds so the bar reads at a glance.
  //   < -18 dBFS: green (comfortable)
  //   -18 to -6: yellow (getting loud)
  //   > -6: red (approaching clip)
  let peakColour = $derived(peakDb > -6 ? '#f87171' : peakDb > -18 ? '#facc15' : '#34d399');

  let volumePct = $derived(Math.round(audioPanel.volume * 100));

  function onVolume(ev: Event) {
    const v = Number((ev.target as HTMLInputElement).value);
    if (!Number.isFinite(v)) return;
    audioPanel.setVolume(v / 100);
  }
</script>

<div class="flex flex-col gap-3 text-xs">
  {#if !audioPanel.attached}
    <p class="text-[10px] text-slate-600">no audio sink in current preset</p>
  {:else}
    {#if browserRuntime.audioState === 'suspended'}
      <p class="text-[10px] text-amber-400/80">
        audio context suspended — click anywhere on the page to start playback
      </p>
    {/if}

    <label class="flex flex-col gap-1">
      <div class="flex items-baseline justify-between">
        <span class="text-[color:var(--color-muted)]">volume</span>
        <span class="font-mono text-slate-300">{audioPanel.muted ? 'muted' : `${volumePct}%`}</span>
      </div>
      <input
        type="range"
        min="0"
        max="200"
        step="1"
        value={volumePct}
        oninput={onVolume}
        disabled={audioPanel.muted}
        class="w-full"
      />
      <label class="flex items-center gap-1">
        <input
          type="checkbox"
          checked={audioPanel.muted}
          onchange={(e) => audioPanel.setMuted((e.currentTarget as HTMLInputElement).checked)}
        />
        <span>mute</span>
      </label>
    </label>

    <div class="flex flex-col gap-1">
      <div class="flex items-baseline justify-between">
        <span class="text-[color:var(--color-muted)]">level</span>
        <span class="font-mono text-slate-300">
          {#if Number.isFinite(peakDb) && peakDb > METER_MIN_DB}
            {peakDb.toFixed(1)} dBFS
          {:else}
            —
          {/if}
        </span>
      </div>
      <!-- Layered bars: RMS (solid), live peak (thin line), peak-hold (thin line) -->
      <div class="meter">
        <div
          class="meter-rms"
          style:width="{rmsFrac * 100}%"
          style:background-color={peakColour}
        ></div>
        <div class="meter-peak" style:left="{peakFrac * 100}%"></div>
        {#if peakHoldFrac > 0}
          <div class="meter-hold" style:left="{peakHoldFrac * 100}%"></div>
        {/if}
      </div>
      <div class="flex justify-between text-[9px] text-[color:var(--color-muted)]">
        <span>{METER_MIN_DB}</span>
        <span>-30</span>
        <span>0 dBFS</span>
      </div>
    </div>
  {/if}
</div>

<style>
  .meter {
    position: relative;
    height: 0.75rem;
    border-radius: 0.125rem;
    background: rgb(15 23 42);
    border: 1px solid rgb(30 41 59);
    overflow: hidden;
  }
  .meter-rms {
    position: absolute;
    top: 0;
    left: 0;
    bottom: 0;
    transition: width 60ms linear;
  }
  .meter-peak {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 2px;
    background: rgba(248, 250, 252, 0.85);
    transform: translateX(-1px);
    transition: left 60ms linear;
  }
  .meter-hold {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 2px;
    background: rgba(252, 165, 165, 0.9);
    transform: translateX(-1px);
  }
</style>
