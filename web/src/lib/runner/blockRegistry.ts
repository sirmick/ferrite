// Web block registry — the set of blocks the browser-side flowgraph
// runner knows how to construct.
//
// The `FrameClient` dependency is injected: every WsIqSource built in
// this graph shares the same WebSocket (one connection per session,
// see `ws/client.ts`). The registry is built once per runtime load.

import { BlockRegistry } from '@ferrite/flowgraph-blocks';
import type { BlockSpec } from '@ferrite/flowgraph-runtime/types';

import { AudioSink } from '../audio/audioSinkBlock.js';
import type { FrameClient } from '../ws/client.js';
import { WsIqSource } from '../ws/wsIqSourceBlock.js';

const WS_IQ_SOURCE_SPEC: BlockSpec = {
  typeName: 'WsIqSource',
  placement: 'wasm_only',
  inputs: [],
  outputs: [{ name: 'out', portType: 'iq_f32' }],
  params: [
    {
      key: 'streamId',
      label: 'Stream ID',
      kind: { kind: 'range', min: 0, max: 65535, step: 1, default: 2, unit: '' },
      mutableWhileStreaming: false,
    },
    {
      key: 'bufferFloats',
      label: 'Ring capacity',
      kind: {
        kind: 'range',
        min: 1024,
        max: 1048576,
        step: 1,
        default: 131072,
        unit: 'floats',
      },
      mutableWhileStreaming: false,
    },
  ],
};

const AUDIO_SINK_SPEC: BlockSpec = {
  typeName: 'AudioSink',
  placement: 'wasm_only',
  inputs: [{ name: 'in', portType: 'real_f32' }],
  outputs: [],
  params: [
    {
      key: 'bufferSamples',
      label: 'Ring capacity',
      kind: {
        kind: 'range',
        min: 256,
        max: 65536,
        step: 1,
        default: 8192,
        unit: 'samples',
      },
      mutableWhileStreaming: false,
    },
  ],
};

interface WsIqSourceConstructParams {
  readonly streamId: number;
  readonly bufferFloats?: number;
}

interface AudioSinkConstructParams {
  readonly bufferSamples?: number;
}

/**
 * Build a `BlockRegistry` wired to the given `FrameClient`. Every
 * `WsIqSource` in the resulting graph will share that client, so a
 * single WebSocket multiplexes all VFO streams.
 */
export function createWebBlockRegistry(client: FrameClient): BlockRegistry {
  const reg = new BlockRegistry();
  reg.add({
    spec: WS_IQ_SOURCE_SPEC,
    construct: (params) => {
      const p = (params ?? {}) as WsIqSourceConstructParams;
      return new WsIqSource({
        client,
        streamId: p.streamId,
        bufferFloats: p.bufferFloats,
      });
    },
  });
  reg.add({
    spec: AUDIO_SINK_SPEC,
    construct: (params) => {
      const p = (params ?? {}) as AudioSinkConstructParams;
      return new AudioSink({ bufferSamples: p.bufferSamples });
    },
  });
  return reg;
}
