import { describe, expect, it } from 'vitest';

import type { DecoderRecord } from '$lib/decoders/store.svelte';

import { EMPTY_CURSOR, GAP_BREAK_MS, TEXT_CAP, foldRunningText } from './runningText';

const rec = (seq: number, text: string, at_ms = seq * 100, mode = 'navtex'): DecoderRecord => ({
  seq,
  epoch: 1,
  at_ms,
  data: { t: 'fldigi', mode, text },
});

describe('foldRunningText', () => {
  it('joins per-character records into continuous text and keeps newlines', () => {
    const chars = [...'ZCZC CA05\n1. THE'].map((c, i) => rec(i + 1, c));
    const { buffer, cursor } = foldRunningText('', EMPTY_CURSOR, chars);
    expect(buffer).toBe('ZCZC CA05\n1. THE');
    expect(cursor.seq).toBe(chars.length);
  });

  it('only folds records newer than the cursor, so a capped ring does not lose text', () => {
    const first = foldRunningText('', EMPTY_CURSOR, [rec(1, 'A'), rec(2, 'B')]);
    // The ring dropped seq 1; seq 2 is already folded, seq 3 is new.
    const second = foldRunningText(first.buffer, first.cursor, [rec(2, 'B'), rec(3, 'C')]);
    expect(second.buffer).toBe('ABC');
    expect(second.cursor.seq).toBe(3);
  });

  it('treats a new epoch as new text even if seq restarted', () => {
    const first = foldRunningText('', EMPTY_CURSOR, [rec(10, 'X')]);
    const restarted: DecoderRecord = { seq: 1, epoch: 2, at_ms: 2000, data: { text: 'Y' } };
    const second = foldRunningText(first.buffer, first.cursor, [restarted]);
    expect(second.buffer).toBe('XY');
  });

  it('starts a new paragraph after a long silence', () => {
    const slot1 = [rec(1, 'N', 1000), rec(2, 'N', 1100), rec(3, '\n', 1200)];
    const slot2 = [rec(4, 'Z', 1200 + GAP_BREAK_MS), rec(5, 'C', 1300 + GAP_BREAK_MS)];
    const { buffer } = foldRunningText('', EMPTY_CURSOR, [...slot1, ...slot2]);
    expect(buffer).toBe('NN\n\nZC');
  });

  it('breaks the line when the mode changes (RSID auto-switch)', () => {
    const { buffer } = foldRunningText('', EMPTY_CURSOR, [
      rec(1, 'CQ', 100, 'rtty45'),
      rec(2, 'de', 200, 'BPSK31'),
    ]);
    expect(buffer).toBe('CQ\nde');
  });

  it('drops carriage returns and skips empty records without moving text', () => {
    const { buffer } = foldRunningText('', EMPTY_CURSOR, [rec(1, 'A\r'), rec(2, ''), rec(3, 'B')]);
    expect(buffer).toBe('AB');
  });

  it('caps the buffer, trimming from the front at a line boundary', () => {
    const long = `${'x'.repeat(100)}\n${'y'.repeat(TEXT_CAP)}`;
    const { buffer } = foldRunningText('', EMPTY_CURSOR, [rec(1, long)]);
    expect(buffer).toBe('y'.repeat(TEXT_CAP));
  });
});
