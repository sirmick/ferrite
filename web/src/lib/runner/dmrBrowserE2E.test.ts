// Browser-path DMR metadata decode e2e.
//
// `DmrDecode` is pure Rust (Placement::Either), so the same decoder the
// node runs also runs here, in the wasm32 runtime. This feeds the real
// off-air fixture from `blocks/tests/dmr_e2e.rs` (40 s of the 441.850
// MHz repeater as 48 kHz FM-discriminator audio, int8) through a
// *browser* `RuntimeHandle` and expects the calls dsd-fme reports on the
// same audio: slot 2 TG 31068 from 3107998, slot 1 TG 93 from 3228520,
// talker alias KR1JOE, colour code 1.
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { beforeAll, describe, expect, it } from 'vitest';

import { initSync, RuntimeHandle } from '../wasm/runtime/runtime.js';

const RUNTIME_WASM = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');
const FIXTURE = resolve(__dirname, '../../../../blocks/tests/fixtures/dmr_441850_disc48k.s8');
const RATE = 48_000;

const DOC = {
  name: 'dmr-browser-e2e',
  environments: ['browser'],
  blocks: {
    rx: {
      type: 'WsBridgeRxF32',
      placement: 'browser',
      params: { stream_id: 1, buffer_samples: 262_144, sample_rate_hz: RATE },
    },
    dmr: { type: 'DmrDecode', placement: 'browser', params: { sample_rate_hz: RATE } },
    sink: { type: 'EventsSink', placement: 'browser', params: { capacity: 4096 } },
  },
  wires: [
    ['rx.out', 'dmr.in'],
    ['dmr.events', 'sink.in'],
  ],
};

interface DsdRec {
  event: string;
  slot?: number;
  tgt?: string;
  src?: string;
  cc?: number;
  text: string;
}

describe('browser-path DMR decode', () => {
  beforeAll(() => {
    initSync({ module: readFileSync(RUNTIME_WASM) });
  });

  it('decodes the 441.850 MHz fixture in the wasm runtime', () => {
    const raw = readFileSync(FIXTURE);
    const audio = new Float32Array(raw.length);
    for (let i = 0; i < raw.length; i++) audio[i] = ((raw[i] << 24) >> 24) / 127;

    const rt = new RuntimeHandle(JSON.stringify(DOC), 'browser');
    rt.init();
    const CHUNK = 8_192;
    const events: DsdRec[] = [];
    for (let off = 0; off < audio.length; off += CHUNK) {
      rt.pushF32('rx', audio.subarray(off, off + CHUNK));
      for (let t = 0; t < 8; t++) rt.tick();
      for (const e of rt.drainEvents('sink') as string[]) events.push(JSON.parse(e) as DsdRec);
    }
    rt.free();

    const call = (slot: number, tgt: string, src: string) =>
      events.some((e) => e.event === 'call' && e.slot === slot && e.tgt === tgt && e.src === src);
    expect(call(2, '31068', '3107998')).toBe(true);
    expect(call(1, '93', '3228520')).toBe(true);
    expect(events.filter((e) => e.event === 'call').every((e) => e.cc === 1)).toBe(true);
    expect(events.some((e) => e.event === 'alias' && e.text.startsWith('KR1JOE'))).toBe(true);
  });
});
