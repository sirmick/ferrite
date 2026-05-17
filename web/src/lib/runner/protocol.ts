// Runner message protocol — wire format between the main thread and the
// flowgraph runner Worker.
//
// Requests are structured {id, kind, ...payload}. Responses carry the
// same id back; `ok: true` variants are discriminated by the same
// `kind` as the request, so callers can narrow on the tuple. Errors
// collapse into a single `{ok:false, error}` shape so the client can
// reject the pending promise without per-kind branches.
//
// `update` (runtime param tweaks) is intentionally absent — the Rust
// runtime's reconfiguration surface lands with M3 once the
// `reconfigScope` schema is in place.

import type { FlowgraphDoc } from '../flowgraph.js';

export type RuntimeState = 'created' | 'initialized' | 'running' | 'stopped';

export type RunnerRequest =
  | {
      readonly id: number;
      readonly kind: 'load';
      readonly doc: FlowgraphDoc;
      readonly wsUrl: string;
    }
  | { readonly id: number; readonly kind: 'start' }
  | { readonly id: number; readonly kind: 'stop' }
  | { readonly id: number; readonly kind: 'state' }
  | {
      /** Apply a partial params delta to a single block inside the
       *  browser-side runtime. Routes through Rust
       *  `RuntimeHandle.live_reconfigure_block`, so block-scoped hot
       *  params (e.g. `FmDemod.max_deviation_hz`) take effect with no
       *  full-graph reload. Falls back to a block-scoped rebuild when
       *  the block's `apply_live_params` returns false — exactly the
       *  same semantics the server has for its half. */
      readonly id: number;
      readonly kind: 'reconfigureBlock';
      readonly blockId: string;
      readonly delta: Record<string, unknown>;
    };

/**
 * Result of a successful `load`: every instantiated block id the graph
 * carries, plus the subset that own a `SharedArrayBuffer` the main
 * thread needs to forward to an `AudioWorkletNode` (keyed by block id).
 */
export interface LoadResult {
  readonly blocks: ReadonlyArray<string>;
  readonly audioSabs: Readonly<Record<string, SharedArrayBuffer>>;
  /** `VoiceTranscribe` blocks' tap rings, keyed by block id. The main
   *  thread spins up one transcription Worker per entry (Silero VAD +
   *  whisper.cpp) that reads the SAB on its own cadence — the
   *  worklet-less sibling of `audioSabs`. */
  readonly transcribeSabs: Readonly<Record<string, SharedArrayBuffer>>;
  /** Browser-side `ui:<name>` Events sinks the split produced (name +
   *  the stream_id env_split allocated — identical to the node half's
   *  allocation). The main thread merges these into `pipeline.uiSinks`
   *  so the advanced view attaches even when the decoder is
   *  browser-placed and node never advertised the sink. */
  readonly uiSinks: ReadonlyArray<{ readonly name: string; readonly stream_id: number }>;
}

export type RunnerResponse =
  | { readonly id: number; readonly ok: true; readonly kind: 'load'; readonly data: LoadResult }
  | { readonly id: number; readonly ok: true; readonly kind: 'start' | 'stop' }
  | {
      readonly id: number;
      readonly ok: true;
      readonly kind: 'state';
      readonly data: { readonly state: RuntimeState };
    }
  | { readonly id: number; readonly ok: true; readonly kind: 'reconfigureBlock' }
  | { readonly id: number; readonly ok: false; readonly error: string };
