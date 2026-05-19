# 14 — Catalog variants, bandplan hard-refs, config validator

Status: **Phases 1, 2, 4 shipped** (design locked 2026-05-18; Phase 3
— bandplan hard-refs — pending). Supersedes the flat catalog +
`presetSlugForMode()` lookup.

> **D-V9 (supersedes D-V2), 2026-05-18.** The block-owned *variant
> taxonomy* idea is dropped. Instead each fldigi block **exposes the
> real decode knobs as typed params** (`baud`/`shift`, `tones`/
> `bandwidth`, `speed`, `interleave`, …); catalog `variants[]` are
> just presets that pin common param bundles. The block does only a
> thin value→fldigi-index/id *adapter* (fldigi's API is index/id
> based), never a curated table. Render: a variant family is one
> expandable **parent row inside its category** owning the shared
> thumbnail/sample; variants nest as the load targets (default
> marked). Singletons stay flat rows. `category` is kept (outer
> grouping). Slug `${name}-${id}`, hard cut; the base is not loadable
> bare. Phase 4 shipped: RttyDemod (`baud`/`shift`), Psk31Demod
> (`baud` 31/63/125), Mt63Demod (`bandwidth`/`interleave`), Olivia/
> Contestia (`tones`/`bandwidth`), DominoEX (`speed`), Throb
> (`tones`/`extended`), NAVTEX (`standard`, singleton). All 7
> mandatory fldigi e2e decode gates still green (param→id composition
> preserves the proven decode path); NAVTEX e2e remains `#[ignore]`
> (pre-existing lossy-fixture issue, unchanged). Related: `03-blocks.md`
(fldigi blocks), `04-flowgraphs.md` (preset schema),
`decoder-roadmap/05-phase-5-ham-digital.md`.

## Problem

1. The fldigi per-mode blocks expose only `afc` / `rx_freq_hz` /
   `reverse` — the decode-relevant fldigi knobs (RTTY shift/baud,
   DominoEX FEC, CW RX-speed bracket, Olivia/Contestia tone×BW, …) are
   not reachable even though the shim's generic CONFIG_LIST passthrough
   already supports every TAG.
2. `RttyDemod` hardcodes `make_modem("rtty45")` with no `variant` — the
   one classic mode that never got the family treatment. RTTY 50/75/100
   and non-170 shifts are unreachable.
3. The catalog is a flat label-sorted list. No grouping.
4. The Bands panel resolves `+RX` through `presetSlugForMode()`, a
   weak chip→slug table where every HF-data mode falls through to bare
   `USB` (no decoder).
5. No corpus-wide cross-reference checking — dangling sample/image
   paths, bad block types, and out-of-range enum params fail silently
   or only at runtime. (The earlier "4 dark families" claim was a
   measurement error — a line-based grep on pretty-printed JSON; all
   families have `environments` and are valid catalog entries. The
   validator therefore lands green; it is a regression gate, not a
   rescue.)

## Locked design decisions

- **D-V1 Variant model = base + overlay (option B).** One base
  flowgraph per family; a `variants[]` table inside the base doc.
  Variants share the base's sample / thumbnail / sigwiki ref. A
  variant is `{ id, label, default?, patch }`.
- **D-V2 Block-owned variant table.** The Kind-1 (distinct
  `make_modem` id) vs Kind-2 (config-TAG combo, e.g. RTTY baud, Olivia
  tone×BW) split is pushed **into the block**. Each family block
  exposes one `variant: EnumString` of curated, on-air-real names; the
  block owns a private static table `variant → (make_modem id, &[(TAG,
  value)…])`. fldigi's TAG zoo never leaks past the block boundary.
  CONFIG_LIST stops being a public surface.
- **D-V3 `patch` is a generic per-block param override** (not
  variant-only) — used for `variant` and for the genuinely-independent
  curated knobs (AFC, reverse, rx_freq, CW RX-speed bracket) that are
  *not* mode identity.
- **D-V4 Slug = `${name}-${variantId}`, hard cut.** No bare-`${name}`
  back-compat fallback. Every existing reference is renamed and the
  validator enforces it.
- **D-V5 One variant carries `default: true`;** validator asserts the
  base's inline `blocks.*.params` equal the resolved default variant.
  Base and default cannot drift.
- **D-V6 Bandplan = hard refs only.** `presetSlugForMode()` deleted.
  Group-level `preset` default, per-entry override; resolution is
  `entry.preset ?? group.preset`, both explicit slugs. **Every band
  entry must resolve to a real catalog slug** — where the decoder
  family doesn't exist yet, the ref points at the matching audio preset
  (`usb` for HF data, `wbfm`/`nbfm`/`wbam` otherwise). No tune-only
  entries. The `mode` chip is derived from the resolved preset's
  `category`; the `mode` field and `presetSlugForMode()` are removed.
- **D-V7 Validator first.** Lands green, then becomes the gate for
  everything after it.
- **D-V8 RSID is its own category.** `FldigiAuto` is the anti-variant
  (it picks the mode live). One catalog entry under an "Auto (RSID)"
  category, not expanded. Bandplan hard-refs it on mixed-digital
  watering holes.

## The bridge surface (reference)

Two channels in `fldigi_modem_set_param`:

- **Three runtime seams** (special-cased, e2e-proven, already shipped):
  `afc` → `progStatus.afconoff`; `rx_freq_hz` → `set_freq()` + wf stub
  `Carrier()`; `rtty_reverse`/`reverse` → wf stub `Reverse()` (every
  FSK/MFSK re-derives polarity from it, not just RTTY). Plus internal
  `RECEIVERSID` (per-handle `cRsId`).
- **Generic CONFIG_LIST passthrough** — any TAG → `progdefaults` →
  `modem->restart()`. Becomes private to the block variant table
  (D-V2). Curated decode-relevant TAGs: RTTY `RTTYSHIFT`/`RTTYBAUD`/
  `RTTYBITS`/`RTTYPARITY`/`RTTYSTOP`; CW `CWLOWERLIMIT`/`CWUPPERLIMIT`/
  `CWTRACK`/`CWBANDWIDTH`; DominoEX `DOMINOEXFEC`; MT63 `MT638BIT`;
  Olivia/Contestia `*TONES`/`*BW`/`*SINTEG`/`*RESETFEC`; PSK
  `PSKSEARCHRANGE`.

### Variant enumeration (curated, on-air-real)

Kind-1 (id-distinct, table maps to a `make_modem` id): MT63 ×6
(`mt63-{500,1000,2000}{S,L}`), Throb ×6 (`throb{1,2,4}`,
`throbx{1,2,4}`), DominoEX ×6 (`dominoex{4,8,11,16,22,44}`), PSK ×3
(`psk{31,63,125}` — 63/125 not yet exposed by a block), NAVTEX ×2
(`navtex`/`sitorb`), CW ×1.

Kind-2 (no distinct id — table maps to `MODE_*` + TAGs): RTTY ~4
baud/shift combos (`rtty45/50/75/100` all build `MODE_RTTY`; baud is
`RTTYBAUD`); Olivia/Contestia ~6–8 curated tone×BW each (4 hard
`MODE_OLIVIA_*`/`MODE_CONTESTIA_*` enums, the rest via `*TONES`/`*BW`
on the generic mode). **Curated, not the full grid.**

Post-D-V2 both kinds are indistinguishable from outside: the catalog
patch is always `{demod:{variant:"…"}}`; the validator's only rule is
`patch.variant ∈ block EnumString set`.

## Phased plan

### Phase 1 — Validator + schema (first catch: the 4 dark families)

- Add `category` + `variants[]` to the flowgraph schema
  (`web/src/lib/flowgraph.ts` type, server `FlowgraphDoc`,
  `04-flowgraphs.md`).
- Shared corpus-wide cross-ref validator. Two entry points: a
  `cargo test` (PR gate) and an opt-in boot warn. Checks:
  - block `type` registered (server block-schema registry);
  - param keys/kinds valid; **EnumString value in range**;
  - `wires` reference declared blocks/ports;
  - `sample_path` / `signal_wiki_image` files exist under `samples/`;
  - **base inline params == resolved default variant** (D-V5);
  - slug uniqueness across the expanded corpus (D-V4);
  - `doc.name` matches the filename slug.

  (The `bands.json` → catalog-slug check is **deferred to Phase 3** —
  it validates the `preset` field Phase 3 adds against the expanded
  catalog Phase 2 produces; it cannot run before either exists. The
  validator is extended there.)
- The corpus is already structurally green for these checks, so the
  validator lands as a pure regression gate (no current rot of this
  class). Its value is forward-looking: it gates Phases 2–4 where
  variant expansion multiplies files and slug/asset/enum drift becomes
  likely. Per-family e2e + thumbnail + sample + sigwiki ship-gate still
  applies in Phase 4.

### Phase 2 — Catalog

- Build-time variant expansion in `catalog.ts`; 2-level grouped
  collapsible render in `SignalCatalog.svelte` (mirror the Bands
  panel's collapsible groups).
- Slug `${name}-${variantId}`, hard cut. Shared `patch`-applying
  resolver used by server `/api/preset` and the browser.

### Phase 3 — Bandplan hard refs

- `preset` field on `bands.json` group + entry; delete `mode` and
  `presetSlugForMode()`; chip from resolved preset `category`; `+RX`
  always enabled (every entry resolves). HF-data entries point at
  `usb` until their decoder ships.
- **Extend the validator**: every `bands.json` group/entry resolves to
  a real expanded-catalog slug (D-V6). This check is introduced here
  because it depends on both the Phase 2 catalog and this phase's
  `preset` field.

### Phase 4 — Per-family variant tables + decode knobs

- Implement the block-owned variant table (D-V2) per family. Fix
  `RttyDemod` (gains `variant`; `psk31`→`PskDemod` 31/63/125). Curated
  independent knobs (CW RX-speed bracket, …) as separate params.
- Extend `FldigiAuto::rearm()` to replay the switched variant's table
  after an RSID `switch_mode` (else post-switch modems run knobs at
  fldigi defaults).
- Fold `ft4.json` into the `ft8` base as a variant; flip the
  corresponding bandplan refs `usb` → real decoder. Each family behind
  the ship-gate.

## Tradeoff accepted

The variant set is code + rebuild (incl. `pnpm wasm:build` — block
change), not a pure JSON edit. Consistent with the project-wide
preference for typed-and-validated over loose-data. The variant
catalog *describes* what a block can do; it does not *configure*
fldigi.
