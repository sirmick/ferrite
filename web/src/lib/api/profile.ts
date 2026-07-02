// REST client for `/api/profile` — the runtime knobs applied before
// `split_for_environment` carves the doc. `audio` gates the audio chain;
// `transcribe` splices a VoiceTranscribe tap (same build-time mechanism,
// implies `audio`); `audio_split` slides the single node↔browser cut
// along the audio spine (demod → resample → NR → transcribe):
//   - 'browser'  — push the whole spine browser-side (thin server).
//   - 'balanced' — leave each block at its preset-authored placement
//                  (the default; historical behaviour).
//   - 'server'   — pull the whole spine node-side; a transcribe tap then
//                  runs headless (whisper in `ferrited`, no browser).
// One ordered setting instead of per-block placement flips — which keeps
// the boundary a single monotonic crossing env_split can realise.

import { ApiError } from '$lib/api/errors';

/** Where the audio spine is cut between daemon and browser. */
export type AudioSplit = 'browser' | 'balanced' | 'server';

export interface Profile {
  audio: boolean;
  transcribe: boolean;
  audio_split: AudioSplit;
}

export const DEFAULT_PROFILE: Profile = {
  audio: true,
  transcribe: false,
  audio_split: 'balanced',
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
