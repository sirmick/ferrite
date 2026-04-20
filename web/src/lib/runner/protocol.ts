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
  | { readonly id: number; readonly kind: 'state' };

/**
 * Result of a successful `load`: every instantiated block id the graph
 * carries, plus the subset that own a `SharedArrayBuffer` the main
 * thread needs to forward to an `AudioWorkletNode` (keyed by block id).
 */
export interface LoadResult {
  readonly blocks: ReadonlyArray<string>;
  readonly audioSabs: Readonly<Record<string, SharedArrayBuffer>>;
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
  | { readonly id: number; readonly ok: false; readonly error: string };
