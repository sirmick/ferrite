// Main-thread store for the audio panel: tracks live peak/RMS and
// relays volume/mute changes into every attached `AudioWorkletNode`.
//
// Persisted state (volume, muted) lives in the client control store —
// `applyControl('client.audio.volume', v)` persists AND calls
// `setVolume` here to push the worklet side. On attach we seed the
// new node from the store so a fresh AudioWorkletNode doesn't snap
// back to unity gain after a preset reload.
//
// Stereo presets attach two nodes, one with role 'left' and one with
// role 'right'. The panel then tracks per-channel peak/rms instead of
// the single-meter `peak` / `rms` used by mono presets; `hasStereo`
// lets the UI pick which meter to render.

import { clientControls } from '$lib/control/clientStore.svelte';
import type { AudioControlMessage, AudioLevelMessage } from './audioRingProcessor';

/** Role carried by an attached worklet node. `mono` fans out to both
 *  output channels; `left` / `right` route to a single channel on a
 *  ChannelMerger so the two sinks combine into one stereo stream. */
export type AudioNodeRole = 'mono' | 'left' | 'right';

/** Peak-hold decay rate, dB per second. Typical pro-audio meters are
 *  around 20 dB/s — fast enough to track a sustained signal, slow
 *  enough that the user can see peaks from a moment ago. */
const PEAK_HOLD_DECAY_DB_PER_SEC = 20;

/** Silence threshold for log conversion — below this, peak/rms map to
 *  the floor of the meter (-80 dBFS ≈ 0.0001 linear) rather than -∞. */
const SILENCE_FLOOR_LINEAR = 1e-4;

export class AudioPanelStore {
  /** Most recent peak reported by the worklet, linear. On a stereo
   *  preset this tracks the louder of L/R; on mono, the only channel. */
  peak = $state(0);
  /** Most recent RMS, linear. Stereo: max(L, R). Mono: the only channel. */
  rms = $state(0);
  /** Peak with slow decay — the classic meter "hold" indicator. */
  peakHold = $state(0);

  /** Per-channel mirrors of peak / rms / peakHold for the stereo UI. */
  peakLeft = $state(0);
  peakRight = $state(0);
  rmsLeft = $state(0);
  rmsRight = $state(0);
  peakHoldLeft = $state(0);
  peakHoldRight = $state(0);

  /** True once any worklet node is attached — UI uses this to pick
   *  between "no audio" and "muted/silent" messaging. */
  attached = $state(false);
  /** True when both a 'left' and 'right' node are attached — AudioPanel
   *  flips to a two-bar layout. */
  hasStereo = $state(false);

  private nodes = new Map<AudioWorkletNode, AudioNodeRole>();
  private decayTimer: ReturnType<typeof setInterval> | undefined;
  private lastDecayAt = 0;

  /** Attach a worklet node in the given channel role. The store seeds
   *  it from the client control store's current volume/muted (so preset
   *  reloads don't snap gain back to unity) and starts the
   *  level-message listener. Idempotent per node. */
  attach(node: AudioWorkletNode, role: AudioNodeRole = 'mono'): void {
    if (this.nodes.has(node)) return;
    this.nodes.set(node, role);
    this.refreshAttachedFlags();
    this.sendTo(node, {
      volume: clientControls.get('client.audio.volume'),
      muted: clientControls.get('client.audio.muted'),
    });
    node.port.onmessage = (ev: MessageEvent) => {
      const msg = ev.data as AudioLevelMessage | undefined;
      if (!msg || msg.type !== 'level') return;
      this.ingestLevel(role, msg);
    };
    if (!this.decayTimer) this.startDecay();
  }

  /** Detach a node — call on disconnect. Stops forwarding state to it
   *  and clears the level listener. Doesn't touch volume/muted so a
   *  subsequent `attach` lines the new node up with the same settings. */
  detach(node: AudioWorkletNode): void {
    if (!this.nodes.delete(node)) return;
    node.port.onmessage = null;
    this.refreshAttachedFlags();
    if (!this.attached) {
      this.peak = 0;
      this.rms = 0;
      this.peakHold = 0;
      this.peakLeft = 0;
      this.peakRight = 0;
      this.rmsLeft = 0;
      this.rmsRight = 0;
      this.peakHoldLeft = 0;
      this.peakHoldRight = 0;
      this.stopDecay();
    }
  }

  private ingestLevel(role: AudioNodeRole, msg: AudioLevelMessage): void {
    if (role === 'left') {
      this.peakLeft = msg.peak;
      this.rmsLeft = msg.rms;
      if (msg.peak > this.peakHoldLeft) this.peakHoldLeft = msg.peak;
    } else if (role === 'right') {
      this.peakRight = msg.peak;
      this.rmsRight = msg.rms;
      if (msg.peak > this.peakHoldRight) this.peakHoldRight = msg.peak;
    } else {
      this.peak = msg.peak;
      this.rms = msg.rms;
      if (msg.peak > this.peakHold) this.peakHold = msg.peak;
      return;
    }
    // Keep the combined peak/rms in sync for any UI that reads the
    // summary fields directly — mirrors `max(L, R)` so a mono meter
    // still looks right on a stereo preset.
    this.peak = Math.max(this.peakLeft, this.peakRight);
    this.rms = Math.max(this.rmsLeft, this.rmsRight);
    this.peakHold = Math.max(this.peakHoldLeft, this.peakHoldRight);
  }

  private refreshAttachedFlags(): void {
    this.attached = this.nodes.size > 0;
    let hasL = false;
    let hasR = false;
    for (const r of this.nodes.values()) {
      if (r === 'left') hasL = true;
      if (r === 'right') hasR = true;
    }
    this.hasStereo = hasL && hasR;
  }

  /** Push a volume change to every attached worklet. Called by the
   *  dispatcher for `client.audio.volume` — the store no longer owns
   *  the persisted value (that lives in `clientControls`). */
  setVolume(v: number): void {
    const clamped = Math.max(0, Math.min(2, v));
    this.broadcast({ volume: clamped });
  }

  /** Push a mute toggle to every attached worklet. Called by the
   *  dispatcher for `client.audio.muted`. */
  setMuted(m: boolean): void {
    this.broadcast({ muted: m });
  }

  private broadcast(msg: AudioControlMessage): void {
    for (const n of this.nodes.keys()) this.sendTo(n, msg);
  }

  private sendTo(node: AudioWorkletNode, msg: AudioControlMessage): void {
    try {
      node.port.postMessage(msg);
    } catch {
      /* closed ports are a silent no-op */
    }
  }

  private startDecay(): void {
    this.lastDecayAt = performance.now();
    // 50 ms tick — smooth visual decay, negligible CPU.
    this.decayTimer = setInterval(() => {
      const now = performance.now();
      const dt = (now - this.lastDecayAt) / 1000;
      this.lastDecayAt = now;
      this.peakHold = decayLinear(this.peakHold, dt);
      this.peakHoldLeft = decayLinear(this.peakHoldLeft, dt);
      this.peakHoldRight = decayLinear(this.peakHoldRight, dt);
    }, 50);
  }

  private stopDecay(): void {
    if (this.decayTimer !== undefined) {
      clearInterval(this.decayTimer);
      this.decayTimer = undefined;
    }
  }
}

/** One decay step on a linear peak-hold reading. Converts to dB,
 *  subtracts the configured per-second decay scaled by elapsed seconds,
 *  and clamps back. Returns 0 for values below the silence floor so
 *  the meter can fully reset. */
function decayLinear(current: number, dt: number): number {
  if (current <= 0) return 0;
  const db = 20 * Math.log10(Math.max(current, SILENCE_FLOOR_LINEAR));
  const newDb = db - PEAK_HOLD_DECAY_DB_PER_SEC * dt;
  const linear = Math.pow(10, newDb / 20);
  return linear > SILENCE_FLOOR_LINEAR ? linear : 0;
}

/** Singleton — one audio panel store per page. */
export const audioPanel = new AudioPanelStore();

/** Convert a linear audio value to dBFS, clamped to a usable display
 *  range. Used by both the meter's own bars and any text readout. */
export function linearToDbfs(linear: number): number {
  if (linear <= SILENCE_FLOOR_LINEAR) return -80;
  return 20 * Math.log10(linear);
}
