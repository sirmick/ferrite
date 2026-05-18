// Noise-reduction presets — named param bundles for the `audio_nr`
// block (AudioNrMono/Stereo). The NR chain is deemph → blanker → notch
// → spectral → neural → agc; every stage is live-reconfigurable
// (ReconfigureScope::SelfBlock), so a preset is just a delta pushed to
// the running browser block.
//
// `auto` is the default and a no-op: it leaves whatever NR the preset
// JSON authored (per-mode, tuned by the preset author) untouched. The
// other presets are *full* bundles — every `*_enable` is set
// explicitly so switching is deterministic and never inherits a stale
// stage from the JSON or a previous pick.
//
// Picking the receiver's Audio control = "transcribe" auto-selects
// `voice` (best signal for whisper); see ProfileChips.

export type NrPresetId = 'auto' | 'off' | 'voice' | 'ssb' | 'am' | 'fm';

export interface NrPreset {
  id: NrPresetId;
  label: string;
  title: string;
  /** Full param delta for `audio_nr`, or `null` for `auto` (no-op —
   *  keep the preset's authored NR). */
  params: Record<string, unknown> | null;
}

const ALL_STAGES_OFF = {
  deemph_enable: false,
  blanker_enable: false,
  notch_enable: false,
  spectral_enable: false,
  neural_enable: false,
  agc_enable: false,
} as const;

export const NR_PRESETS: readonly NrPreset[] = [
  {
    id: 'auto',
    label: 'auto',
    title: "Use the preset's authored noise-reduction (default).",
    params: null,
  },
  {
    id: 'off',
    label: 'off',
    title: 'Bypass — raw demodulated audio, no noise reduction.',
    params: { ...ALL_STAGES_OFF },
  },
  {
    id: 'voice',
    label: 'voice',
    title:
      'DeepFilterNet neural NR + gentle AGC — clean, not over-processed. ' +
      'No spectral subtraction (that double-processes and adds musical ' +
      'noise). Best for intelligibility/transcription; auto-selected by ' +
      '"transcribe".',
    params: {
      deemph_enable: false,
      blanker_enable: false,
      notch_enable: false,
      spectral_enable: false,
      neural_enable: true,
      neural_attenuation_db: 18,
      agc_enable: true,
      agc_target_dbfs: -6,
      agc_release_ms: 500,
      agc_max_gain_db: 20,
    },
  },
  {
    id: 'ssb',
    label: 'ssb',
    title:
      'HF SSB voice — neural NR plus an impulse blanker (static crashes) ' +
      'and an adaptive notch (carrier heterodynes). Still no spectral.',
    params: {
      deemph_enable: false,
      blanker_enable: true,
      blanker_threshold_db: 20,
      notch_enable: true,
      notch_mu: 0.01,
      spectral_enable: false,
      neural_enable: true,
      neural_attenuation_db: 18,
      agc_enable: true,
      agc_target_dbfs: -6,
      agc_release_ms: 600,
      agc_max_gain_db: 20,
    },
  },
  {
    id: 'am',
    label: 'am',
    title:
      'AM broadcast — impulse blanker for static crashes + AGC for ' +
      'fading; no neural/spectral (artifacts on music + speech mix).',
    params: {
      deemph_enable: false,
      blanker_enable: true,
      blanker_threshold_db: 16,
      notch_enable: false,
      spectral_enable: false,
      neural_enable: false,
      agc_enable: true,
      agc_target_dbfs: -6,
      agc_release_ms: 500,
      agc_max_gain_db: 18,
    },
  },
  {
    id: 'fm',
    label: 'fm',
    title:
      'FM / music — de-emphasis only (75 µs). Denoise stages off: ' +
      'spectral/neural wreck musical fidelity.',
    params: {
      deemph_enable: true,
      deemph_tau_us: 75,
      blanker_enable: false,
      notch_enable: false,
      spectral_enable: false,
      neural_enable: false,
      agc_enable: false,
    },
  },
];

const BY_ID = new Map(NR_PRESETS.map((p) => [p.id, p]));

/** The param delta for a preset id, or `null` for `auto`/unknown
 *  (caller should then leave `audio_nr` as authored). */
export function nrBundle(id: string | undefined | null): Record<string, unknown> | null {
  return (id && BY_ID.get(id as NrPresetId)?.params) || null;
}
