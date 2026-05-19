import { describe, expect, it } from 'vitest';

import { applyVariantPatch, buildCatalog } from './catalog';
import type { FlowgraphDoc } from '../flowgraph';

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

  it('surfaces category on the entry', () => {
    const { entries } = buildCatalog({
      '/flowgraphs/wbfm.json': { default: { ...wbfmDoc, category: 'Analog voice' } },
    });
    expect(entries[0].category).toBe('Analog voice');
  });

  it('renders a variant family as ONE parent entry with nested children (D-V1/D-V4)', () => {
    const olivia = {
      ...wbfmDoc,
      name: 'olivia',
      label: 'Olivia',
      category: 'Olivia',
      blocks: {
        ...wbfmDoc.blocks,
        demod: { type: 'OliviaDemod', params: { tones: 8, bandwidth: 500, afc: true } },
      },
      variants: [
        { id: '8-500', label: 'Olivia 8/500', default: true, patch: {} },
        { id: '16-500', patch: { demod: { tones: 16, bandwidth: 500 } } },
      ],
    };
    const { entries, docsBySlug } = buildCatalog({
      '/flowgraphs/olivia.json': { default: olivia },
    });
    // ONE parent entry keyed by the base name; not loadable bare.
    expect(entries.map((e) => e.slug)).toEqual(['olivia']);
    const fam = entries[0];
    expect(fam.category).toBe('Olivia');
    expect(fam.variants!.map((v) => v.slug)).toEqual(['olivia-8-500', 'olivia-16-500']);
    expect(fam.variants!.find((v) => v.slug === 'olivia-8-500')!.isDefault).toBe(true);
    // Default child label explicit; the other falls back to `${baseLabel} ${id}`.
    expect(fam.variants![1].label).toBe('Olivia 16-500');
    // The base name is NOT a loadable slug; only the variants are.
    expect(docsBySlug.has('olivia')).toBe(false);
    expect([...docsBySlug.keys()].sort()).toEqual(['olivia-16-500', 'olivia-8-500']);
    expect(
      (docsBySlug.get('olivia-16-500')!.blocks!.demod.params as Record<string, unknown>).tones,
    ).toBe(16);
    // Parent preview doc = the default variant's resolved doc.
    expect((fam.doc.blocks!.demod.params as Record<string, unknown>).tones).toBe(8);
  });

  it('singletons are loadable by their bare slug', () => {
    const { entries, docsBySlug } = buildCatalog({
      '/flowgraphs/wbfm.json': { default: wbfmDoc },
    });
    expect(entries[0].slug).toBe('wbfm');
    expect(entries[0].variants).toBeUndefined();
    expect(docsBySlug.get('wbfm')).toBe(entries[0].doc);
  });
});

describe('applyVariantPatch', () => {
  const base = {
    name: 'b',
    environments: ['browser'],
    blocks: {
      demod: { type: 'OliviaDemod', params: { variant: 'olivia-8-500', afc: true } },
      other: { type: 'X', params: { keep: 1 } },
    },
    wires: [],
  } as unknown as FlowgraphDoc;

  it('shallow-merges per-block params, patch wins, base untouched', () => {
    const out = applyVariantPatch(base, { demod: { variant: 'olivia-16-500' } });
    expect(out.blocks!.demod.params as Record<string, unknown>).toEqual({
      variant: 'olivia-16-500',
      afc: true,
    });
    // Untouched block preserved; base object not mutated.
    expect(out.blocks!.other).toEqual(base.blocks!.other);
    expect((base.blocks!.demod.params as Record<string, unknown>).variant).toBe('olivia-8-500');
  });

  it('ignores patches on unknown blocks (validator D-V5 rejects them)', () => {
    const out = applyVariantPatch(base, { nope: { x: 1 } });
    expect(out.blocks!.nope).toBeUndefined();
  });
});
