// How tool calls read in the AI transcript.

import { describe, expect, it } from 'vitest';

import { fmtHz, resultSummary, signalsOf, toolLabel, toolSummary, toolWhy } from './toolView';

describe('toolLabel', () => {
  it('drops the ferrite MCP prefix, keeps other servers', () => {
    expect(toolLabel('mcp__ferrite__tune')).toBe('tune');
    expect(toolLabel('mcp__ferrite__set_block_param')).toBe('set_block_param');
    expect(toolLabel('mcp__other__thing')).toBe('other:thing');
    expect(toolLabel('Bash')).toBe('Bash');
  });
});

describe('toolSummary', () => {
  it('formats frequencies and puts the why on its own line', () => {
    const input = JSON.stringify({ carrier_hz: 7025800, note: 'park on the CW signal' });
    expect(toolSummary('mcp__ferrite__tune', input)).toBe('carrier=7.0258 MHz');
    expect(toolWhy(input)).toBe('park on the CW signal');
  });

  it('shows a shell command as `$ …` with its description as the why', () => {
    const input = JSON.stringify({ command: 'ls -la', description: 'List files' });
    expect(toolSummary('Bash', input)).toBe('$ ls -la');
    expect(toolWhy(input)).toBe('List files');
  });

  it('keeps short args readable and falls back to raw text', () => {
    expect(toolSummary('mcp__ferrite__load_preset', '{"name":"cw"}')).toBe('name=cw');
    expect(toolSummary('x', 'not json')).toBe('not json');
  });
});

describe('fmtHz', () => {
  it('picks a unit', () => {
    expect(fmtHz(162_475_000)).toBe('162.475 MHz');
    expect(fmtHz(1200)).toBe('1.2 kHz');
    expect(fmtHz(700)).toBe('700 Hz');
  });
});

describe('resultSummary', () => {
  it('says what failed', () => {
    expect(resultSummary('{"error":"RADIO_UNREACHABLE: bigear down"}', true)).toBe(
      'RADIO_UNREACHABLE: bigear down',
    );
    expect(resultSummary('first line\nsecond', true)).toBe('first line');
  });
  it('is quiet for a plain JSON payload; shows a status or message', () => {
    expect(resultSummary('{"peaks":[]}', false)).toBeNull();
    expect(resultSummary('{"status":"running"}', false)).toBe('running');
    expect(resultSummary(undefined, false)).toBeNull();
  });
});

describe('signalsOf', () => {
  it('reads the signals watchlist', () => {
    const r = JSON.stringify({
      signals: [{ freq_hz: 98.5e6, power_db: -40, snr_db: 28 }, { nope: 1 }],
    });
    expect(signalsOf('mcp__ferrite__signals', r)).toEqual([
      { freq_hz: 98.5e6, power_db: -40, snr_db: 28 },
    ]);
    expect(signalsOf('mcp__ferrite__status', r)).toBeNull();
  });
});
