// Flowgraph runtime — public types.
//
// The full runtime (block registry, wiring, scheduler, lifecycle) lands in
// Phase D (see `docs/10-commits.md`). This file reserves the type shapes
// that both the browser and the Node sidecar will consume.
//
// The block-side contract (BlockSpec, ParamSpec, BlockInstance) mirrors the
// Rust definitions in `blocks/src/block.rs`. Names follow TS convention
// (camelCase) rather than Rust's snake_case — the `#[ferrite_block]` proc
// macro (commit #33) will emit the TS-shaped `spec.json` directly so the
// two sides cannot drift.

export type PortType =
  | "iq_f32"
  | "iq_s16"
  | "real_f32"
  | "real_i16"
  | "fft_f32"
  | "fft_u8"
  | "bits"
  | "frames"
  | "events";

export type Environment = "browser" | "node";

export type Placement = "either" | "native_only" | "wasm_only";

export interface PortSpec {
  readonly name: string;
  readonly portType: PortType;
}

export type ParamKind =
  | {
      readonly kind: "range";
      readonly min: number;
      readonly max: number;
      readonly step: number;
      readonly default: number;
      readonly unit: string;
    }
  | {
      readonly kind: "enum_numeric";
      readonly values: ReadonlyArray<number>;
      readonly default: number;
      readonly unit: string;
    }
  | {
      readonly kind: "enum_string";
      readonly values: ReadonlyArray<string>;
      readonly default: string;
    }
  | { readonly kind: "toggle"; readonly default: boolean }
  | { readonly kind: "text"; readonly default: string };

export interface ParamSpec {
  readonly key: string;
  readonly label: string;
  readonly kind: ParamKind;
  readonly mutableWhileStreaming: boolean;
}

export interface BlockSpec {
  readonly typeName: string;
  readonly placement: Placement;
  readonly inputs: ReadonlyArray<PortSpec>;
  readonly outputs: ReadonlyArray<PortSpec>;
  readonly params: ReadonlyArray<ParamSpec>;
}

/**
 * Sample-rate metadata carried on a port during one `process` call.
 * Mirrors `PortMeta` in `blocks/src/block.rs`.
 */
export interface PortMeta {
  readonly sampleRateHz: number;
  readonly centerFreqHz: number;
}

/**
 * Negotiated rates + per-call frame budget handed to `BlockInstance.init`.
 * The concrete shape of per-port meta lookup is scheduler-owned; this
 * interface is placeholder until Phase D lands the scheduler.
 */
export interface InitCtx {
  readonly frameHint: number;
  inputRate(portName: string): number | undefined;
  outputRate(portName: string): number | undefined;
}

/**
 * Items consumed per input port and produced per output port on one
 * `process` call. Arrays are indexed in port declaration order (same
 * contract as the Rust `Work` struct).
 */
export interface Work {
  readonly consumed: ReadonlyArray<number>;
  readonly produced: ReadonlyArray<number>;
}

/**
 * One port's buffer handle during a process call. The concrete array type
 * (`Float32Array`, `Int16Array`, …) depends on `portType` and is validated
 * by the scheduler before the block sees it.
 */
export interface PortBuf {
  readonly name: string;
  readonly portType: PortType;
  readonly meta: PortMeta;
  readonly data: ArrayBufferView;
}

/**
 * Typed I/O handed to a block during one `process` call. The concrete
 * buffer representation is scheduler-side detail that arrives with Phase
 * D; this interface is the minimum a block needs to address its ports.
 */
export interface BlockIo {
  readonly inputs: ReadonlyArray<PortBuf>;
  readonly outputs: ReadonlyArray<PortBuf>;
  input(name: string): PortBuf | undefined;
  output(name: string): PortBuf | undefined;
}

/**
 * The DSP block trait, TS side. Mirrors `trait Block` in
 * `blocks/src/block.rs`. Blocks are constructed from JSON params via
 * `BlockModule.construct` and driven by the scheduler.
 */
export interface BlockInstance {
  /** One-time wiring — runs after construction, before any samples flow. */
  init(ctx: InitCtx): Promise<void> | void;

  /** One scheduling tick. Reads inputs, writes outputs, reports `Work`. */
  process(io: BlockIo): Work;

  /** Idempotent release. Optional — default is a no-op. */
  stop?(): Promise<void> | void;

  /**
   * Apply a runtime param update. The block deserializes `params` into
   * its own param struct and applies whatever fields it recognises.
   * Only fields flagged `mutableWhileStreaming` in the spec are expected
   * to arrive here; others require a flowgraph restart.
   */
  update?(params: unknown): void;
}

export interface FlowgraphDoc {
  readonly $schema?: string;
  readonly name: string;
  readonly label?: string;
  readonly description?: string;
  readonly environments: ReadonlyArray<Environment>;
  readonly blocks: Readonly<Record<string, BlockInstanceDecl>>;
  readonly wires: ReadonlyArray<Wire>;
}

/** JSON declaration of a block inside a flowgraph doc. */
export interface BlockInstanceDecl {
  readonly type: string;
  readonly params?: Readonly<Record<string, unknown>>;
}

/** Wire endpoints are strings of the form "instance_id.port_name". */
export type Wire = readonly [string, string];

export interface ValidationError {
  readonly phase:
    | "shape"
    | "env_match"
    | "params"
    | "wire_endpoints"
    | "wire_type_match"
    | "fan"
    | "dag"
    | "connectivity"
    | "rate_negotiation";
  readonly error: string;
  readonly wire?: Wire;
  readonly block?: string;
  readonly port?: string;
}
