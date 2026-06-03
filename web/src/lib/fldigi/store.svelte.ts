// fldigi decode store — now a thin adapter over the decoder mirror.
//
// fldigi-family decoders (RttyDemod / Psk31Demod / CwDemod / Mt63Demod
// / FldigiAuto / Olivia / Contestia / DominoEX / Throb / Navtex) emit one
// newline-delimited JSON record per decoded chunk on their `events` port
// — `{"t":"fldigi","mode":"<label>","text":"<decoded>"}` (fldigi_modes.rs)
// — wired to `ui:fldigi` → the server `DecoderStore` (kind `fldigi`,
// append log), mirrored to the browser over `/ws/state`. This keeps the
// original `fldigi.lines` / `fldigi.mode` surface so FldigiView is
// unchanged; it's a continuous text console rendered by TextConsole.

import { decoders } from '$lib/decoders/store.svelte';
import type { ConsoleLine } from '$lib/viz/TextConsole.svelte';

interface FldigiRec {
  t?: string;
  mode?: string;
  text?: string;
}

export const fldigi = {
  /** Append-only decoded-text console (newest last). */
  get lines(): ConsoleLine[] {
    const out: ConsoleLine[] = [];
    for (const r of decoders.kind('fldigi').recent) {
      const d = r.data as FldigiRec | undefined;
      if (typeof d?.text === 'string' && d.text) {
        out.push({ id: String(r.seq), ts: Math.floor(r.at_ms / 1000), text: d.text });
      }
    }
    return out;
  },
  /** Most-recent mode label seen (e.g. `rtty45`, `BPSK31`). */
  get mode(): string {
    const recent = decoders.kind('fldigi').recent;
    for (let i = recent.length - 1; i >= 0; i--) {
      const m = (recent[i].data as FldigiRec | undefined)?.mode;
      if (typeof m === 'string') return m;
    }
    return '';
  },

  // Mirror is always connected — attach/detach are no-ops now.
  attach(_client?: unknown, _streamId?: unknown): void {},
  detach(): void {},
  /** Operator Reset — clears the kind server-side; the mirror follows. */
  reset(): void {
    void fetch('/api/state/fldigi/reset', { method: 'POST' }).catch(() => {});
  },
};
