<script lang="ts">
  import {
    deviceArgsString,
    deviceLabel,
    fetchDevices,
    normaliseEntries,
    reloadDevices,
    type DeviceCapabilities,
    type DeviceEntry,
  } from '$lib/api/devices';

  interface Props {
    /** Fired when the user clicks "Open" on an available device. */
    onSelect: (capabilities: DeviceCapabilities) => void;
  }

  let { onSelect }: Props = $props();

  type LoadState =
    | { kind: 'idle' }
    | { kind: 'loading' }
    | { kind: 'error'; message: string }
    | { kind: 'ok'; entries: DeviceEntry[] };

  let loadState = $state<LoadState>({ kind: 'idle' });
  // Surfaces feedback from the "Reload drivers" button (`POST
  // /api/devices/reload`) — most often "stop the pipeline first" when
  // the server returns 409.
  let reloadStatus = $state<string | null>(null);
  let reloading = $state(false);

  async function refresh() {
    loadState = { kind: 'loading' };
    try {
      const raw = await fetchDevices();
      // The wire form for `available` entries flattens DeviceCapabilities
      // into the same map as the discriminator; normalise so consumers
      // don't have to know about that.
      const entries = normaliseEntries(raw as unknown as Array<Record<string, unknown>>);
      loadState = { kind: 'ok', entries };
    } catch (err) {
      loadState = { kind: 'error', message: err instanceof Error ? err.message : String(err) };
    }
  }

  /** Last-resort recovery when SoapySDR's in-process state is wedged
   *  (external `SoapySDRUtil --find` works but our enumerate hangs).
   *  Server unloads + reloads every driver module, refusing with 409
   *  if the pipeline is up — the resulting message is surfaced as
   *  `reloadStatus` for the operator. */
  async function onReload() {
    reloading = true;
    reloadStatus = null;
    try {
      await reloadDevices();
      reloadStatus = 'Drivers reloaded — re-probing devices…';
      await refresh();
      reloadStatus = 'Drivers reloaded.';
    } catch (err) {
      reloadStatus = err instanceof Error ? err.message : String(err);
    } finally {
      reloading = false;
    }
  }

  $effect(() => {
    void refresh();
  });
</script>

<div class="flex flex-col gap-2 text-sm">
  <div class="flex items-center justify-between gap-2">
    <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">Devices</span>
    <div class="flex items-center gap-1">
      <button
        type="button"
        class="rounded border border-slate-800 px-1.5 py-0 text-[10px] hover:border-slate-600 disabled:cursor-not-allowed disabled:opacity-50"
        onclick={() => void onReload()}
        disabled={reloading || loadState.kind === 'loading'}
        title="Unload + re-load every SoapySDR driver module — use when external `SoapySDRUtil --find` works but our list hangs/misses. Pipeline must be stopped."
      >
        {reloading ? 'Reloading…' : 'Reload'}
      </button>
      <button
        type="button"
        class="rounded border border-slate-800 px-1.5 py-0 text-[10px] hover:border-slate-600"
        onclick={() => void refresh()}
        disabled={loadState.kind === 'loading' || reloading}
        title="Re-probe currently-loaded SoapySDR drivers for connected devices."
      >
        {loadState.kind === 'loading' ? '…' : 'Refresh'}
      </button>
    </div>
  </div>
  {#if reloadStatus !== null}
    <p class="text-[11px] text-[color:var(--color-muted)]">{reloadStatus}</p>
  {/if}

  {#if loadState.kind === 'idle' || loadState.kind === 'loading'}
    <p class="text-[11px] text-[color:var(--color-muted)]">Probing SoapySDR devices…</p>
  {:else if loadState.kind === 'error'}
    <p class="text-[11px] text-rose-400">Failed to load devices: {loadState.message}</p>
  {:else if loadState.entries.length === 0}
    <p class="text-[11px] text-[color:var(--color-muted)]">No SoapySDR devices found.</p>
  {:else}
    <!-- Dense one-line rows. Whole row is the select affordance for
         available entries; unavailable entries are dimmed + show the
         reason in the title attribute (hover to read). -->
    <ul class="flex flex-col">
      {#each loadState.entries as entry, i (i)}
        {#if entry.status === 'available'}
          <li>
            <button
              type="button"
              class="flex w-full items-center justify-between gap-3 rounded px-2 py-1 text-left hover:bg-slate-800/60 focus:bg-slate-800/60 focus:outline-none"
              onclick={() => onSelect(entry.capabilities)}
              title={deviceArgsString(entry.capabilities.info)}
            >
              <span class="truncate">{deviceLabel(entry)}</span>
              <span class="font-mono text-[10px] text-[color:var(--color-muted)] whitespace-nowrap">
                {entry.capabilities.driver_key} · rx:{entry.capabilities.rx_channels.length}
              </span>
            </button>
          </li>
        {:else}
          <li
            class="flex items-center justify-between gap-3 rounded px-2 py-1 opacity-60"
            title={entry.error}
          >
            <span class="truncate">{deviceLabel(entry)}</span>
            <span
              class="font-mono text-[10px] whitespace-nowrap text-amber-400"
              title={entry.error}
            >
              unavailable
            </span>
          </li>
        {/if}
      {/each}
    </ul>
  {/if}
</div>
