# Passthru

Real-valued passthrough with a `gain` multiplier. Pure-TS, no WASM —
serves as the reference implementation of the flowgraph-blocks folder
contract (see `../../registry.ts`).

## Files

- `spec.json` — static `BlockSpec`. Hand-written here; Rust-backed
  blocks will get theirs from the `#[ferrite_block]` proc macro
  (commit #33).
- `block.ts` — the `BlockInstance` class.
- `index.ts` — default-exported `BlockModule` = `{ spec, construct }`.
- `README.md` — this file.

## Ports

| direction | name | type       |
| --------- | ---- | ---------- |
| input     | in   | `real_f32` |
| output    | out  | `real_f32` |

## Params

| key  | kind  | default | mutable while streaming |
| ---- | ----- | ------- | ----------------------- |
| gain | range | 1.0     | yes                     |
