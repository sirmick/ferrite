// Passthru — real_f32 → real_f32 with a gain knob.
//
// This block exists to exercise the flowgraph-blocks folder contract
// end-to-end in pure TS, without pulling in the WASM toolchain. It
// mirrors the Rust `Passthru` fixture used in `blocks/src/block.rs`
// tests so native ↔ browser runtime parity has a trivial starting case.

import type {
  BlockInstance,
  BlockIo,
  InitCtx,
  Work,
} from "@ferrite/flowgraph-runtime/types";

export interface PassthruParams {
  /** Multiplier applied to every sample. Defaults to 1.0. */
  gain?: number;
}

export class Passthru implements BlockInstance {
  #gain: number;

  constructor(params: PassthruParams = {}) {
    this.#gain = params.gain ?? 1.0;
  }

  init(_ctx: InitCtx): void {
    // no precomputation needed
  }

  process(io: BlockIo): Work {
    const src = io.input("in");
    const dst = io.output("out");
    if (!src || !dst) {
      return { consumed: [0], produced: [0] };
    }
    const from = src.data as Float32Array;
    const to = dst.data as Float32Array;
    const n = Math.min(from.length, to.length);
    const g = this.#gain;
    for (let i = 0; i < n; i++) {
      to[i] = from[i]! * g;
    }
    return { consumed: [n], produced: [n] };
  }

  stop(): void {
    // idempotent; nothing to release
  }

  update(params: unknown): void {
    const p = (params ?? {}) as PassthruParams;
    if (typeof p.gain === "number" && Number.isFinite(p.gain)) {
      this.#gain = p.gain;
    }
  }
}
