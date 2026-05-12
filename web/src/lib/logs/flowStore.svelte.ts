// Flow-diag store — consumes the `flowdiag` JSON snapshots the runner
// (browser) and ferrited (node) emit once per second and keeps a
// delta-per-block view for the Flow tab. One store instance holds both
// sides; the UI shows them side-by-side so you can see where in the
// chain sample rate changes or drops.

// Mirrors `ferrite_runtime::DiagSnapshot` on the Rust side. The runtime
// returns a serde-json string; we parse it on receipt and treat these
// as structural types.
export interface DiagPortRaw {
  name: string;
  samples_cum: number;
  buffered: number;
}

export interface DiagBlock {
  id: string;
  type_name: string;
  process_calls_cum: number;
  process_ns_cum: number;
  inputs: DiagPortRaw[];
  outputs: DiagPortRaw[];
}

export interface DiagSnapshot {
  blocks: DiagBlock[];
}

export type Side = 'node' | 'browser';

export interface FlowPortRow {
  name: string;
  sps: number;
  buffered: number;
}

export interface FlowBlockRow {
  id: string;
  typeName: string;
  processPct: number;
  processCalls: number;
  inputs: FlowPortRow[];
  outputs: FlowPortRow[];
}

export interface FlowSide {
  lastUpdate: number;
  windowMs: number;
  blocks: FlowBlockRow[];
}

interface PrevSnap {
  at: number;
  blocks: Map<
    string,
    { processNs: bigint; processCalls: bigint; in: Map<string, bigint>; out: Map<string, bigint> }
  >;
}

class FlowStore {
  node = $state<FlowSide | null>(null);
  browser = $state<FlowSide | null>(null);

  private prev: Record<Side, PrevSnap | null> = { node: null, browser: null };

  ingest(side: Side, snap: DiagSnapshot): void {
    const now = Date.now();
    const prev = this.prev[side];

    // De-dupe: a browser-side flowdiag line is logged locally as it's
    // emitted by the runner AND echoed back from the server through
    // /api/debug/log → server-logs WS, so the same payload arrives
    // twice within milliseconds. The second ingest would see
    // identical `samples_cum` against a prev that's already been
    // bumped to `now`, producing delta=0 → sps=0 across every port
    // and flickering the table to "—" once a second. Drop the
    // duplicate by comparing the snapshot's `process_calls_cum`
    // signature — those increase monotonically tick-by-tick, so an
    // unchanged signature means we've seen this snapshot already.
    if (prev && snapshotMatchesPrev(snap, prev)) {
      return;
    }

    const current: PrevSnap = {
      at: now,
      blocks: new Map(
        snap.blocks.map((b) => [
          b.id,
          {
            processNs: BigInt(b.process_ns_cum),
            processCalls: BigInt(b.process_calls_cum),
            in: new Map(b.inputs.map((p) => [p.name, BigInt(p.samples_cum)])),
            out: new Map(b.outputs.map((p) => [p.name, BigInt(p.samples_cum)])),
          },
        ]),
      ),
    };
    this.prev[side] = current;

    const windowMs = prev ? now - prev.at : 0;
    if (!prev || windowMs <= 0) {
      // First snapshot seen — seed the prev cache, publish blocks with
      // zeros so the table structure is visible immediately.
      const blocks: FlowBlockRow[] = snap.blocks.map((b) => ({
        id: b.id,
        typeName: b.type_name,
        processPct: 0,
        processCalls: 0,
        inputs: b.inputs.map((p) => ({ name: p.name, sps: 0, buffered: p.buffered })),
        outputs: b.outputs.map((p) => ({ name: p.name, sps: 0, buffered: p.buffered })),
      }));
      const view: FlowSide = { lastUpdate: now, windowMs: 0, blocks };
      if (side === 'node') this.node = view;
      else this.browser = view;
      return;
    }

    const windowNs = BigInt(windowMs) * 1_000_000n;
    const blocks: FlowBlockRow[] = snap.blocks.map((b) => {
      const prevBlock = prev.blocks.get(b.id);
      const nowProcessNs = BigInt(b.process_ns_cum);
      const nowCalls = BigInt(b.process_calls_cum);
      const deltaNs = prevBlock ? nowProcessNs - prevBlock.processNs : 0n;
      const deltaCalls = prevBlock ? nowCalls - prevBlock.processCalls : 0n;
      // % wall-clock in process() = deltaNs / windowNs.
      const processPct = windowNs > 0n ? Number((deltaNs * 10000n) / windowNs) / 100 : 0;
      const secs = windowMs / 1000;
      const inputs = b.inputs.map((p) => {
        const prevVal = prevBlock?.in.get(p.name) ?? BigInt(p.samples_cum);
        const delta = BigInt(p.samples_cum) - prevVal;
        return { name: p.name, sps: Number(delta) / secs, buffered: p.buffered };
      });
      const outputs = b.outputs.map((p) => {
        const prevVal = prevBlock?.out.get(p.name) ?? BigInt(p.samples_cum);
        const delta = BigInt(p.samples_cum) - prevVal;
        return { name: p.name, sps: Number(delta) / secs, buffered: p.buffered };
      });
      return {
        id: b.id,
        typeName: b.type_name,
        processPct,
        processCalls: Number(deltaCalls),
        inputs,
        outputs,
      };
    });
    const view: FlowSide = { lastUpdate: now, windowMs, blocks };
    if (side === 'node') this.node = view;
    else this.browser = view;
  }

  clear(): void {
    this.node = null;
    this.browser = null;
    this.prev = { node: null, browser: null };
  }
}

/**
 * Cheap structural comparison between an inbound snapshot and the
 * previously-stored one. We only need to detect "this is the same
 * tick we just ingested," not full equality — the runtime's
 * `process_calls_cum` advances every tick on every block that
 * actually ticked, so comparing the cumulative call count for each
 * known block is a sound shortcut. If the block list itself shifts
 * (env-split changed shape) we treat that as a fresh snapshot.
 */
function snapshotMatchesPrev(snap: DiagSnapshot, prev: PrevSnap): boolean {
  if (snap.blocks.length !== prev.blocks.size) return false;
  for (const b of snap.blocks) {
    const prevBlock = prev.blocks.get(b.id);
    if (!prevBlock) return false;
    if (BigInt(b.process_calls_cum) !== prevBlock.processCalls) return false;
  }
  return true;
}

export const flow = new FlowStore();

// Parse the `flowdiag side=… {json}` line format emitted on both sides.
// Server-side lines have a `target: ` prefix inserted by the tracing
// broadcast layer and may carry trailing structured fields (e.g.
// `source="client"` when the browser-side line round-trips through
// /api/debug/log). The regex finds the marker anywhere in the line
// and grabs the JSON object greedily — `\{.*\}` extends to the LAST
// `}` in the string, so any trailing tracing fields are tolerated as
// long as they don't contain a `}` themselves (none of ours do).
const FLOWDIAG_RE = /flowdiag\s+side=(node|browser)\s+(\{.*\})/s;

export function parseFlowdiagLine(text: string): { side: Side; snap: DiagSnapshot } | null {
  const m = FLOWDIAG_RE.exec(text);
  if (!m) return null;
  try {
    const snap = JSON.parse(m[2]) as DiagSnapshot;
    return { side: m[1] as Side, snap };
  } catch {
    return null;
  }
}
