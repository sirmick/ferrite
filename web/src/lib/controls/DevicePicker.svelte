<script lang="ts">
  import {
    deviceArgsString,
    deviceLabel,
    fetchDevices,
    normaliseEntries,
    type DeviceEntry,
    type DeviceInfo,
  } from '$lib/api/devices';

  interface Props {
    /** Fired when the user clicks "Open" on an available device. */
    onSelect: (info: DeviceInfo, args: string) => void;
  }

  let { onSelect }: Props = $props();

  type LoadState =
    | { kind: 'idle' }
    | { kind: 'loading' }
    | { kind: 'unsupported' }
    | { kind: 'error'; message: string }
    | { kind: 'ok'; entries: DeviceEntry[] };

  let state = $state<LoadState>({ kind: 'idle' });

  async function refresh() {
    state = { kind: 'loading' };
    try {
      const raw = await fetchDevices();
      if (raw === null) {
        state = { kind: 'unsupported' };
        return;
      }
      // The wire form for `available` entries flattens DeviceCapabilities
      // into the same map as the discriminator; normalise so consumers
      // don't have to know about that.
      const entries = normaliseEntries(raw as unknown as Array<Record<string, unknown>>);
      state = { kind: 'ok', entries };
    } catch (err) {
      state = { kind: 'error', message: err instanceof Error ? err.message : String(err) };
    }
  }

  $effect(() => {
    void refresh();
  });
</script>

<div class="flex flex-col gap-3 text-sm">
  <div class="flex items-center justify-between">
    <h2 class="text-sm font-semibold">Devices</h2>
    <button
      type="button"
      class="rounded border border-slate-700 px-2 py-0.5 text-xs hover:border-slate-600"
      onclick={() => void refresh()}
      disabled={state.kind === 'loading'}
    >
      {state.kind === 'loading' ? 'Refreshing…' : 'Refresh'}
    </button>
  </div>

  {#if state.kind === 'idle' || state.kind === 'loading'}
    <p class="text-xs text-[color:var(--color-muted)]">Probing SoapySDR devices…</p>
  {:else if state.kind === 'unsupported'}
    <p class="text-xs text-[color:var(--color-muted)]">
      ferrited was built without the <code>soapysdr</code> feature; rebuild with
      <code>cargo run -p ferrited --features soapysdr</code> to enable hardware.
    </p>
  {:else if state.kind === 'error'}
    <p class="text-xs text-rose-400">Failed to load devices: {state.message}</p>
  {:else if state.entries.length === 0}
    <p class="text-xs text-[color:var(--color-muted)]">
      No SoapySDR devices found. Plug one in and click Refresh.
    </p>
  {:else}
    <ul class="flex flex-col gap-2">
      {#each state.entries as entry, i (i)}
        <li class="rounded border border-slate-800 bg-slate-900/40 p-3">
          <div class="flex items-baseline justify-between gap-3">
            <div class="flex flex-col">
              <span class="font-medium">{deviceLabel(entry)}</span>
              <span class="font-mono text-[10px] text-[color:var(--color-muted)]">
                {entry.status === 'available'
                  ? deviceArgsString(entry.capabilities.info)
                  : deviceArgsString(entry.info)}
              </span>
            </div>
            {#if entry.status === 'available'}
              <button
                type="button"
                class="rounded bg-[color:var(--color-accent)] px-3 py-1 text-xs font-semibold text-slate-900"
                onclick={() => {
                  const info = entry.capabilities.info;
                  onSelect(info, deviceArgsString(info));
                }}
              >
                Open
              </button>
            {:else}
              <span
                class="rounded border border-amber-700 px-2 py-0.5 text-[10px] uppercase tracking-wide text-amber-400"
                title={entry.error}
              >
                Unavailable
              </span>
            {/if}
          </div>
          {#if entry.status === 'available'}
            <dl class="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-[11px]">
              <dt class="text-[color:var(--color-muted)]">Driver</dt>
              <dd class="font-mono">{entry.capabilities.driver_key}</dd>
              <dt class="text-[color:var(--color-muted)]">Hardware</dt>
              <dd class="font-mono">{entry.capabilities.hardware_key}</dd>
              <dt class="text-[color:var(--color-muted)]">Rx channels</dt>
              <dd class="font-mono">{entry.capabilities.rx_channels.length}</dd>
            </dl>
          {:else}
            <p class="mt-2 text-[11px] text-amber-300/80">{entry.error}</p>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>
