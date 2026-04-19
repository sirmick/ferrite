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

  async open(req: OpenDeviceRequest): Promise<void> {
    await this.teardown();
    this.phase = 'connecting';
    this.wsStatus = 'idle';
    this.errorMessage = null;
    this.request = req;
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
    this.phase = 'idle';
    this.wsStatus = 'idle';
    if (c) c.close();
    if (s) await closeDevice(s).catch(() => {});
  }
}

export const session = new SessionStore();
