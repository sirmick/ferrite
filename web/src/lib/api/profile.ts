// REST client for `/api/profile` — the runtime knobs applied before
// `split_for_environment` carves the doc. Today: `audio` (gates the
// audio chain), `transcribe` (splices a VoiceTranscribe tap before
// every AudioSink — same build-time mechanism as `audio`, implies it),
// and `demod_placement` (overrides where the `placement_role: "demod"`
// block lives).

import { ApiError } from '$lib/api/errors';

export type DemodPlacement = 'node' | 'browser';

export interface Profile {
  audio: boolean;
  transcribe: boolean;
  demod_placement: DemodPlacement | null;
}

export const DEFAULT_PROFILE: Profile = {
  audio: true,
  transcribe: false,
  demod_placement: null,
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
