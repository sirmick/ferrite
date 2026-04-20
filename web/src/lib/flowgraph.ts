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
}

export type Wire = readonly [string, string];

export interface FlowgraphDoc {
  readonly $schema?: string;
  readonly name?: string;
  readonly label?: string;
  readonly description?: string;
  readonly environments?: ReadonlyArray<Environment>;
  readonly blocks?: Readonly<Record<string, BlockInstanceDecl>>;
  readonly wires?: ReadonlyArray<Wire>;
}
