<script lang="ts">
  // StationMap — the reusable vector-map primitive for "advanced"
  // mode views (FT8/FT4/WSPR station map first; ADS-B later).
  //
  // Deliberately offline: the basemap is bundled Natural-Earth
  // GeoJSON (static/map/ — 1:10m land + country borders + lakes,
  // 1:50m populated places for city dots), public domain, drawn by
  // MapLibre GL with no tile server and no API key. An SDR box on an
  // isolated network still gets a detailed map. Swapping in
  // PMTiles/online tiles later is still just the `sources`/`style`
  // block. (City *names* need an offline glyph font the style
  // doesn't bundle yet — dots only for now.)
  //
  // Pure presentational component: it owns no decode state. The
  // caller passes the resolved points; selection is two-way via
  // `selectedId` / `onselect` so a sibling table can stay coupled
  // (click a row → highlight the station, click the map → select the
  // row).

  import { onMount } from 'svelte';
  import 'maplibre-gl/dist/maplibre-gl.css';
  import type { Map as MlMap, GeoJSONSource, StyleSpecification, MapMouseEvent } from 'maplibre-gl';

  export interface MapStation {
    id: string;
    call: string;
    lat: number;
    lon: number;
    /** Visual treatment. `home`/`station` → circle (FT8/WSPR);
     *  `aircraft` → heading-rotated triangle (ADS-B). */
    kind?: 'home' | 'station' | 'aircraft';
    /** Track in degrees, only used for `kind: 'aircraft'`. */
    heading?: number;
  }
  export interface MapLink {
    id: string;
    from: [number, number]; // [lon, lat]
    to: [number, number];
  }
  /** A per-id position history polyline (ADS-B aircraft trails). */
  export interface MapTrail {
    id: string;
    path: [number, number][]; // [lon, lat] points, oldest → newest
  }

  interface Props {
    stations?: MapStation[];
    links?: MapLink[];
    /** Optional position trails (ADS-B). FT8/WSPR omit this. */
    trails?: MapTrail[];
    selectedId?: string | null;
    onselect?: (id: string | null) => void;
    /** localStorage key for persisting the camera (center/zoom/
     *  bearing/pitch). Each advanced view passes a distinct key so
     *  toggling the view off/and back — or a full reload — restores
     *  *that mode's* last pan/zoom instead of snapping to the world
     *  default. Different modes want very different framings (FT8
     *  global vs ADS-B/APRS local). */
    storageKey?: string;
  }

  let {
    stations = [],
    links = [],
    trails = [],
    selectedId = $bindable(null),
    onselect,
    storageKey = 'ferrite.map.camera',
  }: Props = $props();

  // Persisted camera (per `storageKey`). Read once for the initial
  // view; written on every moveend so a remount/reload resumes where
  // the operator left the map instead of the world default.
  interface Camera {
    center: [number, number];
    zoom: number;
    bearing: number;
    pitch: number;
  }
  function loadCamera(): Camera | null {
    if (typeof localStorage === 'undefined') return null;
    try {
      const c = JSON.parse(localStorage.getItem(storageKey) ?? 'null');
      if (
        c &&
        Array.isArray(c.center) &&
        c.center.length === 2 &&
        c.center.every((n: unknown) => typeof n === 'number' && Number.isFinite(n)) &&
        typeof c.zoom === 'number' &&
        Number.isFinite(c.zoom)
      ) {
        return {
          center: [c.center[0], c.center[1]],
          zoom: c.zoom,
          bearing: typeof c.bearing === 'number' ? c.bearing : 0,
          pitch: typeof c.pitch === 'number' ? c.pitch : 0,
        };
      }
    } catch {
      /* corrupt blob — fall through to default */
    }
    return null;
  }
  function saveCamera(): void {
    if (!map || typeof localStorage === 'undefined') return;
    const c = map.getCenter();
    try {
      localStorage.setItem(
        storageKey,
        JSON.stringify({
          center: [c.lng, c.lat],
          zoom: map.getZoom(),
          bearing: map.getBearing(),
          pitch: map.getPitch(),
        }),
      );
    } catch {
      /* storage full / disabled — non-fatal, camera just won't persist */
    }
  }

  let container: HTMLDivElement;
  let map: MlMap | undefined;
  let ready = $state(false);

  // Dark style consistent with the spectrum/waterfall panels. Sources
  // start empty; $effect below pumps the live data once the style has
  // loaded.
  const STYLE: StyleSpecification = {
    version: 8,
    // Bundled offline glyphs (Open Sans Regular, Latin + Latin-1
    // Supplement/Extended ranges) so symbol layers can render text
    // without a font server. Every text layer must pin
    // `text-font: ['Open Sans Regular']` so the {fontstack} token
    // resolves to exactly the bundled directory.
    glyphs: '/map/fonts/{fontstack}/{range}.pbf',
    sources: {
      // Natural Earth 1:10m vectors, bundled (offline, no tile
      // server). Land + country borders + lakes; 1:50m populated
      // places for city markers. Swapping to PMTiles/online tiles
      // later is still just this `sources`/`style` block.
      land: { type: 'geojson', data: '/map/ne_10m_land.geojson' },
      countries: { type: 'geojson', data: '/map/ne_10m_admin_0_countries.geojson' },
      lakes: { type: 'geojson', data: '/map/ne_10m_lakes.geojson' },
      places: { type: 'geojson', data: '/map/ne_50m_populated_places.geojson' },
      stations: { type: 'geojson', data: emptyFc() },
      links: { type: 'geojson', data: emptyFc() },
      trails: { type: 'geojson', data: emptyFc() },
    },
    layers: [
      { id: 'bg', type: 'background', paint: { 'background-color': '#0a0e14' } },
      {
        id: 'land',
        type: 'fill',
        source: 'land',
        paint: { 'fill-color': '#1b2230' },
      },
      {
        id: 'lakes',
        type: 'fill',
        source: 'lakes',
        paint: { 'fill-color': '#0e1626' },
      },
      {
        id: 'countries',
        type: 'line',
        source: 'countries',
        paint: { 'line-color': '#33425a', 'line-width': 0.6 },
      },
      {
        // City markers (1:50m populated places). Dotted by importance
        // — SCALERANK 0 = world city, higher = smaller town. Names
        // would need an offline glyph/font source the style doesn't
        // bundle yet (a follow-up); dots give geographic orientation
        // now.
        id: 'places',
        type: 'circle',
        source: 'places',
        paint: {
          'circle-radius': [
            'interpolate',
            ['linear'],
            ['coalesce', ['to-number', ['get', 'SCALERANK']], 10],
            0,
            3.2,
            4,
            2,
            8,
            1.2,
          ],
          'circle-color': '#5b6b86',
          'circle-opacity': 0.85,
        },
      },
      {
        // City names — only the major ones (low SCALERANK) so the map
        // isn't a wall of text; dots above still show all places.
        // NAMEASCII is pure-ASCII (always in the bundled Latin
        // glyphs); fall back to NAME.
        id: 'place-labels',
        type: 'symbol',
        source: 'places',
        filter: ['<=', ['coalesce', ['to-number', ['get', 'SCALERANK']], 10], 4],
        layout: {
          'text-field': ['coalesce', ['get', 'NAMEASCII'], ['get', 'NAME']],
          'text-font': ['Open Sans Regular'],
          'text-size': 9,
          'text-offset': [0, 0.7],
          'text-anchor': 'top',
          'symbol-sort-key': ['coalesce', ['to-number', ['get', 'SCALERANK']], 10],
        },
        paint: {
          'text-color': '#7e8aa0',
          'text-halo-color': '#0a0e14',
          'text-halo-width': 1,
        },
      },
      {
        id: 'links',
        type: 'line',
        source: 'links',
        paint: {
          'line-color': '#3b82f6',
          'line-opacity': 0.5,
          'line-width': 1,
        },
      },
      {
        id: 'trails',
        type: 'line',
        source: 'trails',
        paint: {
          'line-color': '#38bdf8',
          'line-opacity': 0.35,
          'line-width': 1,
        },
      },
      {
        id: 'stations',
        type: 'circle',
        source: 'stations',
        // Aircraft are drawn by the rotated-icon layer added at load;
        // keep them out of the circle layer so they don't double-draw.
        filter: ['!=', ['get', 'kind'], 'aircraft'],
        paint: {
          'circle-radius': [
            'case',
            ['==', ['get', 'kind'], 'home'],
            6,
            ['boolean', ['feature-state', 'selected'], false],
            6,
            4,
          ],
          'circle-color': [
            'case',
            ['==', ['get', 'kind'], 'home'],
            '#f59e0b',
            ['boolean', ['feature-state', 'selected'], false],
            '#f8fafc',
            '#38bdf8',
          ],
          'circle-stroke-color': '#0a0e14',
          'circle-stroke-width': 1,
        },
      },
      {
        id: 'labels',
        type: 'symbol',
        source: 'stations',
        layout: {
          'text-field': ['get', 'call'],
          'text-font': ['Open Sans Regular'],
          'text-size': 10,
          'text-offset': [0, 1.1],
          'text-anchor': 'top',
          'text-allow-overlap': false,
        },
        paint: {
          'text-color': '#cbd5e1',
          'text-halo-color': '#0a0e14',
          'text-halo-width': 1.2,
        },
      },
    ],
  };

  function emptyFc(): GeoJSON.FeatureCollection {
    return { type: 'FeatureCollection', features: [] };
  }

  // Densify a station-to-station path into a great-circle polyline so
  // long DX paths bow correctly instead of cutting straight across
  // the Mercator projection. Slerp on the unit sphere.
  function greatCircle(a: [number, number], b: [number, number], n = 48): [number, number][] {
    const r = Math.PI / 180;
    const d2 = 180 / Math.PI;
    const lon1 = a[0] * r;
    const lat1 = a[1] * r;
    const lon2 = b[0] * r;
    const lat2 = b[1] * r;
    const d =
      2 *
      Math.asin(
        Math.sqrt(
          Math.sin((lat2 - lat1) / 2) ** 2 +
            Math.cos(lat1) * Math.cos(lat2) * Math.sin((lon2 - lon1) / 2) ** 2,
        ),
      );
    if (!isFinite(d) || d === 0) return [a, b];
    const out: [number, number][] = [];
    for (let i = 0; i <= n; i++) {
      const f = i / n;
      const A = Math.sin((1 - f) * d) / Math.sin(d);
      const B = Math.sin(f * d) / Math.sin(d);
      const x = A * Math.cos(lat1) * Math.cos(lon1) + B * Math.cos(lat2) * Math.cos(lon2);
      const y = A * Math.cos(lat1) * Math.sin(lon1) + B * Math.cos(lat2) * Math.sin(lon2);
      const z = A * Math.sin(lat1) + B * Math.sin(lat2);
      out.push([Math.atan2(y, x) * d2, Math.atan2(z, Math.sqrt(x * x + y * y)) * d2]);
    }
    return out;
  }

  function stationsFc(list: MapStation[]): GeoJSON.FeatureCollection {
    return {
      type: 'FeatureCollection',
      features: list.map((s) => ({
        type: 'Feature',
        id: s.id,
        geometry: { type: 'Point', coordinates: [s.lon, s.lat] },
        properties: {
          call: s.call,
          kind: s.kind ?? 'station',
          heading: s.heading ?? 0,
        },
      })),
    };
  }

  function trailsFc(list: MapTrail[]): GeoJSON.FeatureCollection {
    return {
      type: 'FeatureCollection',
      features: list
        .filter((t) => t.path.length >= 2)
        .map((t) => ({
          type: 'Feature',
          id: t.id,
          geometry: { type: 'LineString', coordinates: t.path },
          properties: {},
        })),
    };
  }

  function linksFc(list: MapLink[]): GeoJSON.FeatureCollection {
    return {
      type: 'FeatureCollection',
      features: list.map((l) => ({
        type: 'Feature',
        id: l.id,
        geometry: { type: 'LineString', coordinates: greatCircle(l.from, l.to) },
        properties: {},
      })),
    };
  }

  // Track which feature currently carries the `selected` feature-state
  // so we can clear it before setting the next one.
  let stateSelected: string | null = null;

  function applySelection(id: string | null): void {
    if (!map) return;
    if (stateSelected !== null) {
      map.setFeatureState({ source: 'stations', id: stateSelected }, { selected: false });
    }
    if (id !== null) {
      map.setFeatureState({ source: 'stations', id }, { selected: true });
    }
    stateSelected = id;
  }

  onMount(() => {
    let disposed = false;
    // Dynamic import keeps MapLibre (and its WebGL/`window` use) out
    // of the SSR/prerender bundle entirely.
    (async () => {
      const maplibregl = (await import('maplibre-gl')).default;
      if (disposed) return;
      const cam = loadCamera();
      map = new maplibregl.Map({
        container,
        style: STYLE,
        center: cam?.center ?? [0, 25],
        zoom: cam?.zoom ?? 1.2,
        bearing: cam?.bearing ?? 0,
        pitch: cam?.pitch ?? 0,
        attributionControl: false,
        // Mercator is lighter than the v5 globe and fine for a
        // station map; revisit if polar paths matter.
        renderWorldCopies: true,
      });
      map.addControl(new maplibregl.NavigationControl({ showCompass: false }), 'top-right');
      // Persist the camera after any pan/zoom/rotate so a remount
      // (view toggle) or reload resumes here, not at the default.
      map.on('moveend', saveCamera);

      map.on('load', () => {
        // Offline rotated-aircraft marker: a triangle rasterised on a
        // canvas and registered via addImage (no glyphs/sprite server
        // needed — works on an isolated SDR box). SDF so `icon-color`
        // can recolour it for the selection highlight.
        const S = 18;
        const cv = document.createElement('canvas');
        cv.width = S;
        cv.height = S;
        const cx = cv.getContext('2d')!;
        cx.fillStyle = '#fff';
        cx.beginPath();
        cx.moveTo(S / 2, 1); // apex = heading 0 (north)
        cx.lineTo(S - 2, S - 2);
        cx.lineTo(S / 2, S * 0.68);
        cx.lineTo(2, S - 2);
        cx.closePath();
        cx.fill();
        const img = cx.getImageData(0, 0, S, S);
        if (!map!.hasImage('aircraft-tri')) {
          map!.addImage('aircraft-tri', img, { sdf: true });
        }
        map!.addLayer({
          id: 'aircraft',
          type: 'symbol',
          source: 'stations',
          filter: ['==', ['get', 'kind'], 'aircraft'],
          layout: {
            'icon-image': 'aircraft-tri',
            'icon-rotate': ['get', 'heading'],
            'icon-rotation-alignment': 'map',
            'icon-allow-overlap': true,
            'icon-size': 1,
          },
          paint: {
            'icon-color': [
              'case',
              ['boolean', ['feature-state', 'selected'], false],
              '#f8fafc',
              '#38bdf8',
            ],
          },
        });
        ready = true;
      });

      // Both the FT8 circle layer and the ADS-B icon layer are
      // clickable; whichever the preset uses, the other is empty.
      const HIT = ['stations', 'aircraft'];
      const hit = (e: MapMouseEvent) =>
        map!.queryRenderedFeatures(e.point, {
          layers: HIT.filter((l) => map!.getLayer(l)),
        });
      const pick = (e: MapMouseEvent) => {
        const f = hit(e)[0];
        const id = f ? String(f.id) : null;
        selectedId = id;
        onselect?.(id);
      };
      map.on('click', 'stations', pick);
      map.on('click', 'aircraft', pick);
      map.on('mouseenter', 'aircraft', () => (map!.getCanvas().style.cursor = 'pointer'));
      map.on('mouseleave', 'aircraft', () => (map!.getCanvas().style.cursor = ''));
      map.on('click', (e) => {
        // Clicks that miss every marker clear the selection.
        if (!hit(e).length) {
          selectedId = null;
          onselect?.(null);
        }
      });
      map.on('mouseenter', 'stations', () => (map!.getCanvas().style.cursor = 'pointer'));
      map.on('mouseleave', 'stations', () => (map!.getCanvas().style.cursor = ''));
    })();

    return () => {
      disposed = true;
      map?.remove();
      map = undefined;
      ready = false;
    };
  });

  // Push live station/link data once the style is up. Re-runs
  // whenever the reactive props change.
  $effect(() => {
    const sFc = stationsFc(stations);
    const lFc = linksFc(links);
    const tFc = trailsFc(trails);
    if (!ready || !map) return;
    (map.getSource('stations') as GeoJSONSource | undefined)?.setData(sFc);
    (map.getSource('links') as GeoJSONSource | undefined)?.setData(lFc);
    (map.getSource('trails') as GeoJSONSource | undefined)?.setData(tFc);
    // setData drops feature-state; re-assert the current selection.
    stateSelected = null;
    applySelection(selectedId ?? null);
  });

  // External selection changes (e.g. a table row click) reflect onto
  // the map highlight without re-pushing all the data.
  $effect(() => {
    if (ready && map) applySelection(selectedId ?? null);
  });
</script>

<div bind:this={container} class="h-full w-full" data-testid="station-map"></div>

<style>
  /* MapLibre injects its own canvas; ensure it fills the panel. */
  div :global(.maplibregl-map) {
    height: 100%;
    width: 100%;
  }
</style>
