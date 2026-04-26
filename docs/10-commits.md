# 10 — Forward commits

History is in `git log`. This file holds the small set of next concrete
commits that follow from the live decisions in
[`09-decisions.md`](09-decisions.md). Decoder-side commits live in
[`docs/decoder-roadmap/`](decoder-roadmap/) instead.

Discipline ([D16](09-decisions.md)): one conceptual change per commit;
every commit green; conventional-commits style with optional scope.

## UX-1 follow-ups (D25 / D26 / D27)

Done:

- [x] Generic block-params pipe (D24), preset registry
      (`GET /api/presets` / `POST /api/preset`), `<BlockParams>` component.
- [x] Spectrum click handlers (D25) — left = VFO, double-click = SDR centre,
      right-click cancels. Wired in `web/src/lib/viz/Spectrum.svelte`.
- [x] Catalog ↔ bands split (D27) — catalog entries carry topology only,
      bands entries carry frequency + optional `vfo_offset_hz`.

Pending:

- [ ] `feat(server): GET /api/source/capabilities sample-rate dropdown wiring`
      — endpoint exists; web-side dropdown still hard-codes choices.

## Decoder-side

See [`docs/decoder-roadmap/`](decoder-roadmap/). Phase 1 (analog
listening) and Phase 2 (multimon-ng vendor) shipped. Phase 3 partially
shipped — ADS-B and APRS land end-to-end against live RF; rtl_433, AIS,
and Mode A/C remain. Commits land in those phase docs as they open.

## How this file stays honest

PRs cross items off (or rewrite them). Nothing here promises post-decision
work; everything has a Dnn anchor in [`09-decisions.md`](09-decisions.md)
or a phase entry in [`docs/decoder-roadmap/`](decoder-roadmap/).
