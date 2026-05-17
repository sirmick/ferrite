import { describe, expect, it } from 'vitest';

import { applyHamPostProcess, extractCallsigns, __test } from './hamPostProcess';

describe('applyHamPostProcess — callsign recovery', () => {
  it('collapses a phonetic spell-out into a valid callsign', () => {
    expect(applyHamPostProcess('whiskey one alpha bravo')).toBe('W1AB');
  });

  it('recovers a callsign embedded in a sentence', () => {
    expect(applyHamPostProcess('this is kilo niner charlie zulu calling cq')).toBe(
      'this is K9CZ calling CQ',
    );
  });

  it('handles a 2x1 prefix with portable appendix', () => {
    // VK2DEF/P — prefix V K, digit 2, suffix D E F, /P
    expect(__test.CALLSIGN_RE.test('VK2DEF/P')).toBe(true);
  });

  it('leaves ordinary speech containing a lone phonetic word alone', () => {
    // A lone "alpha" is a run of 1 — not callsign-shaped, not collapsed.
    expect(applyHamPostProcess('run the alpha test again')).toBe('run the alpha test again');
  });

  it('does not collapse a short non-callsign phonetic pair', () => {
    // "alpha bravo" → AB, not callsign-shaped, run length < 4 → kept.
    expect(applyHamPostProcess('alpha bravo')).toBe('alpha bravo');
  });
});

describe('applyHamPostProcess — signal reports & prosigns', () => {
  it('folds a spoken signal report into digits', () => {
    expect(applyHamPostProcess('you are five nine here')).toBe('you are 59 here');
  });

  it('preserves the "by" connector in an RST-with-tone report', () => {
    expect(applyHamPostProcess('five by nine')).toBe('5 by 9');
  });

  it('normalises seventy three and Q-codes', () => {
    expect(applyHamPostProcess('seventy three and qsl')).toBe('73 and QSL');
  });
});

describe('extractCallsigns', () => {
  it('pulls validated callsigns out of cleaned text', () => {
    const cleaned = applyHamPostProcess('cq cq de whiskey one alpha bravo k');
    expect(extractCallsigns(cleaned)).toEqual(['W1AB']);
  });

  it('returns nothing when no callsign is present', () => {
    expect(extractCallsigns('good morning on the band')).toEqual([]);
  });
});

describe('foldNumbers', () => {
  it('joins consecutive digit words, leaves others', () => {
    expect(__test.foldNumbers(['five', 'nine', 'and', 'seven'])).toEqual(['59', 'and', '7']);
  });
});
