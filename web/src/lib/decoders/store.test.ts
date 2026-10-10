import { describe, expect, it } from 'vitest';

import { EventBus, type MqttLike } from '$lib/events/bus';

import { DecoderMirror, connectDecoders, type Snapshot } from './store.svelte';

function delta(kind: string, seq: number, op: 'add' | 'upsert', data: unknown, key?: string) {
  return JSON.stringify({
    kind,
    seq,
    epoch: 1,
    op,
    record: { seq, epoch: 1, at_ms: 0, ...(key ? { key } : {}), data },
  });
}

class FakeClient implements MqttLike {
  connected = false;
  private connectCbs: Array<() => void> = [];
  private msgCbs: Array<(t: string, p: Uint8Array, pk: { retain?: boolean }) => void> = [];
  on(event: string, cb: unknown): unknown {
    if (event === 'connect') this.connectCbs.push(cb as () => void);
    if (event === 'message') this.msgCbs.push(cb as (typeof this.msgCbs)[number]);
    return this;
  }
  subscribe(): unknown {
    return this;
  }
  unsubscribe(): unknown {
    return this;
  }
  end(): unknown {
    return this;
  }
  open(): void {
    this.connected = true;
    for (const cb of this.connectCbs) cb();
  }
  deliver(topic: string, payload: string): void {
    const b = new TextEncoder().encode(payload);
    for (const cb of this.msgCbs) cb(topic, b, { retain: false });
  }
}

describe('DecoderMirror', () => {
  it('dedups per kind so a coalesced replace record is not dropped by a newer append', () => {
    const m = new DecoderMirror();
    m.replaceSnapshot({ seq: 10, kinds: {} });
    m.ingest({
      topic: 'ferrite/state/pager',
      payload: delta('pager', 12, 'add', { a: 1 }),
      retain: false,
    });
    // Arrives late (coalesced server-side) with a lower seq than the
    // pager record — but it's the newest `readback` we've seen.
    m.ingest({
      topic: 'ferrite/state/readback',
      payload: delta('readback', 11, 'upsert', { gain: 20 }),
      retain: false,
    });
    expect(m.kind('readback').current['current']?.data).toEqual({ gain: 20 });
    expect(m.seq).toBe(12);
    // Anything at or below the snapshot floor is still skipped.
    m.ingest({
      topic: 'ferrite/state/pager',
      payload: delta('pager', 9, 'add', { old: 1 }),
      retain: false,
    });
    expect(m.kind('pager').recent).toHaveLength(1);
  });

  it('ignores log topics and cleared retained entities', () => {
    const m = new DecoderMirror();
    m.replaceSnapshot({ seq: 0, kinds: {} });
    m.ingest({
      topic: 'ferrite/state/log/decoder/pocsag',
      payload: '{"message":"x"}',
      retain: false,
    });
    m.ingest({ topic: 'ferrite/state/adsb/abc', payload: '', retain: true });
    expect(Object.keys(m.kinds)).toEqual([]);
  });

  it('keyed upserts land in current and the capped recent log', () => {
    const m = new DecoderMirror();
    m.replaceSnapshot({ seq: 0, kinds: {} });
    m.ingest({
      topic: 'ferrite/state/adsb',
      payload: delta('adsb', 1, 'upsert', { alt: 100 }, 'abc'),
      retain: false,
    });
    expect(m.kind('adsb').current['abc']?.data).toEqual({ alt: 100 });
    expect(m.kind('adsb').recent).toHaveLength(1);
  });
});

describe('connectDecoders', () => {
  it('buffers deltas until the snapshot lands, then replays with dedup', async () => {
    const bus = new EventBus();
    const client = new FakeClient();
    bus.attach(client);
    const mirror = new DecoderMirror();
    let resolveSnap!: (s: Snapshot) => void;
    const snap = new Promise<Snapshot>((r) => (resolveSnap = r));
    connectDecoders(bus, () => snap, mirror);
    client.open();
    // Live deltas arrive while /api/state is in flight.
    client.deliver('ferrite/state/pager', delta('pager', 5, 'add', { n: 5 }));
    client.deliver('ferrite/state/pager', delta('pager', 7, 'add', { n: 7 }));
    expect(mirror.kind('pager').recent).toHaveLength(0);
    resolveSnap({
      seq: 6,
      kinds: { pager: { recent: [{ seq: 6, at_ms: 0, data: { n: 6 } }], current: {} } },
    });
    await snap;
    await Promise.resolve();
    // seq 5 was covered by the snapshot; seq 7 was not.
    expect(mirror.kind('pager').recent.map((r) => r.seq)).toEqual([6, 7]);
    // After the snapshot, deltas apply directly.
    client.deliver('ferrite/state/pager', delta('pager', 8, 'add', { n: 8 }));
    expect(mirror.kind('pager').recent.map((r) => r.seq)).toEqual([6, 7, 8]);
  });
});
