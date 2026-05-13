import { describe, expect, test } from 'vitest';
import type { FlowgraphDoc } from '$lib/flowgraph';
import { predictScope, type BlockSchema, type ParamSchema, type ReconfigScope } from './flowgraph';
import { applyRecipe, findRecipe } from '$lib/presets/receivers';

// Minimal block-schema fixtures matching the `reconfig_scope` choices
// declared in `blocks/src/*.rs`. If those scopes shift, these fixtures
// (and the expectations below) must move in lockstep — which is
// exactly what these tests guard.
const RANGE_SELF = (key: string, scope: ReconfigScope): ParamSchema => ({
  key,
  label: key,
  reconfig_scope: scope,
  ai_notes: '',
  kind: 'range',
  min: 0,
  max: 1e9,
  step: 1,
  default: 0,
  unit: '',
});

const FM_DEMOD: BlockSchema = {
  type_name: 'FmDemod',
  placement: 'either',
  inputs: [{ name: 'in', port_type: 'iq_f32' }],
  outputs: [{ name: 'out', port_type: 'real_f32' }],
  params: [
    RANGE_SELF('sample_rate_hz', 'sourceRestart'),
    RANGE_SELF('max_deviation_hz', 'downstream'),
  ],
  ai_notes: '',
};

const AM_DEMOD: BlockSchema = {
  type_name: 'AmDemod',
  placement: 'either',
  inputs: [{ name: 'in', port_type: 'iq_f32' }],
  outputs: [{ name: 'out', port_type: 'real_f32' }],
  params: [RANGE_SELF('sample_rate_hz', 'sourceRestart'), RANGE_SELF('bias_tau_ms', 'downstream')],
  ai_notes: '',
};

const DECIMATOR: BlockSchema = {
  type_name: 'Decimator',
  placement: 'either',
  inputs: [{ name: 'in', port_type: 'iq_f32' }],
  outputs: [{ name: 'out', port_type: 'iq_f32' }],
  params: [
    RANGE_SELF('factor', 'downstream'),
    RANGE_SELF('num_taps', 'downstream'),
    RANGE_SELF('cutoff_normalized', 'downstream'),
  ],
  ai_notes: '',
};

const SOAPY_SOURCE: BlockSchema = {
  type_name: 'SoapySource',
  placement: 'native',
  inputs: [],
  outputs: [{ name: 'out', port_type: 'iq_f32' }],
  params: [
    RANGE_SELF('sample_rate_hz', 'sourceRestart'),
    RANGE_SELF('center_freq_hz', 'self'),
    RANGE_SELF('bandwidth_hz', 'self'),
  ],
  ai_notes: '',
};

const SCHEMAS: BlockSchema[] = [AM_DEMOD, FM_DEMOD, DECIMATOR, SOAPY_SOURCE];

function wbfmDoc(): FlowgraphDoc {
  return {
    name: 'wbfm',
    environments: ['node', 'browser'],
    blocks: {
      src: {
        type: 'SoapySource',
        params: {
          args: 'driver=rtlsdr',
          sample_rate_hz: 2_400_000,
          center_freq_hz: 100_100_000,
          bandwidth_hz: 2_000_000,
        },
      },
      decim: {
        type: 'Decimator',
        placement: 'browser',
        params: { factor: 5, num_taps: 41, cutoff_normalized: 0.08 },
      },
      demod: {
        type: 'FmDemod',
        placement: 'browser',
        params: { sample_rate_hz: 48000, max_deviation_hz: 75000 },
      },
    },
    wires: [
      ['src.out', 'decim.in'],
      ['decim.out', 'demod.in'],
    ],
  };
}

function editParam(doc: FlowgraphDoc, blockId: string, key: string, value: unknown): FlowgraphDoc {
  const blocks = { ...(doc.blocks ?? {}) };
  const b = blocks[blockId];
  if (!b) throw new Error(`no block ${blockId}`);
  blocks[blockId] = { ...b, params: { ...(b.params ?? {}), [key]: value } };
  return { ...doc, blocks };
}

describe('predictScope — dialog reconfigure paths', () => {
  test('no change is a no-op', () => {
    const d = wbfmDoc();
    const plan = predictScope(d, d, SCHEMAS);
    expect(plan.noop).toBe(true);
    expect(plan.overall).toBe('self');
    expect(plan.changes).toEqual([]);
    expect(plan.structural_count).toBe(0);
  });

  test('FmDemod.max_deviation_hz edit is downstream', () => {
    const before = wbfmDoc();
    const after = editParam(before, 'demod', 'max_deviation_hz', 50_000);
    const plan = predictScope(before, after, SCHEMAS);
    expect(plan.changes).toHaveLength(1);
    expect(plan.changes[0]).toMatchObject({
      block_id: 'demod',
      param_key: 'max_deviation_hz',
      scope: 'downstream',
    });
    expect(plan.overall).toBe('downstream');
  });

  test('FmDemod.sample_rate_hz edit is sourceRestart', () => {
    const before = wbfmDoc();
    const after = editParam(before, 'demod', 'sample_rate_hz', 44_100);
    const plan = predictScope(before, after, SCHEMAS);
    expect(plan.overall).toBe('sourceRestart');
    expect(plan.changes[0]?.scope).toBe('sourceRestart');
  });

  test('AmDemod.bias_tau_ms edit is downstream', () => {
    // Start from an AM variant so the demod block type matches AmDemod.
    const before = applyRecipe(wbfmDoc(), findRecipe('am'))!;
    const after = editParam(before, 'demod', 'bias_tau_ms', 50);
    const plan = predictScope(before, after, SCHEMAS);
    expect(plan.overall).toBe('downstream');
    expect(plan.changes[0]?.scope).toBe('downstream');
  });

  test('Decimator.factor edit is downstream', () => {
    const before = wbfmDoc();
    const after = editParam(before, 'decim', 'factor', 10);
    const plan = predictScope(before, after, SCHEMAS);
    expect(plan.overall).toBe('downstream');
    expect(plan.changes[0]?.scope).toBe('downstream');
  });

  test('SoapySource.center_freq_hz edit from the source dialog is self', () => {
    const before = wbfmDoc();
    const after = editParam(before, 'src', 'center_freq_hz', 101_000_000);
    const plan = predictScope(before, after, SCHEMAS);
    expect(plan.overall).toBe('self');
    expect(plan.changes[0]?.scope).toBe('self');
  });

  test('SoapySource.sample_rate_hz edit is sourceRestart', () => {
    const before = wbfmDoc();
    const after = editParam(before, 'src', 'sample_rate_hz', 1_024_000);
    const plan = predictScope(before, after, SCHEMAS);
    expect(plan.overall).toBe('sourceRestart');
  });

  test('receivers-pane FM→AM swap is structural → sourceRestart', () => {
    const before = wbfmDoc();
    const after = applyRecipe(before, findRecipe('am'))!;
    const plan = predictScope(before, after, SCHEMAS);
    expect(plan.structural_count).toBe(1);
    expect(plan.overall).toBe('sourceRestart');
  });

  test('mixed edit uses the max scope across param changes', () => {
    const before = wbfmDoc();
    const after = editParam(
      editParam(before, 'demod', 'max_deviation_hz', 50_000),
      'decim',
      'factor',
      10,
    );
    const plan = predictScope(before, after, SCHEMAS);
    // Both edits are downstream, no sourceRestart involved.
    expect(plan.changes).toHaveLength(2);
    expect(plan.overall).toBe('downstream');
  });

  test('source + demod mixed edit merges to sourceRestart', () => {
    const before = wbfmDoc();
    const after = editParam(
      editParam(before, 'demod', 'max_deviation_hz', 50_000),
      'src',
      'sample_rate_hz',
      1_024_000,
    );
    const plan = predictScope(before, after, SCHEMAS);
    expect(plan.overall).toBe('sourceRestart');
  });
});
