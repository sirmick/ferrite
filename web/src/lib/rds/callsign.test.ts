import { describe, expect, it } from 'vitest';

import { rbdsCallsign } from './callsign';

describe('rbdsCallsign', () => {
  it('decodes K and W calls', () => {
    expect(rbdsCallsign(0x1d2b)).toBe('KEZR'); // Mix 106.5 San Jose, heard live
    expect(rbdsCallsign(0x1000)).toBe('KAAA');
    expect(rbdsCallsign(0x54a7)).toBe('KZZZ');
    expect(rbdsCallsign(0x54a8)).toBe('WAAA');
    expect(rbdsCallsign(0x994f)).toBe('WZZZ');
  });

  it('returns null outside the derived ranges', () => {
    expect(rbdsCallsign(null)).toBeNull();
    expect(rbdsCallsign(0x0fff)).toBeNull();
    expect(rbdsCallsign(0x9950)).toBeNull(); // three-letter calls
    expect(rbdsCallsign(0xc000)).toBeNull(); // Canada
  });
});
