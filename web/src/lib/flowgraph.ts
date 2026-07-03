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
  /** Pre-split gate. When present, `apply_profile` strips this block
   *  (and wires touching it) unless every (key, expected) pair matches
   *  the active runtime profile. Unknown keys don't gate
   *  (forward-compat). Today the only recognized key is `"audio"`. */
  readonly when?: Readonly<Record<string, unknown>>;
  /** Pre-split tag marking this block as part of the audio spine
   *  (`"demod"` / `"audio"` / `"nr"` / `"transcribe"`). The profile's
   *  `audio_split` moves every tagged block to the same side of the
   *  node↔browser cut. */
  readonly placement_role?: string;
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
  /** 2-level catalog grouping key (e.g. "RTTY", "Olivia", "WSPR"). See
   *  `docs/14-fldigi-catalog-variants.md`. Optional during rollout. */
  readonly category?: string;
  /** Catalog variants of this base preset. Empty/absent = a singleton.
   *  Each variant overlays `patch` (`blockId → { paramKey: value }`)
   *  onto the base; the resolved slug is `${name}-${id}`. Exactly one
   *  variant carries `default: true` and its resolved params must
   *  equal the base's inline block params. */
  readonly variants?: ReadonlyArray<VariantDecl>;
}

export interface VariantDecl {
  readonly id: string;
  readonly label?: string;
  readonly default?: boolean;
  readonly patch?: Readonly<Record<string, Record<string, unknown>>>;
}

// The browser runtime runs the server's authoritative *browser-half* doc
// verbatim (`GET /api/flowgraph/browser-half`, mirrored on
// `pipeline.browserHalf`). The old client-side `composeSource` /
// `injectVoiceTranscribe` re-derivation lived here; it diverged from the
// node's env-split under any non-`balanced` audio split and silently
// dropped cross-env audio, so it was deleted — this file is now pure
// flowgraph types shared by the browser code.
