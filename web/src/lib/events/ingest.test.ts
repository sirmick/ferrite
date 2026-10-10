import { describe, expect, it } from 'vitest';

import { EventBus, type MqttLike } from './bus';
import {
  ingestTopic,
  kindForStream,
  publishDecoderLines,
  publishTranscriptSegment,
} from './ingest';

class FakeClient implements MqttLike {
  connected = false;
  readonly published: Array<[string, string]> = [];
  private connectCbs: Array<() => void> = [];
  on(event: string, cb: unknown): unknown {
    if (event === 'connect') this.connectCbs.push(cb as () => void);
    return this;
  }
  subscribe(): unknown {
    return this;
  }
  unsubscribe(): unknown {
    return this;
  }
  publish(topic: string, payload: string): unknown {
    this.published.push([topic, payload]);
    return this;
  }
  end(): unknown {
    return this;
  }
  open(): void {
    this.connected = true;
    for (const cb of this.connectCbs) cb();
  }
}

function connectedBus(): { bus: EventBus; client: FakeClient } {
  const bus = new EventBus();
  const client = new FakeClient();
  bus.attach(client);
  client.open();
  return { bus, client };
}

describe('ingest helpers', () => {
  it('maps a runner stream id back to its ui sink kind', () => {
    const sinks = [
      { name: 'ft8', stream_id: 1003 },
      { name: 'wspr', stream_id: 1004 },
    ];
    expect(kindForStream(sinks, 1004)).toBe('wspr');
    expect(kindForStream(sinks, 7)).toBeNull();
    expect(ingestTopic('/ft8/')).toBe('ferrite/ingest/ft8');
  });

  it('publishes each JSON object line stamped with the epoch, skipping junk', () => {
    const { bus, client } = connectedBus();
    const n = publishDecoderLines(
      'ft8',
      ['{"call":"K1ABC","snr":-12}', '', 'not json', '[1,2]', '{"call":"W2XYZ","epoch":3}'],
      9,
      bus,
    );
    expect(n).toBe(2);
    expect(client.published.map(([t]) => t)).toEqual(['ferrite/ingest/ft8', 'ferrite/ingest/ft8']);
    expect(JSON.parse(client.published[0][1])).toEqual({ call: 'K1ABC', snr: -12, epoch: 9 });
    // An event that already says which epoch it saw keeps its own.
    expect(JSON.parse(client.published[1][1]).epoch).toBe(3);
  });

  it('publishes nothing while the bus is down so callers can fall back', () => {
    const bus = new EventBus();
    const client = new FakeClient();
    bus.attach(client); // never opened
    expect(publishDecoderLines('ft8', ['{"a":1}'], 1, bus)).toBe(0);
    expect(
      publishTranscriptSegment(
        {
          atMs: 1,
          vfoHz: null,
          t0: 0,
          t1: 1,
          text: 'hi',
          confidence: 1,
          noSpeechProb: 0,
          cont: false,
          gapMs: 0,
          speakerTurn: false,
        },
        1,
        bus,
      ),
    ).toBe(false);
    expect(client.published).toHaveLength(0);
  });

  it('publishes a transcript segment as a transcribe record', () => {
    const { bus, client } = connectedBus();
    const ok = publishTranscriptSegment(
      {
        atMs: 123,
        vfoHz: 162_400_000,
        t0: 0.5,
        t1: 2.0,
        text: 'wind north ten knots',
        confidence: 0.9,
        noSpeechProb: 0.05,
        cont: false,
        gapMs: 400,
        speakerTurn: true,
      },
      4,
      bus,
    );
    expect(ok).toBe(true);
    const [topic, payload] = client.published[0];
    expect(topic).toBe('ferrite/ingest/transcribe');
    const rec = JSON.parse(payload);
    expect(rec.text).toBe('wind north ten knots');
    expect(rec.vfoHz).toBe(162_400_000);
    expect(rec.epoch).toBe(4);
    expect(rec.source).toBe('browser');
  });
});
