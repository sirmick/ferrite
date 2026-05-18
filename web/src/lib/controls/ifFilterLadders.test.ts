// Validates the generated IF-filter ladder artifact. The data is the
// Rust-owned single source of truth (tools/ferrite-ctl/src/sdr_tables
// .rs); `cargo run -p ferrite-ctl -- gen-tables` writes the JSON and a
// Rust CI test guards byte-equality. This test guards the *shape* the
// web side relies on (`ladderFor` / `pickFromLadder` in optionsModel),
// so a malformed artifact fails `pnpm test`, not the device dialog.

import { describe, expect, it } from 'vitest';

import ladders from './if-filter-ladders.generated.json';

describe('if-filter-ladders.generated.json', () => {
  it('is an object of driver → number[]', () => {
    expect(typeof ladders).toBe('object');
    expect(ladders).not.toBeNull();
    expect(Array.isArray(ladders)).toBe(false);
  });

  const entries = Object.entries(ladders as Record<string, number[]>);

  it('has at least one driver', () => {
    expect(entries.length).toBeGreaterThan(0);
  });

  for (const [driver, ladder] of entries) {
    describe(driver, () => {
      it('key is a lowercase SoapySDR driver short name', () => {
        expect(driver).toMatch(/^[a-z][a-z0-9]*$/);
      });

      it('is a non-empty sorted-ascending unique-positive Hz list', () => {
        expect(Array.isArray(ladder)).toBe(true);
        expect(ladder.length).toBeGreaterThan(0);
        for (let i = 0; i < ladder.length; i++) {
          expect(typeof ladder[i]).toBe('number');
          expect(ladder[i]).toBeGreaterThan(0);
          if (i > 0) expect(ladder[i]).toBeGreaterThan(ladder[i - 1]);
        }
      });
    });
  }
});
