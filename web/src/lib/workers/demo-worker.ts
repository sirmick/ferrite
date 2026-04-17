// Demo worker — proves the Rust → wasm-pack → Vite → Worker path.
// Replaced by the real flowgraph runner in Phase D.

import init, { demo_add } from '../wasm/blocks/ferrite_blocks';

interface DemoRequest {
  readonly kind: 'demo_add';
  readonly a: number;
  readonly b: number;
}

interface DemoResponse {
  readonly kind: 'demo_add:result';
  readonly sum: number;
}

let ready: Promise<void> | null = null;

function ensureReady(): Promise<void> {
  ready ??= (async () => {
    await init();
  })();
  return ready;
}

self.onmessage = async (event: MessageEvent<DemoRequest>) => {
  await ensureReady();
  const { a, b } = event.data;
  const response: DemoResponse = { kind: 'demo_add:result', sum: demo_add(a, b) };
  (self as unknown as Worker).postMessage(response);
};
