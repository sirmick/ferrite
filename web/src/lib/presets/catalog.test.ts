import { describe, expect, it } from 'vitest';

import { buildCatalog } from './catalog';

const wbfmDoc = {
  $schema: 'https://ferrite.example/flowgraph-v1.json',
  name: 'wbfm',
  label: 'WBFM broadcast',
  description: 'stereo WBFM demod',
  environments: ['browser'],
  blocks: {
    src: { type: 'WsBridgeRx', params: { streamId: 2, bufferFloats: 131072 } },
    audio: { type: 'AudioSink', params: { bufferSamples: 8192 } },
  },
  wires: [['src.out', 'audio.in']],
};

describe('buildCatalog', () => {
  it('loads valid presets and sorts by label', () => {
    const aDoc = { ...wbfmDoc, name: 'aaa', label: 'AAA' };
    const modules = {
      '/flowgraphs/wbfm.json': { default: wbfmDoc },
      '/flowgraphs/aaa.json': { default: aDoc },
    };
    const { entries, errors } = buildCatalog(modules);
    expect(errors).toEqual([]);
    expect(entries.map((e) => e.slug)).toEqual(['aaa', 'wbfm']);
    expect(entries[0].label).toBe('AAA');
    expect(entries[1].description).toBe('stereo WBFM demod');
  });

  it('falls back to slug when label missing', () => {
    const noLabel = { ...wbfmDoc, label: undefined };
    const { entries } = buildCatalog({
      '/flowgraphs/wbfm.json': { default: noLabel },
    });
    expect(entries[0].label).toBe('wbfm');
  });

  it('collects errors for invalid JSON without throwing', () => {
    // Missing required `name` triggers a shape-phase validation error.
    const broken = { ...wbfmDoc, name: undefined };
    const { entries, errors } = buildCatalog({
      '/flowgraphs/wbfm.json': { default: wbfmDoc },
      '/flowgraphs/broken.json': { default: broken },
    });
    expect(entries.map((e) => e.slug)).toEqual(['wbfm']);
    expect(errors).toHaveLength(1);
    expect(errors[0].path).toBe('/flowgraphs/broken.json');
  });

  it('accepts raw-string modules (as JSON text)', () => {
    const { entries, errors } = buildCatalog({
      '/flowgraphs/wbfm.json': JSON.stringify(wbfmDoc),
    });
    expect(errors).toEqual([]);
    expect(entries[0].slug).toBe('wbfm');
  });
});
