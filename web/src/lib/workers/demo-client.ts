// Thin client around the demo worker — used by the app shell to prove
// that WASM loads and runs off-main-thread.

export interface DemoAddResult {
  sum: number;
}

export async function demoAddInWorker(a: number, b: number): Promise<DemoAddResult> {
  const worker = new Worker(new URL('./demo-worker.ts', import.meta.url), {
    type: 'module',
  });
  const result = await new Promise<DemoAddResult>((resolve, reject) => {
    worker.onmessage = (event: MessageEvent<{ kind: string; sum: number }>) => {
      if (event.data.kind === 'demo_add:result') {
        resolve({ sum: event.data.sum });
      } else {
        reject(new Error(`unexpected reply: ${event.data.kind}`));
      }
    };
    worker.onerror = (err) => reject(err);
    worker.postMessage({ kind: 'demo_add', a, b });
  });
  worker.terminate();
  return result;
}
