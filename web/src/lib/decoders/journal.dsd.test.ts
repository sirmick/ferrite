import { describe, expect, it } from 'vitest';

import { journalLine } from './journal';

describe('journalLine dsd', () => {
  it('renders a DMR call with slot, talkgroup, source and colour code', () => {
    expect(
      journalLine('dsd', {
        t: 'dsd',
        event: 'call',
        protocol: 'DMR',
        slot: 2,
        tgt: '31068',
        src: '3214431',
        cc: 1,
        text: 'SLOT 2 TGT=31068 SRC=3214431 Group Call',
      }),
    ).toBe('DMR S2  TG 31068 ← 3214431  CC 1');
  });

  it('renders talker alias, sync and helper events', () => {
    expect(journalLine('dsd', { event: 'alias', protocol: 'DMR', slot: 1, text: 'KR1JOE' })).toBe(
      'DMR S1  talker alias: KR1JOE',
    );
    expect(journalLine('dsd', { event: 'sync', protocol: 'DMR', text: '+DMR [slot1] slot2' })).toBe(
      'DMR sync  +DMR [slot1] slot2',
    );
    expect(journalLine('dsd', { event: 'voice', text: 'voice audio started' })).toBe(
      'voice audio started',
    );
  });

  it('flags encrypted calls', () => {
    expect(
      journalLine('dsd', { event: 'call', protocol: 'P25p1', tgt: '1201', src: '7001', enc: true }),
    ).toBe('P25p1  TG 1201 ← 7001  ENC');
  });
});
