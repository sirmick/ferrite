// Main-thread store for the audio panel: volume, mute, and live
// peak/RMS levels. Mirrors state into every attached `AudioWorkletNode`
// (the worklet owns the gain multiplier and sends back batch stats),
// and decays a peak-hold line so the meter's top indicator lingers
// briefly at the loudest recent sample.
//
// Lives on the main thread; the worklet's message port is the only
// audio-thread contact. Survives preset reloads: when the browser
// runtime tears down and recreates AudioWorkletNodes, the store keeps
// the user's volume/mute settings and re-pushes them on attach.

import type { AudioControlMessage, AudioLevelMessage } from './audioRingProcessor';

/** Peak-hold decay rate, dB per second. Typical pro-audio meters are
 *  around 20 dB/s — fast enough to track a sustained signal, slow
 *  enough that the user can see peaks from a moment ago. */
const PEAK_HOLD_DECAY_DB_PER_SEC = 20;

/** Silence threshold for log conversion — below this, peak/rms map to
 *  the floor of the meter (-80 dBFS ≈ 0.0001 linear) rather than -∞. */
const SILENCE_FLOOR_LINEAR = 1e-4;

export class AudioPanelStore {
  /** User volume knob. 1.0 = unity gain, 2.0 = +6 dB, 0 = silent. */
  volume = $state(1.0);
  /** Separate mute toggle so users can unmute back to their chosen volume. */
  muted = $state(false);
  /** Most recent peak reported by the worklet, linear. */
  peak = $state(0);
  /** Most recent RMS, linear. */
  rms = $state(0);
  /** Peak with slow decay — the classic meter "hold" indicator. */
  peakHold = $state(0);
  /** True once any worklet node is attached — UI uses this to pick
   *  between "no audio" and "muted/silent" messaging. */
  attached = $state(false);

  private nodes = new Set<AudioWorkletNode>();
  private decayTimer: ReturnType<typeof setInterval> | undefined;
  private lastDecayAt = 0;

  /** Attach a worklet node. The store immediately pushes current
   *  volume/muted and subscribes to level updates. Idempotent per node. */
  attach(node: AudioWorkletNode): void {
    if (this.nodes.has(node)) return;
    this.nodes.add(node);
    this.attached = this.nodes.size > 0;
    // Push the current state so a freshly-created node doesn't start
    // at unity when the user had it at 50%.
    this.sendTo(node, { volume: this.volume, muted: this.muted });
    node.port.onmessage = (ev: MessageEvent) => {
      const msg = ev.data as AudioLevelMessage | undefined;
      if (!msg || msg.type !== 'level') return;
      this.peak = msg.peak;
      this.rms = msg.rms;
      if (msg.peak > this.peakHold) this.peakHold = msg.peak;
    };
    if (!this.decayTimer) this.startDecay();
  }

  /** Detach a node — call on disconnect. Stops forwarding state to it
   *  and clears the level listener. Doesn't touch volume/muted so a
   *  subsequent `attach` lines the new node up with the same settings. */
  detach(node: AudioWorkletNode): void {
    if (!this.nodes.delete(node)) return;
    node.port.onmessage = null;
    this.attached = this.nodes.size > 0;
    if (!this.attached) {
      this.peak = 0;
      this.rms = 0;
      this.peakHold = 0;
      this.stopDecay();
    }
  }

  /** Slider writes land here. Pushes to every attached node. */
  setVolume(v: number): void {
    const clamped = Math.max(0, Math.min(2, v));
    if (clamped === this.volume) return;
    this.volume = clamped;
    this.broadcast({ volume: clamped });
  }

  setMuted(m: boolean): void {
    if (m === this.muted) return;
    this.muted = m;
    this.broadcast({ muted: m });
  }

  private broadcast(msg: AudioControlMessage): void {
    for (const n of this.nodes) this.sendTo(n, msg);
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
      if (this.peakHold <= 0) return;
      // Convert the current peakHold to dB, subtract the decay, convert
      // back. Clamping to the silence floor keeps the bar from inching
      // below the scale.
      const db = 20 * Math.log10(Math.max(this.peakHold, SILENCE_FLOOR_LINEAR));
      const newDb = db - PEAK_HOLD_DECAY_DB_PER_SEC * dt;
      const linear = Math.pow(10, newDb / 20);
      this.peakHold = linear > SILENCE_FLOOR_LINEAR ? linear : 0;
    }, 50);
  }

  private stopDecay(): void {
    if (this.decayTimer !== undefined) {
      clearInterval(this.decayTimer);
      this.decayTimer = undefined;
    }
  }
}

/** Singleton — one audio panel store per page. */
export const audioPanel = new AudioPanelStore();

/** Convert a linear audio value to dBFS, clamped to a usable display
 *  range. Used by both the meter's own bars and any text readout. */
export function linearToDbfs(linear: number): number {
  if (linear <= SILENCE_FLOOR_LINEAR) return -80;
  return 20 * Math.log10(linear);
}
