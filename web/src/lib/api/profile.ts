// REST client for `/api/profile` — the runtime knobs applied before
// `split_for_environment` carves the doc. Today: `audio` (gates the
// audio chain), `transcribe` (splices a VoiceTranscribe tap before
// every AudioSink — same build-time mechanism as `audio`, implies it),
// and three placement overrides — `demod_placement`, `nr_placement`,
// `transcribe_placement` — each flipping where the matching
// `placement_role`-tagged block runs (node vs browser). `null` leaves
// the preset's authored side in effect. `transcribe_placement: 'node'`
// is the headless path: whisper runs in `ferrited`, no browser needed.

import { ApiError } from '$lib/api/errors';

/** node = server-side (`ferrited`); browser = in the tab via WASM. */
export type Placement = 'node' | 'browser';
/** @deprecated use {@link Placement} */
export type DemodPlacement = Placement;

export interface Profile {
  audio: boolean;
  transcribe: boolean;
  demod_placement: Placement | null;
  nr_placement: Placement | null;
  transcribe_placement: Placement | null;
}

export const DEFAULT_PROFILE: Profile = {
  audio: true,
  transcribe: false,
  demod_placement: null,
  nr_placement: null,
  transcribe_placement: null,
};

export async function fetchProfile(): Promise<Profile> {
  const r = await fetch('/api/profile');
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `profile fetch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as Profile;
}

/** Replace the active profile and re-compose the current preset so the
 *  change takes effect on a running pipeline. Server-side: stores the
 *  profile, then runs `patch_flowgraph` on the current doc. */
export async function patchProfile(profile: Profile): Promise<Profile> {
  const r = await fetch('/api/profile', {
    method: 'PATCH',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(profile),
  });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `profile patch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as Profile;
}
