import { describe, expect, test } from 'vitest';
import type { FlowgraphDoc } from '$lib/flowgraph';
import { RECEIVERS, applyRecipe, detectReceiver, findRecipe } from './receivers';

function baseDoc(demodType: string): FlowgraphDoc {
  return {
    name: 'test',
    environments: ['node', 'browser'],
    blocks: {
      src: {
        type: 'SoapySource',
        params: { args: 'driver=rtlsdr', sample_rate_hz: 2_400_000 },
      },
      demod: {
        type: demodType,
        placement: 'browser',
        params: { sample_rate_hz: 48000 },
      },
      audio: { type: 'AudioSink', params: {} },
    },
    wires: [
      ['src.out', 'demod.in'],
      ['demod.out', 'audio.in'],
    ],
  };
}

describe('receivers', () => {
  test('detect recognises FM and AM demods', () => {
    expect(detectReceiver(baseDoc('FmDemod'))).toBe('fm');
    expect(detectReceiver(baseDoc('AmDemod'))).toBe('am');
    expect(detectReceiver(baseDoc('Something'))).toBeNull();
    expect(detectReceiver(null)).toBeNull();
  });

  test('applyRecipe swaps only the demod block and keeps source byte-identical', () => {
    const doc = baseDoc('FmDemod');
    const am = applyRecipe(doc, findRecipe('am'));
    expect(am).not.toBeNull();
    expect(am?.blocks?.src).toEqual(doc.blocks?.src);
    expect(am?.blocks?.demod?.type).toBe('AmDemod');
    expect(am?.blocks?.demod?.placement).toBe('browser');
    expect(am?.blocks?.demod?.params).toEqual({ sample_rate_hz: 48000, bias_tau_ms: 100 });
    expect(am?.wires).toEqual(doc.wires);
  });

  test('applyRecipe returns null when the doc lacks a demod block', () => {
    const stripped: FlowgraphDoc = {
      name: 'x',
      environments: ['node'],
      blocks: { src: { type: 'SoapySource', params: {} } },
      wires: [],
    };
    expect(applyRecipe(stripped, findRecipe('fm'))).toBeNull();
  });

  test('findRecipe returns definitions for both shipped modes', () => {
    for (const r of RECEIVERS) {
      expect(findRecipe(r.id)).toBe(r);
    }
  });
});
