// Block-module contract + registry.
//
// The contract is deliberately small: every block folder default-exports a
// `BlockModule` with a static `spec`, a `construct` factory, and optional
// `widget` / install hooks. The scheduler never touches this file — it
// consumes `BlockInstance` from `@ferrite/flowgraph-runtime/types`.
//
// See `docs/03-blocks.md` for why blocks are transport/UI-blind and how
// this contract composes with the JSON flowgraph at `docs/04-flowgraphs.md`.

import type {
  BlockInstance,
  BlockSpec,
} from "@ferrite/flowgraph-runtime/types";

/**
 * Reserved for block-specific one-shot setup hooks. The concrete shape
 * lands as the first block needs it (e.g. AudioSink registering its
 * AudioWorklet module on first use).
 */
export type InstallCtx = Readonly<Record<string, never>>;

/**
 * A Svelte-component-shaped placeholder. We type it as `unknown` so this
 * package doesn't take a Svelte dep — the web app casts on import.
 * Shape settles when the first custom widget lands (Phase C / #64).
 */
export type BlockWidget = unknown;

/**
 * The contract every `blocks/<name>/index.ts` must default-export. Keep
 * it narrow: the scheduler instantiates via `construct(params)` and
 * drives `init/process/stop` on the returned `BlockInstance`.
 */
export interface BlockModule {
  readonly spec: BlockSpec;
  construct(params: unknown): BlockInstance;
  readonly widget?: BlockWidget;
  onInstall?(ctx: InstallCtx): Promise<void> | void;
  onUninstall?(ctx: InstallCtx): Promise<void> | void;
}

/**
 * Identity helper that preserves literal types. Use instead of an
 * inline object cast so the compiler catches shape drift at the block
 * file, not at the registry.
 */
export function defineBlock<M extends BlockModule>(mod: M): M {
  return mod;
}

/**
 * In-memory block registry. One instance per runtime (one per Worker).
 * Type-name collisions throw on `add` so late registrations can't
 * silently shadow earlier ones.
 */
export class BlockRegistry {
  readonly #blocks = new Map<string, BlockModule>();

  add(mod: BlockModule): void {
    const name = mod.spec.typeName;
    if (this.#blocks.has(name)) {
      throw new Error(`duplicate block registration: ${name}`);
    }
    this.#blocks.set(name, mod);
  }

  get(typeName: string): BlockModule | undefined {
    return this.#blocks.get(typeName);
  }

  has(typeName: string): boolean {
    return this.#blocks.has(typeName);
  }

  list(): ReadonlyArray<string> {
    return [...this.#blocks.keys()].sort();
  }

  size(): number {
    return this.#blocks.size;
  }
}
