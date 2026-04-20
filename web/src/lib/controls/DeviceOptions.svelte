<script lang="ts">
  import { Dialog } from 'bits-ui';
  import type { SourceConfig } from '$lib/api/source';
  import type { DeviceCapabilities } from '$lib/api/devices';
  import {
    defaultsFor,
    toSourceConfig,
    validate,
    type OptionsState,
  } from '$lib/controls/optionsModel';

  interface Props {
    open: boolean;
    capabilities: DeviceCapabilities | null;
    onApply: (cfg: SourceConfig) => void;
    onClose: () => void;
  }

  let { open = $bindable(), capabilities, onApply, onClose }: Props = $props();

  let state = $state<OptionsState | null>(null);

  // Re-seed the form whenever a fresh device is presented or the dialog
  // is reopened. Without this the previous device's gains/freq leak in.
  $effect(() => {
    if (open && capabilities) {
      state = defaultsFor(capabilities);
    }
  });

  let errors = $derived(state ? validate(state) : []);
  let canApply = $derived(state !== null && errors.length === 0);

  function apply() {
    if (!state || !capabilities || errors.length > 0) return;
    onApply(toSourceConfig(capabilities, state));
    open = false;
  }

  function fmtHz(hz: number): string {
    if (hz >= 1e9) return `${(hz / 1e9).toFixed(3)} GHz`;
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(3)} MHz`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(1)} kHz`;
    return `${hz.toFixed(0)} Hz`;
  }
</script>

<Dialog.Root bind:open onOpenChange={(o) => !o && onClose()}>
  <Dialog.Portal>
    <Dialog.Overlay class="fixed inset-0 z-40 bg-black/50" />
    <Dialog.Content
      class="fixed left-1/2 top-1/2 z-50 w-[28rem] -translate-x-1/2 -translate-y-1/2 rounded-md border border-slate-800 bg-[color:var(--color-bg)] text-[color:var(--color-fg)] shadow-xl"
    >
      <div class="flex flex-col gap-4 p-4">
        <div class="flex items-baseline justify-between">
          <Dialog.Title class="text-sm font-semibold">
            {capabilities ? capabilities.info.label : 'Device options'}
          </Dialog.Title>
          <Dialog.Description
            class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
          >
            {capabilities?.driver_key ?? ''}
          </Dialog.Description>
        </div>

        {#if !state || !capabilities}
          <p class="text-xs text-[color:var(--color-muted)]">No channel info available.</p>
        {:else}
          <section class="flex flex-col gap-3">
            <header class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
              Sampling
            </header>
            <label class="grid gap-1 text-xs">
              <span class="text-[color:var(--color-muted)]">Sample rate</span>
              {#if state.sample_rate_choices.length > 1}
                <select
                  class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                  bind:value={state.sample_rate_hz}
                >
                  {#each state.sample_rate_choices as r (r)}
                    <option value={r}>{fmtHz(r)}</option>
                  {/each}
                </select>
              {:else}
                <input
                  type="number"
                  class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                  bind:value={state.sample_rate_hz}
                />
              {/if}
            </label>

            {#if state.bandwidth_choices.length > 0}
              <label class="grid gap-1 text-xs">
                <span class="text-[color:var(--color-muted)]">Analog bandwidth</span>
                <select
                  class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                  bind:value={state.bandwidth_hz}
                >
                  {#each state.bandwidth_choices as b (b)}
                    <option value={b}>{fmtHz(b)}</option>
                  {/each}
                </select>
              </label>
            {/if}
          </section>

          <section class="flex flex-col gap-3">
            <header class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
              Tuning
            </header>
            <label class="grid gap-1 text-xs">
              <div class="flex justify-between">
                <span class="text-[color:var(--color-muted)]">Centre frequency</span>
                <span class="font-mono">{fmtHz(state.center_freq_hz)}</span>
              </div>
              <input
                type="number"
                min={state.freq_range.min}
                max={state.freq_range.max}
                step={state.freq_range.step}
                class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                bind:value={state.center_freq_hz}
              />
              <div class="flex justify-between text-[10px] text-[color:var(--color-muted)]">
                <span>{fmtHz(state.freq_range.min)}</span>
                <span>{fmtHz(state.freq_range.max)}</span>
              </div>
            </label>
          </section>

          {#if state.antenna_choices.length > 0 || state.has_agc || state.gains.length > 0}
            <section class="flex flex-col gap-3">
              <header class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
                Frontend
              </header>

              {#if state.antenna_choices.length > 0}
                <label class="grid gap-1 text-xs">
                  <span class="text-[color:var(--color-muted)]">Antenna</span>
                  <select
                    class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                    bind:value={state.antenna}
                  >
                    {#each state.antenna_choices as a (a)}
                      <option value={a}>{a}</option>
                    {/each}
                  </select>
                </label>
              {/if}

              {#if state.has_agc}
                <label class="flex items-center justify-between gap-2 text-xs">
                  <span class="text-[color:var(--color-muted)]">AGC</span>
                  <input type="checkbox" bind:checked={state.agc} />
                </label>
              {/if}

              {#each state.gains as gain, i (gain.name)}
                <label class="grid gap-1 text-xs">
                  <div class="flex justify-between">
                    <span class="text-[color:var(--color-muted)]">{gain.name} gain</span>
                    <span class="font-mono">{gain.value_db.toFixed(1)} dB</span>
                  </div>
                  <input
                    type="range"
                    min={gain.range.min}
                    max={gain.range.max}
                    step={gain.range.step}
                    bind:value={state.gains[i].value_db}
                    disabled={state.has_agc && state.agc}
                  />
                  <div class="flex justify-between text-[10px] text-[color:var(--color-muted)]">
                    <span>{gain.range.min} dB</span>
                    <span>{gain.range.max} dB</span>
                  </div>
                </label>
              {/each}
            </section>
          {/if}

          {#if errors.length > 0}
            <ul class="text-[11px] text-rose-400">
              {#each errors as e (e)}
                <li>{e}</li>
              {/each}
            </ul>
          {/if}
        {/if}

        <div class="flex justify-end gap-2 pt-2">
          <button
            type="button"
            class="rounded border border-slate-700 px-3 py-1 text-sm"
            onclick={() => {
              open = false;
              onClose();
            }}
          >
            Cancel
          </button>
          <button
            type="button"
            class="rounded bg-[color:var(--color-accent)] px-3 py-1 text-sm font-semibold text-slate-900 disabled:opacity-50"
            disabled={!canApply}
            onclick={apply}
          >
            Apply
          </button>
        </div>
      </div>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
