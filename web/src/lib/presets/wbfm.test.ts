// Full-instantiation smoke for the shipped wbfm preset. Structural
// validation lives in flowgraph-runtime; this test wires up the real
// browser block registry (WsIqSource, AudioSink, plus the DSP blocks
// from @ferrite/flowgraph-blocks) and confirms the preset passes
// every registry-dependent phase — including wire_type_match, which
// is what bit earlier versions of the preset.

import wbfmJson from '../../../../flowgraphs/wbfm.json';
import { registerAll } from '@ferrite/flowgraph-blocks';
import { instantiateFlowgraph } from '@ferrite/flowgraph-runtime';
import type { FlowgraphDoc } from '@ferrite/flowgraph-runtime/types';
import { describe, expect, it } from 'vitest';

import { createWebBlockRegistry } from '../runner/blockRegistry.js';
import type { FrameClient } from '../ws/client.js';

function fakeClient(): FrameClient {
  return {
    subscribe: () => () => {},
    send: () => {},
    close: () => {},
    isOpen: () => false,
    onOpen: () => () => {},
    onClose: () => () => {},
  } as unknown as FrameClient;
}

describe('wbfm preset', () => {
  it('instantiates against the full browser registry', () => {
    const reg = createWebBlockRegistry(fakeClient());
    registerAll(reg);
    const graph = instantiateFlowgraph(wbfmJson as unknown as FlowgraphDoc, reg, 'browser');
    expect([...graph.instances.keys()].sort()).toEqual(['audio', 'decim', 'demod', 'src']);
  });
});
