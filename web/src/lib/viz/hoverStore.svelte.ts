// Cross-pane mouse-hover frequency.
//
// When the pointer is over any spectrum/waterfall pane (wide or narrow,
// FFT or waterfall), the pane writes the freq under the cursor here;
// the other three panes read it and paint a vertical preview line at
// that freq — so the operator sees, in real Hz, where the VFO would
// land if they clicked. `null` means "no pane has hover" → all four
// panes hide the line.
//
// In-memory only — bringing the page back up with a hover line would
// look broken.

class HoverStore {
  /** Freq the operator is currently pointing at, in absolute Hz, or
   *  `null` when no pane has a live pointer-over. The producing pane
   *  uses its own captured axis (margin-aware) to compute this; the
   *  consuming panes each project it back into their own visible
   *  window and skip drawing when it falls outside. */
  freqHz = $state<number | null>(null);
}

export const hoverStore = new HoverStore();

/** Pct (0–100) of `freqHz` within a freq window `[centerHz ± rateHz/2]`,
 *  or `null` when the freq is outside the window or the input is bad.
 *  Consumers feed the result into a `calc(LEFT_MARGIN + (100% -
 *  margins) * pct / 100)` style left-position — same pattern the
 *  existing VFO marker overlays use. */
export function hoverPctInWindow(
  freqHz: number | null,
  centerHz: number | undefined,
  rateHz: number | undefined,
): number | null {
  if (
    freqHz === null ||
    centerHz === undefined ||
    rateHz === undefined ||
    !(rateHz > 0) ||
    !Number.isFinite(freqHz)
  ) {
    return null;
  }
  const min = centerHz - rateHz / 2;
  if (freqHz < min || freqHz > centerHz + rateHz / 2) return null;
  return ((freqHz - min) / rateHz) * 100;
}
