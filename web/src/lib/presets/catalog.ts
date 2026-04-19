// Loader for the Signal Catalog — enumerates flowgraph presets shipped
// under `flowgraphs/` at the repo root and exposes a filtered,
// structurally-valid list to the UI.
//
// Presets are imported via `import.meta.glob` with `eager: true`, so the
// catalog is resolved at build time and embedded in the bundle. A later
// commit will trade this for a server-fetched list once users can save
// their own flowgraphs; the panel API stays the same.

import { parseFlowgraph, type FlowgraphDoc } from '@ferrite/flowgraph-runtime';

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
      const text = typeof raw === 'string' ? raw : JSON.stringify(raw);
      const { doc, warnings } = parseFlowgraph(text);
      if (warnings.length > 0) {
        errors.push({ path, message: warnings.map((w) => w.error).join('; ') });
        continue;
      }
      entries.push({
        slug: doc.name ?? slug,
        label: doc.label ?? doc.name ?? slug,
        description: doc.description,
        environments: doc.environments,
        doc,
      });
    } catch (err) {
      errors.push({ path, message: err instanceof Error ? err.message : String(err) });
    }
  }

  entries.sort((a, b) => a.label.localeCompare(b.label));
  return { entries, errors };
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
