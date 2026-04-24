// Ring-buffered log store shared across the UI. Entries come from three
// sources: the `/ws/logs` stream (server tracing), the browser console
// (patched in `init()`), and explicit calls from session/WS code paths.

import { flow, parseFlowdiagLine } from './flowStore.svelte';

export type LogLevel = 'error' | 'warn' | 'info' | 'debug' | 'trace';
export type LogSource = 'server' | 'client' | 'vite';

export interface LogEntry {
  id: number;
  t: number;
  source: LogSource;
  level: LogLevel;
  text: string;
}

const MAX_ENTRIES = 500;

class LogStore {
  entries = $state<LogEntry[]>([]);
  /** Number of error-level entries logged since the user last acked.
   *  Drives the red dot on the sidebar's Logs tab so errors don't go
   *  unseen while the tab isn't active. Call `ackErrors()` on tab-open. */
  unreadErrors = $state<number>(0);
  private nextId = 1;

  push(source: LogSource, level: LogLevel, text: string): void {
    // Peel off `flowdiag side=… {json}` lines and feed the Flow store.
    // Still push them into the log stream so the raw JSON is greppable
    // on the Logs tab; the tab also forwards client-side ones to the
    // server so ferrited's stdout gets the full record.
    const flowdiag = parseFlowdiagLine(text);
    if (flowdiag) flow.ingest(flowdiag.side, flowdiag.snap);
    const entry: LogEntry = {
      id: this.nextId++,
      t: Date.now(),
      source,
      level,
      text,
    };
    const next =
      this.entries.length >= MAX_ENTRIES
        ? this.entries.slice(-(MAX_ENTRIES - 1))
        : this.entries.slice();
    next.push(entry);
    this.entries = next;
    if (level === 'error') this.unreadErrors += 1;
    // Forward client-origin entries to the server so they land in
    // ferrited's stdout alongside server-side tracing. Only forward
    // `client` source — echoing `server` back would trivially loop.
    if (source === 'client') forwardToServer(level, text);
  }

  clear(): void {
    this.entries = [];
    this.unreadErrors = 0;
  }

  /** Mark the error badge as read. Called when the Logs tab gains focus. */
  ackErrors(): void {
    if (this.unreadErrors !== 0) this.unreadErrors = 0;
  }
}

export const logs = new LogStore();

// Parse `[LEVEL] module: message` as emitted by the server broadcast layer.
const SERVER_LINE = /^\[(TRACE|DEBUG|INFO|WARN|ERROR)\]\s*(.*)$/;

export function pushServerLine(line: string): void {
  const m = SERVER_LINE.exec(line);
  if (m) {
    logs.push('server', m[1].toLowerCase() as LogLevel, m[2]);
  } else {
    logs.push('server', 'info', line);
  }
}

let consolePatched = false;

export function patchConsole(): void {
  if (consolePatched || typeof window === 'undefined') return;
  consolePatched = true;
  // Patch the full common set. `log` and `info` both map to LogLevel
  // 'info'; `debug` maps to 'debug'. Upstream `RUST_LOG=browser=warn`
  // can mute the chatty levels server-side if needed.
  const map: Record<'log' | 'info' | 'debug' | 'warn' | 'error', LogLevel> = {
    log: 'info',
    info: 'info',
    debug: 'debug',
    warn: 'warn',
    error: 'error',
  };
  for (const key of Object.keys(map) as Array<keyof typeof map>) {
    const original = console[key].bind(console);
    console[key] = (...args: unknown[]) => {
      original(...args);
      logs.push('client', map[key], args.map(formatArg).join(' '));
    };
  }
  window.addEventListener('error', (e) => {
    logs.push('client', 'error', `${e.message} @ ${e.filename}:${e.lineno}`);
  });
  window.addEventListener('unhandledrejection', (e) => {
    const reason = e.reason instanceof Error ? e.reason.message : String(e.reason);
    logs.push('client', 'error', `unhandled rejection: ${reason}`);
  });
}

/**
 * Fire-and-forget POST of one client-side log entry to the server.
 * Dev-only diagnostic — makes `logs.push('client', …)` and patched
 * `console.*` calls visible in ferrited's stdout.
 *
 * Guards:
 * - SSR: `fetch` isn't reliable during SvelteKit prerender; bail when
 *   `window` is absent.
 * - Recursion: a POST that fails must *not* end up in `logs.push`
 *   again (would re-enter this function). We swallow the promise with
 *   `.catch(() => {})` — never `console.*` in here.
 * - Backpressure: `keepalive: true` lets the final flushes survive a
 *   page unload without queueing; no batching yet (log volume is low).
 */
function forwardToServer(level: LogLevel, text: string): void {
  if (typeof window === 'undefined') return;
  // `trace` isn't an HTTP method we care to proxy; collapse to debug.
  const forwardLevel: LogLevel = level === 'trace' ? 'debug' : level;
  try {
    void fetch('/api/debug/log', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ level: forwardLevel, source: 'client', message: text }),
      keepalive: true,
    }).catch(() => {
      /* best-effort; never surface into logs.push again */
    });
  } catch {
    /* best-effort */
  }
}

function formatArg(v: unknown): string {
  if (v instanceof Error) return v.stack ?? v.message;
  if (typeof v === 'string') return v;
  try {
    return JSON.stringify(v);
  } catch {
    return String(v);
  }
}
