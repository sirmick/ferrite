// `GET /api/flowdiag` — the latest node-side flow-diagnostics
// snapshot, a dedicated channel (not the log stream). Returns `null`
// when the pipeline is stopped or hasn't produced one yet. Polled ~1 Hz
// by `FlowPanel` while the Flow tab is open.

import type { DiagSnapshot } from '$lib/logs/flowStore.svelte';

export async function fetchFlowdiag(): Promise<DiagSnapshot | null> {
  const r = await fetch('/api/flowdiag');
  if (!r.ok) return null;
  return (await r.json()) as DiagSnapshot | null;
}
