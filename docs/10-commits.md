# 10 — Forward commits

History is in `git log`. This file holds the small set of next concrete
commits that follow from the live decisions in
[`09-decisions.md`](09-decisions.md). Decoder-side commits live in
[`docs/decoder-roadmap/`](decoder-roadmap/) instead.

Discipline ([D16](09-decisions.md)): one conceptual change per commit;
every commit green; conventional-commits style with optional scope.

## UX-1 follow-ups (D25 / D26)

The generic block-params pipe (D24), preset registry (`GET /api/presets` /
`POST /api/preset`), `<BlockParams>` component, and bands.json `preset`
field are in place. Items still pending from the UX-1 cluster:

- [ ] `feat(web): spectrum click handlers — left = VFO, right = SDR centre`
      — D25. One-liners on `setBlockParam` once the click hit-testing is
      wired through the fixed spectrum-over-waterfall layout.
- [ ] `feat(server): GET /api/source/capabilities sample-rate dropdown wiring`
      — endpoint exists; web-side dropdown still hard-codes choices.

## Decoder-side

See [`docs/decoder-roadmap/`](decoder-roadmap/) for the analog-listening
helper blocks (Phase 1) and the first vendored C core (Phase 2,
multimon-ng). Commits land in those phase docs as they open.

## How this file stays honest

PRs cross items off (or rewrite them). Nothing here promises post-decision
work; everything has a Dnn anchor in [`09-decisions.md`](09-decisions.md)
or a phase entry in [`docs/decoder-roadmap/`](decoder-roadmap/).
