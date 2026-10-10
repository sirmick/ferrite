// The browser end of the event plane: one MQTT connection, local
// fan-out by topic filter.
//
// Everything observable about ferrited is a record on a topic
// (`ferrite/state/<kind>`, `ferrite/state/<kind>/<key>`,
// `ferrite/state/log/<target>` — see server/src/event_plane.rs). This
// module owns the single broker connection and routes each message to
// every local subscriber whose filter matches, so the store mirror,
// the log sidebar and any pane can each subscribe to exactly what they
// render without holding their own socket.
//
// Broker-side we hold one wide subscription (`ferrite/state/#`) and
// only add filters that fall outside it (`ferrite/ingest/#`, …). MQTT
// delivers a message once per matching *broker* subscription, so
// overlapping broker filters would duplicate every log line; local
// matching is where the fine-grained routing happens.
//
// The transport is injected (`MqttLike`) so tests drive a fake; the
// production client is created in `./connect.ts`.

export interface BusMessage {
  readonly topic: string;
  /** UTF-8 payload. Empty for a cleared retained entity. */
  readonly payload: string;
  readonly retain: boolean;
}

export type BusHandler = (m: BusMessage) => void;

/** The slice of an MQTT.js client the bus needs. */
export interface MqttLike {
  readonly connected: boolean;
  on(event: 'connect', cb: () => void): unknown;
  on(event: 'close', cb: () => void): unknown;
  on(
    event: 'message',
    cb: (topic: string, payload: Uint8Array, packet: { retain?: boolean }) => void,
  ): unknown;
  subscribe(filter: string): unknown;
  unsubscribe(filter: string): unknown;
  end(force?: boolean): unknown;
}

/** MQTT topic-filter match: `+` is one level, `#` is the rest. */
export function topicMatches(filter: string, topic: string): boolean {
  if (filter === topic) return true;
  const f = filter.split('/');
  const t = topic.split('/');
  for (let i = 0; i < f.length; i++) {
    const seg = f[i];
    if (seg === '#') return true;
    if (i >= t.length) return false;
    if (seg !== '+' && seg !== t[i]) return false;
  }
  return f.length === t.length;
}

/** Does the broker-side filter `root` already deliver everything
 *  `filter` would? Only the `prefix/#` shape is recognised — enough
 *  for our tree. */
function covers(root: string, filter: string): boolean {
  if (!root.endsWith('/#')) return root === filter;
  const prefix = root.slice(0, -1); // keep the trailing slash
  return filter === root || filter.startsWith(prefix);
}

const ROOT_FILTER = 'ferrite/state/#';
const decoder = new TextDecoder();

export class EventBus {
  private client: MqttLike | null = null;
  private readonly handlers = new Map<string, Set<BusHandler>>();
  /** Filters we asked the broker for, beyond `ROOT_FILTER`. */
  private readonly brokerFilters = new Set<string>();
  private readonly connectListeners = new Set<() => void>();
  private readonly closeListeners = new Set<() => void>();
  private _connected = false;

  get connected(): boolean {
    return this._connected;
  }

  /** Attach a client. Called once in production (`connect.ts`); tests
   *  attach a fake. Safe to call again after `detach()`. */
  attach(client: MqttLike): void {
    this.client = client;
    client.on('connect', () => {
      this._connected = true;
      // MQTT.js resubscribes on reconnect by itself; a fresh attach
      // needs the root + every extra filter registered.
      client.subscribe(ROOT_FILTER);
      for (const f of this.brokerFilters) client.subscribe(f);
      for (const cb of this.connectListeners) cb();
    });
    client.on('close', () => {
      this._connected = false;
      for (const cb of this.closeListeners) cb();
    });
    client.on('message', (topic, payload, packet) => {
      const m: BusMessage = {
        topic,
        payload: decoder.decode(payload),
        retain: packet?.retain === true,
      };
      for (const [filter, set] of this.handlers) {
        if (!topicMatches(filter, topic)) continue;
        for (const h of set) h(m);
      }
    });
    if (client.connected) {
      this._connected = true;
      client.subscribe(ROOT_FILTER);
      for (const f of this.brokerFilters) client.subscribe(f);
      for (const cb of this.connectListeners) cb();
    }
  }

  detach(): void {
    const c = this.client;
    this.client = null;
    this._connected = false;
    c?.end(true);
  }

  /** Subscribe `handler` to every message whose topic matches
   *  `filter`. Returns the unsubscribe function. */
  subscribe(filter: string, handler: BusHandler): () => void {
    let set = this.handlers.get(filter);
    if (!set) {
      set = new Set();
      this.handlers.set(filter, set);
      if (!covers(ROOT_FILTER, filter) && !this.brokerFilters.has(filter)) {
        this.brokerFilters.add(filter);
        if (this._connected) this.client?.subscribe(filter);
      }
    }
    set.add(handler);
    return () => {
      const s = this.handlers.get(filter);
      if (!s) return;
      s.delete(handler);
      if (s.size === 0) {
        this.handlers.delete(filter);
        if (this.brokerFilters.delete(filter) && this._connected) {
          this.client?.unsubscribe(filter);
        }
      }
    };
  }

  /** Fire `cb` on every (re)connect. If already connected, fires now. */
  onConnect(cb: () => void): () => void {
    this.connectListeners.add(cb);
    if (this._connected) cb();
    return () => {
      this.connectListeners.delete(cb);
    };
  }

  onClose(cb: () => void): () => void {
    this.closeListeners.add(cb);
    return () => {
      this.closeListeners.delete(cb);
    };
  }
}

/** The app-wide bus. Stores subscribe here; the page attaches the
 *  transport once on mount. */
export const eventBus = new EventBus();
