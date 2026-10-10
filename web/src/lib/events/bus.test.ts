import { describe, expect, it } from 'vitest';

import { EventBus, topicMatches, type MqttLike } from './bus';

type Handlers = {
  connect: Array<() => void>;
  close: Array<() => void>;
  message: Array<(topic: string, payload: Uint8Array, packet: { retain?: boolean }) => void>;
};

/** Minimal MQTT.js stand-in: records broker-side subscriptions and lets
 *  the test push messages / connection events. */
class FakeClient implements MqttLike {
  connected = false;
  readonly subscribed: string[] = [];
  readonly unsubscribed: string[] = [];
  readonly published: Array<[string, string]> = [];
  ended = 0;
  private readonly h: Handlers = { connect: [], close: [], message: [] };

  on(event: 'connect', cb: () => void): unknown;
  on(event: 'close', cb: () => void): unknown;
  on(
    event: 'message',
    cb: (topic: string, payload: Uint8Array, packet: { retain?: boolean }) => void,
  ): unknown;
  on(event: keyof Handlers, cb: unknown): unknown {
    (this.h[event] as unknown[]).push(cb);
    return this;
  }
  subscribe(filter: string): unknown {
    this.subscribed.push(filter);
    return this;
  }
  unsubscribe(filter: string): unknown {
    this.unsubscribed.push(filter);
    return this;
  }
  publish(topic: string, payload: string): unknown {
    this.published.push([topic, payload]);
    return this;
  }
  end(): unknown {
    this.ended += 1;
    return this;
  }

  // test drivers
  open(): void {
    this.connected = true;
    for (const cb of this.h.connect) cb();
  }
  drop(): void {
    this.connected = false;
    for (const cb of this.h.close) cb();
  }
  deliver(topic: string, payload: string, retain = false): void {
    const bytes = new TextEncoder().encode(payload);
    for (const cb of this.h.message) cb(topic, bytes, { retain });
  }
}

describe('topicMatches', () => {
  it('matches exact, + and # filters per the MQTT rules', () => {
    expect(topicMatches('a/b', 'a/b')).toBe(true);
    expect(topicMatches('a/+', 'a/b')).toBe(true);
    expect(topicMatches('a/+', 'a/b/c')).toBe(false);
    expect(topicMatches('a/#', 'a/b/c')).toBe(true);
    // Per the spec `a/#` also matches the parent `a` itself.
    expect(topicMatches('a/#', 'a')).toBe(true);
    expect(topicMatches('a/#', 'b')).toBe(false);
    expect(topicMatches('ferrite/state/log/#', 'ferrite/state/log/decoder/pocsag')).toBe(true);
    expect(topicMatches('ferrite/state/log/#', 'ferrite/state/adsb')).toBe(false);
    expect(topicMatches('ferrite/state/+', 'ferrite/state/adsb/abc')).toBe(false);
  });
});

describe('EventBus', () => {
  it('routes each message to every local filter that matches', () => {
    const bus = new EventBus();
    const client = new FakeClient();
    bus.attach(client);
    const all: string[] = [];
    const logsOnly: string[] = [];
    bus.subscribe('ferrite/state/#', (m) => all.push(m.topic));
    bus.subscribe('ferrite/state/log/#', (m) => logsOnly.push(m.payload));
    client.open();
    client.deliver('ferrite/state/adsb', '{"kind":"adsb"}');
    client.deliver('ferrite/state/log/decoder/pocsag', '{"message":"hi"}');
    expect(all).toEqual(['ferrite/state/adsb', 'ferrite/state/log/decoder/pocsag']);
    expect(logsOnly).toEqual(['{"message":"hi"}']);
  });

  it('holds one broker subscription for the state tree and adds only uncovered filters', () => {
    const bus = new EventBus();
    const client = new FakeClient();
    bus.attach(client);
    bus.subscribe('ferrite/state/log/#', () => {});
    bus.subscribe('ferrite/state/adsb', () => {});
    const off = bus.subscribe('ferrite/ingest/#', () => {});
    client.open();
    // Root + the one filter outside it; the two inside the root add
    // nothing broker-side (a second broker filter would duplicate
    // every delivery).
    expect(client.subscribed).toEqual(['ferrite/state/#', 'ferrite/ingest/#']);
    off();
    expect(client.unsubscribed).toEqual(['ferrite/ingest/#']);
  });

  it('fires onConnect for late listeners immediately and again on reconnect', () => {
    const bus = new EventBus();
    const client = new FakeClient();
    bus.attach(client);
    client.open();
    let n = 0;
    bus.onConnect(() => n++);
    expect(n).toBe(1);
    client.drop();
    expect(bus.connected).toBe(false);
    client.open();
    expect(n).toBe(2);
    expect(bus.connected).toBe(true);
  });

  it('decodes payload bytes and surfaces the retain flag', () => {
    const bus = new EventBus();
    const client = new FakeClient();
    bus.attach(client);
    const got: Array<[string, boolean]> = [];
    bus.subscribe('ferrite/state/graph', (m) => got.push([m.payload, m.retain]));
    client.open();
    client.deliver('ferrite/state/graph', '{"a":1}', true);
    client.deliver('ferrite/state/graph', '', true);
    expect(got).toEqual([
      ['{"a":1}', true],
      ['', true],
    ]);
  });

  it('detach ends the client', () => {
    const bus = new EventBus();
    const client = new FakeClient();
    bus.attach(client);
    bus.detach();
    expect(client.ended).toBe(1);
    expect(bus.connected).toBe(false);
  });
});
