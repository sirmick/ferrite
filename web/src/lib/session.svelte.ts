// Singleton session store — owns the FrameClient + REST handles for the
// one active ferrited session. Components read `session.*` fields and
// the runes-based reactivity propagates updates.
//
// Lifetime model: at most one open session at a time (mirrors the
// server's last-connect-wins eviction policy in
// `server/src/session.rs::AppState::open`). Calling `open()` while a
// previous session is live tears the previous one down first.

import {
  closeDevice,
  fetchSessionState,
  openDevice,
  patchSettings,
  wsUrlFor,
  type OpenDeviceRequest,
  type PatchSettingsRequest,
  type SessionState,
} from '$lib/api/device';
import type { DeviceCapabilities } from '$lib/api/devices';
import { FrameClient, type ClientStatus } from '$lib/ws/client';

export type SessionPhase = 'idle' | 'connecting' | 'open' | 'error';

class SessionStore {
  phase = $state<SessionPhase>('idle');
  wsStatus = $state<ClientStatus | 'idle'>('idle');
  errorMessage = $state<string | null>(null);
  state = $state<SessionState | null>(null);
  request = $state<OpenDeviceRequest | null>(null);

  /** Active FrameClient, or `undefined` between sessions. */
  client = $state<FrameClient | undefined>(undefined);
  sessionId = $state<string | undefined>(undefined);
  /**
   * Capability schema from the device picker, when this session was
   * opened against a Soapy device. Cached so the apply-and-restart UX
   * (#67) doesn't have to re-probe every time the user touches a non-
   * mutable knob.
   */
  caps = $state<DeviceCapabilities | null>(null);

  async open(req: OpenDeviceRequest, caps: DeviceCapabilities | null = null): Promise<void> {
    await this.teardown();
    this.phase = 'connecting';
    this.wsStatus = 'idle';
    this.errorMessage = null;
    this.request = req;
    this.caps = caps;
    try {
      const opened = await openDevice(req);
      this.sessionId = opened.session_id;
      this.client = new FrameClient({
        url: wsUrlFor(opened.ws_url),
        onStatus: (s) => {
          this.wsStatus = s;
        },
        onDecodeError: (err) => {
          this.errorMessage = `decode error: ${err.message}`;
        },
      });
      this.phase = 'open';
      this.state = await fetchSessionState(opened.session_id);
    } catch (err) {
      this.phase = 'error';
      this.errorMessage = err instanceof Error ? err.message : String(err);
      this.client = undefined;
      this.sessionId = undefined;
    }
  }

  async patch(req: PatchSettingsRequest): Promise<void> {
    const id = this.sessionId;
    if (!id) return;
    try {
      const next = await patchSettings(id, req);
      if (next === null) {
        // Session was evicted out from under us; treat as a clean close.
        await this.teardown();
        return;
      }
      this.state = next;
    } catch (err) {
      this.errorMessage = err instanceof Error ? err.message : String(err);
    }
  }

  /**
   * Re-open the current session, merging `overrides` into the prior open
   * request. Used for knobs that the device cannot retune live (sample
   * rate, antenna, bandwidth) — the only safe option is close + reopen.
   *
   * No-op if there is no prior request to merge into.
   */
  async restart(overrides: Partial<OpenDeviceRequest>): Promise<void> {
    if (!this.request) return;
    const next: OpenDeviceRequest = { ...this.request, ...overrides };
    await this.open(next, this.caps);
  }

  async refreshState(): Promise<void> {
    const id = this.sessionId;
    if (!id) return;
    this.state = await fetchSessionState(id);
  }

  /** Tear down the in-memory client + ask the server to close. */
  async teardown(): Promise<void> {
    const c = this.client;
    const s = this.sessionId;
    this.client = undefined;
    this.sessionId = undefined;
    this.state = null;
    this.caps = null;
    this.phase = 'idle';
    this.wsStatus = 'idle';
    if (c) c.close();
    if (s) await closeDevice(s).catch(() => {});
  }
}

export const session = new SessionStore();
