// US call sign from an RBDS PI code (NRSC-4, Annex D).
//
// US stations derive their PI from their call letters, so the PI —
// which repeats in every RDS group — names the station even when the
// 8-char PS is scrolling a slogan or a song title past. Four-letter
// calls only; three-letter calls (a lookup table), Canada / Mexico and
// non-derived codes return null.

const K_BASE = 0x1000; // KAAA
const W_BASE = 0x54a8; // WAAA
const W_END = 0x994f; // WZZZ

export function rbdsCallsign(pi: number | null): string | null {
  if (pi === null || !Number.isInteger(pi)) return null;
  let first: string;
  let n: number;
  if (pi >= K_BASE && pi < W_BASE) {
    first = 'K';
    n = pi - K_BASE;
  } else if (pi >= W_BASE && pi <= W_END) {
    first = 'W';
    n = pi - W_BASE;
  } else {
    return null;
  }
  const letter = (i: number) => String.fromCharCode(65 + i);
  return first + letter(Math.floor(n / 676)) + letter(Math.floor((n % 676) / 26)) + letter(n % 26);
}
