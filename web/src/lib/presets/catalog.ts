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
      entries.push({
        slug: d.name ?? slug,
        label: d.label ?? d.name ?? slug,
        description: d.description,
        environments: envs,
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

export const catalog: ReadonlyArray<CatalogEntry> = buildCatalog(modules).entries;
