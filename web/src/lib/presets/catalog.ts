// Loader for the Signal Catalog — enumerates flowgraph presets shipped
// under `flowgraphs/` at the repo root and exposes a filtered,
// structurally-valid list to the UI.
//
// Presets are imported via `import.meta.glob` with `eager: true`, so the
// catalog is resolved at build time and embedded in the bundle. The
// structural checks here are deliberately minimal — the Rust runtime
// performs the authoritative validation at load time, so the catalog
// only filters out shapes that would fail to even reach the runtime.

import type { FlowgraphDoc } from '../flowgraph.js';

export interface CatalogEntry {
  /** Filename minus `.json` — stable slug for addressing the preset. */
  readonly slug: string;
  /** Human label; falls back to the slug when the doc omits one. */
  readonly label: string;
  readonly description?: string;
  readonly environments: ReadonlyArray<string>;
  /** sigidwiki page URL, drawn as a link in the catalog row. */
  readonly signalWikiUrl?: string;
  /** Resolved (hashed, bundler-emitted) URL of the audio sample, when
   *  the preset declares `sample_path`. The path is looked up against
   *  `samples/sigidwiki/*.{mp3,wav,ogg}` at build time. */
  readonly sampleUrl?: string;
  /** Resolved URL of the spectrum / waterfall image, when the preset
   *  declares `signal_wiki_image`. */
  readonly signalWikiImageUrl?: string;
  readonly doc: FlowgraphDoc;
}

/**
 * Build the catalog from an `import.meta.glob` result, sorted by label.
 * Invalid or structurally-broken JSONs are skipped and collected in
 * `errors` so the caller can surface them in a diagnostics panel.
 */
export function buildCatalog(modules: Readonly<Record<string, unknown>>): {
  entries: CatalogEntry[];
  errors: Array<{ path: string; message: string }>;
} {
  const entries: CatalogEntry[] = [];
  const errors: Array<{ path: string; message: string }> = [];

  for (const [path, mod] of Object.entries(modules)) {
    const slug =
      path
        .split('/')
        .pop()
        ?.replace(/\.json$/, '') ?? path;
    try {
      const raw = extractDefault(mod);
      const doc = typeof raw === 'string' ? (JSON.parse(raw) as unknown) : raw;
      const message = shapeError(doc);
      if (message) {
        errors.push({ path, message });
        continue;
      }
      const d = doc as FlowgraphDoc;
      // Drop headless / node-only presets — they're useful from the
      // CLI (`am-audio-record`, `fm-audio-record`, `capture_fm`) but
      // can't be selected from the browser because they have no
      // AudioSink for the WASM runtime to attach to. Keeping them in
      // the catalog would just confuse users who click them and get
      // silence with no error.
      const envs = d.environments ?? [];
      if (!envs.includes('browser')) continue;
      // Test canaries / diagnostics opt out of the catalog with
      // `catalog_visible: false` so they stay loadable by name from
      // the CLI / `/api/preset` without cluttering the UI list.
      if (d.catalog_visible === false) continue;
      entries.push({
        slug: d.name ?? slug,
        label: d.label ?? d.name ?? slug,
        description: d.description,
        environments: envs,
        signalWikiUrl: d.signal_wiki_url,
        sampleUrl: resolveAsset(AUDIO_URL_BY_REPO_PATH, d.sample_path),
        signalWikiImageUrl: resolveAsset(IMAGE_URL_BY_REPO_PATH, d.signal_wiki_image),
        doc: d,
      });
    } catch (err) {
      errors.push({ path, message: err instanceof Error ? err.message : String(err) });
    }
  }

  entries.sort((a, b) => a.label.localeCompare(b.label));
  return { entries, errors };
}

function shapeError(doc: unknown): string | null {
  if (!doc || typeof doc !== 'object') return 'root is not an object';
  const d = doc as Record<string, unknown>;
  if (typeof d.name !== 'string' || d.name.length === 0) return 'missing required field: name';
  if (!Array.isArray(d.environments) || d.environments.length === 0) {
    return 'missing required field: environments';
  }
  if (!d.blocks || typeof d.blocks !== 'object') return 'missing required field: blocks';
  if (!Array.isArray(d.wires)) return 'missing required field: wires';
  return null;
}

function extractDefault(mod: unknown): unknown {
  if (mod && typeof mod === 'object' && 'default' in mod) {
    return (mod as { default: unknown }).default;
  }
  return mod;
}

// Vite resolves the glob relative to this file — `../../../..` walks up
// from `web/src/lib/presets/` to the repo root, so `flowgraphs/*.json`
// picks up every preset we ship.
const modules = import.meta.glob('../../../../flowgraphs/*.json', {
  eager: true,
});

// Asset URL maps. At build time Vite emits each referenced file under
// `_app/immutable/assets/<hash>.<ext>` and these globs hand us the
// resulting hashed URLs. The maps are keyed by repo-relative path
// (e.g. `samples/sigidwiki/images/Broadcast_FM.jpg`) so a preset's
// `signal_wiki_image` / `sample_path` field can look up directly.
const IMAGE_URL_BY_REPO_PATH = bundleAssetMap(
  import.meta.glob('../../../../samples/sigidwiki/images/*.{jpg,jpeg,png,gif,webp}', {
    eager: true,
    query: '?url',
    import: 'default',
  }),
);
const AUDIO_URL_BY_REPO_PATH = bundleAssetMap(
  import.meta.glob('../../../../samples/sigidwiki/*.{mp3,wav,ogg}', {
    eager: true,
    query: '?url',
    import: 'default',
  }),
);

function bundleAssetMap(mods: Readonly<Record<string, unknown>>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(mods)) {
    // Glob keys look like `../../../../samples/sigidwiki/...` — strip
    // the leading `../` segments so we can key by the same repo-relative
    // path the preset JSON uses.
    out[k.replace(/^(?:\.\.\/)+/, '')] = v as string;
  }
  return out;
}

function resolveAsset(
  map: Readonly<Record<string, string>>,
  repoPath: string | undefined,
): string | undefined {
  if (!repoPath) return undefined;
  return map[repoPath];
}

export const catalog: ReadonlyArray<CatalogEntry> = buildCatalog(modules).entries;
