// Minimal structural shape of a flowgraph JSON document, mirroring what
// the Rust runtime accepts. The authoritative schema lives in the Rust
// `ferrite-runtime` crate; this TS declaration is a permissive surface
// the web code uses when passing docs across Worker message boundaries
// and into the RuntimeHandle constructor.

export type Environment = 'node' | 'browser';
export type Placement = 'node' | 'browser' | 'either';

export interface BlockInstanceDecl {
  readonly type: string;
  readonly placement?: Placement;
  readonly params?: Readonly<Record<string, unknown>>;
}

export type Wire = readonly [string, string];

export interface FlowgraphDoc {
  readonly $schema?: string;
  readonly name?: string;
  readonly label?: string;
  readonly description?: string;
  /** Plain-prose preset-level note injected into the ferrite-ai system
   *  prompt when this preset is active. 3–6 sentences answering
   *  "when do I pick this preset? what signal does it expect? any
   *  gotchas?" See `docs/13-ai-notes-migration.md`. */
  readonly ai_notes?: string;
  readonly environments?: ReadonlyArray<Environment>;
  readonly blocks?: Readonly<Record<string, BlockInstanceDecl>>;
  readonly wires?: ReadonlyArray<Wire>;
  /** Optional sigidwiki / Signal Identification Guide URL — drawn as a
   *  ↗ link in the SignalCatalog tile so users can read up on the
   *  protocol that the preset decodes. */
  readonly signal_wiki_url?: string;
  /** Optional path (repo-relative) to a representative audio sample of
   *  the signal, kept under `samples/` so the wrapper's analyzer
   *  binaries can replay against a known-good capture. The
   *  SignalCatalog UI surfaces a "play sample" button when set. */
  readonly sample_path?: string;
  /** Optional path (repo-relative) to a representative spectrum /
   *  waterfall image (typically pulled from the matching sigidwiki
   *  page). The SignalCatalog UI shows it as a thumbnail in the
   *  preset's info pane. */
  readonly signal_wiki_image?: string;
  /** Whether this preset should appear in the user-facing
   *  SignalCatalog. Defaults to `true`. Set `false` on test canaries,
   *  diagnostic flowgraphs, and other internal-only presets so they
   *  stay loadable from the CLI / API without cluttering the UI. */
  readonly catalog_visible?: boolean;
}

/** Canonical `Source` placeholder id + sentinel type. Must match the
 *  Rust constants in `compose.rs`. */
export const SOURCE_ID = 'src';
export const SOURCE_SENTINEL_TYPE = 'Source';

/** Mirror of Rust `compose_source` for client-side callers. Replaces
 *  the `src` placeholder's `"Source"` type with the active source's
 *  real type, merges params (source wins on collisions), and
 *  propagates `sample_rate_hz` into any block that declares
 *  `input_rate_hz`. Used by the browser runtime, which needs a
 *  self-contained doc the wasm runtime can actually instantiate —
 *  `GET /api/flowgraph` returns the preset only. */
export function composeSource(
  preset: FlowgraphDoc,
  source: { type: string; params: Record<string, unknown> },
): FlowgraphDoc {
  const srcBlocks: Record<string, BlockInstanceDecl> = {};
  for (const [k, v] of Object.entries(preset.blocks ?? {})) srcBlocks[k] = v;
  const placeholder = srcBlocks[SOURCE_ID];
  if (!placeholder || placeholder.type !== SOURCE_SENTINEL_TYPE) {
    // Preset has no placeholder — caller handed us something already
    // composed, or a bareband preset. Either way, nothing to do.
    return preset;
  }
  const hints = (placeholder.params ?? {}) as Record<string, unknown>;
  const merged: Record<string, unknown> = { ...hints, ...source.params };
  srcBlocks[SOURCE_ID] = {
    type: source.type,
    placement: placeholder.placement,
    params: merged,
  };
  const rate = merged.sample_rate_hz;
  if (typeof rate === 'number' && rate > 0) {
    for (const [id, block] of Object.entries(srcBlocks)) {
      if (id === SOURCE_ID) continue;
      const p = block.params;
      if (p && typeof p === 'object' && 'input_rate_hz' in p) {
        srcBlocks[id] = {
          ...block,
          params: { ...p, input_rate_hz: rate },
        };
      }
    }
  }
  return { ...preset, blocks: srcBlocks };
}
