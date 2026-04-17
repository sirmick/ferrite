// Flowgraph runtime — public types.
//
// The full runtime (block registry, wiring, scheduler, lifecycle) lands in
// Phase D (see `docs/10-commits.md`). This file reserves the type shapes
// that both the browser and the Node sidecar will consume.

export type PortType =
  | 'iq_f32'
  | 'iq_s16'
  | 'real_f32'
  | 'real_i16'
  | 'fft_f32'
  | 'bits'
  | 'frames'
  | 'events';

export type Environment = 'browser' | 'node';

export interface FlowgraphDoc {
  readonly $schema?: string;
  readonly name: string;
  readonly label?: string;
  readonly description?: string;
  readonly environments: ReadonlyArray<Environment>;
  readonly blocks: Readonly<Record<string, BlockInstance>>;
  readonly wires: ReadonlyArray<Wire>;
}

export interface BlockInstance {
  readonly type: string;
  readonly params?: Readonly<Record<string, unknown>>;
}

/** Wire endpoints are strings of the form "instance_id.port_name". */
export type Wire = readonly [string, string];

export interface ValidationError {
  readonly phase:
    | 'shape'
    | 'env_match'
    | 'params'
    | 'wire_endpoints'
    | 'wire_type_match'
    | 'fan'
    | 'dag'
    | 'connectivity'
    | 'rate_negotiation';
  readonly error: string;
  readonly wire?: Wire;
  readonly block?: string;
  readonly port?: string;
}
