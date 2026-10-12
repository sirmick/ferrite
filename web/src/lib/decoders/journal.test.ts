import { describe, expect, it } from 'vitest';

import { journalLine } from './journal';

describe('journalLine', () => {
  it('renders a FLEX alpha page as protocol, capcode and the text', () => {
    const line = journalLine('pager', {
      proto: 'FLEX',
      protocol: 'FLEX_NEXT',
      capcode: 7381450,
      format: 'alpha',
      frag: 'K',
      message: 'From: bigmessagesllc1@verizon.net Subject: BM',
      text: 'FLEX_NEXT|3200/4|11.020.A|0007381450|LS|5|ALN|3.0.K|From: …',
    });
    expect(line).toBe('FLEX 7381450  From: bigmessagesllc1@verizon.net Subject: BM');
  });

  it('marks fragments, numeric and tone-only pages', () => {
    expect(
      journalLine('pager', {
        protocol: 'FLEX_NEXT',
        capcode: 1,
        format: 'alpha',
        frag: 'C',
        message: 'start',
      }),
    ).toBe('FLEX 1 part 1  start');
    expect(
      journalLine('pager', {
        protocol: 'FLEX_NEXT',
        capcode: 1,
        format: 'alpha',
        frag: 'F',
        message: 'more',
      }),
    ).toBe('FLEX 1 cont.  more');
    expect(
      journalLine('pager', {
        protocol: 'FLEX_NEXT',
        capcode: 2,
        format: 'numeric',
        message: '555-1234',
      }),
    ).toBe('FLEX 2 num  555-1234');
    expect(
      journalLine('pager', { protocol: 'FLEX_NEXT', capcode: 3, format: 'tone', message: '' }),
    ).toBe('FLEX 3  (tone only)');
  });

  it('renders POCSAG by address and falls back to the raw line', () => {
    expect(
      journalLine('pager', {
        protocol: 'POCSAG',
        baud: 1200,
        address: 1234567,
        format: 'alpha',
        message: 'HI',
      }),
    ).toBe('POCSAG 1234567  HI');
    expect(journalLine('pager', { proto: 'FLEX', text: 'FLEX_NEXT: Locked' })).toBe(
      'FLEX_NEXT: Locked',
    );
  });

  it('renders an EAS header and the end-of-message marker', () => {
    expect(
      journalLine('eas', {
        type: 'header',
        originator: 'WXR',
        originator_name: 'National Weather Service',
        event: 'RWT',
        event_name: 'Required Weekly Test',
        locations: ['012057', '012081'],
        purge_minutes: 30,
        sender: 'KTBW/NWS',
      }),
    ).toBe(
      'EAS Required Weekly Test · National Weather Service · KTBW/NWS · areas 012057,012081 · 30 min',
    );
    expect(journalLine('eas', { type: 'end_of_message' })).toBe('EAS end of message');
  });

  it('shows other kinds by their text or compactly', () => {
    expect(journalLine('rtl_433', { text: 'Acurite 5n1 id=42' })).toBe('Acurite 5n1 id=42');
    expect(journalLine('rtl_433', { model: 'Acurite', id: 42 })).toBe(
      '{"model":"Acurite","id":42}',
    );
  });
});
