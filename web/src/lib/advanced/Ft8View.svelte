<script lang="ts">
  // Advanced view shared by FT8 / FT4 / WSPR — a decode table beside
  // a station map (WSJT-X + GridTracker, fused). Mounted in place of
  // the wide FFT/waterfall column when the operator toggles
  // "Advanced" and the active preset advertises a `ui:ft8` sink.
  //
  // Selection is keyed by callsign so the table and map stay coupled:
  // click a decode row → its station highlights on the map and the
  // map recentres nothing (operator-driven pan only); click a
  // station → its most-recent row highlights and scrolls into view.

  import { pipeline } from '$lib/pipeline.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { ft8, type FtDecode } from '$lib/ft8/store.svelte';
  import DecodeTable from '$lib/viz/DecodeTable.svelte';
  import StationMap, { type MapStation } from '$lib/viz/StationMap.svelte';
  import Split from '$lib/layout/Split.svelte';

  let streamId = $derived(pipeline.uiSinks.ft8?.stream_id);
  let client = $derived(pipeline.client);

  $effect(() => {
    if (client && streamId !== undefined) {
      ft8.attach(client, streamId);
      return () => ft8.detach();
    }
    ft8.detach();
    return () => {};
  });

  // Operator QTH grid comes from client settings, not the stream.
  $effect(() => {
    ft8.setHomeGrid(clientControls.get('client.station.grid'));
  });

  // Selection is by callsign — shared between both panes.
  let selectedCall = $state<string | null>(null);

  // Most-recent row for the selected call drives the table highlight
  // + scroll-into-view (the map is keyed by callsign directly).
  let tableSelId = $derived.by(() => {
    if (!selectedCall) return null;
    let id: string | null = null;
    for (const d of ft8.decodes) if (d.de === selectedCall) id = d.id;
    return id;
  });
  function onTableSelect(id: string | null): void {
    selectedCall = id ? (ft8.decodes.find((d) => d.id === id)?.de ?? null) : null;
  }

  // The only thing that clears the long-lived log. Also drops the
  // current selection so the now-empty table/map aren't left
  // referencing a vanished callsign.
  function onReset(): void {
    ft8.reset();
    selectedCall = null;
  }

  // Mode is per-session (one preset). Derive it from the stream so
  // the columns match (WSPR has power/drift + no DX; FT8/FT4 have a
  // DX call). Default to ft8 until the first decode lands.
  let mode = $derived(ft8.decodes.at(-1)?.t ?? 'ft8');

  const hms = (utc: number) => (utc ? new Date(utc * 1000).toISOString().slice(11, 19) : '—');
  const snr = (d: FtDecode) => (d.snr > 0 ? `+${d.snr}` : `${d.snr}`);

  let columns = $derived([
    { key: 'utc' as const, label: 'UTC', numeric: true, format: (d: FtDecode) => hms(d.utc) },
    { key: 'snr' as const, label: 'dB', numeric: true, format: snr },
    {
      key: 'dt' as const,
      label: 'DT',
      numeric: true,
      format: (d: FtDecode) => (d.dt === null ? '—' : d.dt.toFixed(1)),
    },
    {
      key: 'freq' as const,
      label: 'Freq',
      numeric: true,
      format: (d: FtDecode) => `${Math.round(d.freq)}`,
    },
    { key: 'de' as const, label: 'Call', format: (d: FtDecode) => d.de || '—' },
    ...(mode === 'wspr'
      ? [
          {
            key: 'pwr' as const,
            label: 'dBm',
            numeric: true,
            format: (d: FtDecode) => (d.pwr === null ? '—' : `${d.pwr}`),
          },
          {
            key: 'drift' as const,
            label: 'Drift',
            numeric: true,
            format: (d: FtDecode) => (d.drift === null ? '—' : `${d.drift}`),
          },
        ]
      : [
          {
            key: 'dx' as const,
            label: 'To',
            format: (d: FtDecode) => d.dx ?? '—',
          },
        ]),
    { key: 'msg' as const, label: 'Message', format: (d: FtDecode) => d.msg },
  ]);

  // Home marker rides alongside the decoded stations.
  let mapStations = $derived.by<MapStation[]>(() => {
    const h = ft8.home;
    const base = ft8.stations;
    return h
      ? [...base, { id: '__home__', call: h.call, lat: h.lat, lon: h.lon, kind: 'home' as const }]
      : base;
  });

  let modeLabel = $derived(mode.toUpperCase());
</script>

<div class="flex h-full w-full min-h-0 flex-col bg-[color:var(--color-bg)]">
  <header class="panel-head">
    <span>
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">{modeLabel}</span>
      <span class="ml-2 text-[color:var(--color-muted)]">
        {ft8.decodes.length} decodes · {ft8.stations.length} mapped
      </span>
    </span>
    <span class="flex items-center gap-2">
      {#if !ft8.home}
        <span
          class="text-[10px] text-[color:var(--color-muted)]"
          title="Set your Maidenhead grid in settings to draw contact lines from your QTH"
        >
          no home grid — set <code>client.station.grid</code>
        </span>
      {/if}
      <button
        type="button"
        class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none normal-case text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
        disabled={ft8.decodes.length === 0}
        title="Clear the decode log + station map (the log otherwise persists across view toggles and ft8/ft4/wspr switches)"
        onclick={onReset}
      >
        Reset
      </button>
    </span>
  </header>

  <div class="min-h-0 flex-1">
    <Split direction="row" defaultFraction={0.62} storageKey="ferrite.split.advanced-ft8">
      {#snippet a()}
        <div class="h-full min-h-0">
          <DecodeTable
            {columns}
            rows={ft8.decodes}
            selectedId={tableSelId}
            onselect={onTableSelect}
            defaultSort={{ key: 'utc', dir: 'desc' }}
            placeholder="waiting for the next slot…"
          />
        </div>
      {/snippet}
      {#snippet b()}
        <div class="h-full min-h-0 border-l border-slate-800">
          <StationMap
            stations={mapStations}
            links={ft8.links}
            selectedId={selectedCall}
            onselect={(id) => (selectedCall = id)}
            storageKey="ferrite.map.ft8"
          />
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
