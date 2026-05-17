<script lang="ts">
  // Advanced view for APRS — station map + station table + a live
  // packet console (status / messages / raw frames). Mounted when the
  // operator toggles "APRS" and the active preset advertises a
  // `ui:aprs` sink. Selection is keyed by callsign, coupling the
  // table and the map both ways.

  import { pipeline } from '$lib/pipeline.svelte';
  import { aprs, type AprsStation } from '$lib/aprs/store.svelte';
  import DecodeTable from '$lib/viz/DecodeTable.svelte';
  import StationMap from '$lib/viz/StationMap.svelte';
  import TextConsole from '$lib/viz/TextConsole.svelte';
  import Split from '$lib/layout/Split.svelte';

  let streamId = $derived(pipeline.uiSinks.aprs?.stream_id);
  let client = $derived(pipeline.client);

  $effect(() => {
    if (client && streamId !== undefined) {
      aprs.attach(client, streamId);
      return () => aprs.detach();
    }
    aprs.detach();
    return () => {};
  });

  let selected = $state<string | null>(null);
  function onReset(): void {
    aprs.reset();
    selected = null;
  }

  const fix = (v: number | null) => (v === null ? '—' : v.toFixed(3));
  const columns = [
    { key: 'id' as const, label: 'Station' },
    { key: 'kind' as const, label: 'Type' },
    { key: 'lat' as const, label: 'Lat', numeric: true, format: (s: AprsStation) => fix(s.lat) },
    { key: 'lon' as const, label: 'Lon', numeric: true, format: (s: AprsStation) => fix(s.lon) },
    {
      key: 'text' as const,
      label: 'Comment / text',
      format: (s: AprsStation) => s.name ?? s.text ?? '',
    },
    { key: 'msgs' as const, label: 'n', numeric: true, format: (s: AprsStation) => `${s.msgs}` },
  ];

  let positioned = $derived(aprs.mapStations.length);
</script>

<div class="flex h-full w-full min-h-0 flex-col bg-[color:var(--color-bg)]">
  <header class="panel-head">
    <span>
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">APRS</span>
      <span class="ml-2 text-[color:var(--color-muted)]">
        {aprs.stations.length} stations · {positioned} positioned
      </span>
    </span>
    <button
      type="button"
      class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none normal-case text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
      disabled={aprs.stations.length === 0 && aprs.lines.length === 0}
      title="Clear stations + console (otherwise persists across view toggles)"
      onclick={onReset}
    >
      Reset
    </button>
  </header>

  <div class="min-h-0 flex-1">
    <Split direction="row" defaultFraction={0.5} storageKey="ferrite.split.advanced-aprs">
      {#snippet a()}
        <div class="h-full min-h-0">
          <StationMap
            stations={aprs.mapStations}
            selectedId={selected}
            onselect={(id) => (selected = id)}
            storageKey="ferrite.map.aprs"
          />
        </div>
      {/snippet}
      {#snippet b()}
        <div class="flex h-full min-h-0 flex-col border-l border-slate-800">
          <Split
            direction="column"
            defaultFraction={0.55}
            storageKey="ferrite.split.advanced-aprs-right"
          >
            {#snippet a()}
              <div class="h-full min-h-0">
                <DecodeTable
                  {columns}
                  rows={aprs.stations}
                  selectedId={selected}
                  onselect={(id) => (selected = id)}
                  defaultSort={{ key: 'msgs', dir: 'desc' }}
                  placeholder="listening for APRS… (144.39 NA / 144.80 EU)"
                />
              </div>
            {/snippet}
            {#snippet b()}
              <div class="flex h-full min-h-0 flex-col border-t border-slate-800">
                <div
                  class="px-2 py-0.5 text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
                >
                  Packet console
                </div>
                <div class="min-h-0 flex-1">
                  <TextConsole lines={aprs.lines} placeholder="no packets yet…" />
                </div>
              </div>
            {/snippet}
          </Split>
        </div>
      {/snippet}
    </Split>
  </div>
</div>

<style>
  .panel-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 3px 8px;
    border-bottom: 1px solid rgb(30 41 59);
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--color-muted);
  }
</style>
