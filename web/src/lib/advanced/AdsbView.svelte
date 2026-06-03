<script lang="ts">
  // Advanced view for ADS-B — a station map (hero) beside an aircraft
  // table. Mounted in place of the wide FFT/waterfall column when the
  // operator toggles "ADS-B" and the active preset advertises a
  // `ui:adsb` sink. Selection is keyed by ICAO so the map triangle and
  // the table row stay coupled both ways.

  import { adsb, type Aircraft } from '$lib/adsb/store.svelte';
  import DecodeTable from '$lib/viz/DecodeTable.svelte';
  import StationMap from '$lib/viz/StationMap.svelte';
  import Split from '$lib/layout/Split.svelte';

  // Aircraft come from the always-connected decoder mirror (kind `adsb`)
  // — no per-view WS attach.

  // Selection is the ICAO id — shared verbatim by table + map.
  let selected = $state<string | null>(null);
  function onReset(): void {
    adsb.reset();
    selected = null;
  }

  const columns = [
    {
      key: 'flight' as const,
      label: 'Flight',
      format: (a: Aircraft) => a.flight || '—',
      // FlightAware tracks by flight ident (the ADS-B callsign, e.g.
      // UAL123). Only link when we actually have one.
      href: (a: Aircraft) =>
        a.flight.trim()
          ? `https://www.flightaware.com/live/flight/${encodeURIComponent(a.flight.trim())}`
          : null,
    },
    {
      key: 'id' as const,
      label: 'ICAO',
      // Look up the airframe (registration, type, photos) by its 24-bit
      // ICAO hex address.
      href: (a: Aircraft) =>
        a.id ? `https://www.planespotters.net/hex/${encodeURIComponent(a.id.toUpperCase())}` : null,
    },
    {
      key: 'alt' as const,
      label: 'Alt ft',
      numeric: true,
      format: (a: Aircraft) => (a.alt ? a.alt.toLocaleString() : '—'),
    },
    {
      key: 'gs' as const,
      label: 'GS kt',
      numeric: true,
      format: (a: Aircraft) => (a.gs ? `${a.gs}` : '—'),
    },
    {
      key: 'trk' as const,
      label: 'Trk',
      numeric: true,
      format: (a: Aircraft) => (a.lat === null ? '—' : `${a.trk}°`),
    },
    { key: 'msgs' as const, label: 'Msgs', numeric: true, format: (a: Aircraft) => `${a.msgs}` },
    { key: 'age' as const, label: 'Age', numeric: true, format: (a: Aircraft) => `${a.age}s` },
  ];

  let positioned = $derived(adsb.stations.length);
</script>

<div class="flex h-full w-full min-h-0 flex-col bg-[color:var(--color-bg)]">
  <header class="panel-head">
    <span>
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">ADS-B</span>
      <span class="ml-2 text-[color:var(--color-muted)]">
        {adsb.aircraft.length} aircraft · {positioned} positioned
      </span>
    </span>
    <button
      type="button"
      class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none normal-case text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
      disabled={adsb.aircraft.length === 0}
      title="Clear the aircraft list + tracks (otherwise persists across view toggles)"
      onclick={onReset}
    >
      Reset
    </button>
  </header>

  <div class="min-h-0 flex-1">
    <!-- Map-hero: the map is the point of ADS-B; the table rides
         alongside as the sortable detail/selection list. -->
    <Split direction="row" defaultFraction={0.64} storageKey="ferrite.split.advanced-adsb">
      {#snippet a()}
        <div class="h-full min-h-0">
          <StationMap
            stations={adsb.stations}
            trails={adsb.trails}
            selectedId={selected}
            onselect={(id) => (selected = id)}
            storageKey="ferrite.map.adsb"
          />
        </div>
      {/snippet}
      {#snippet b()}
        <div class="h-full min-h-0 border-l border-slate-800">
          <DecodeTable
            {columns}
            rows={adsb.aircraft}
            selectedId={selected}
            onselect={(id) => (selected = id)}
            defaultSort={{ key: 'alt', dir: 'desc' }}
            placeholder="listening for aircraft… (needs line-of-sight + 1090 MHz)"
          />
        </div>
      {/snippet}
    </Split>
  </div>
</div>

<style></style>
