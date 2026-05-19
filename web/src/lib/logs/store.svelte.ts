// Ring-buffered log store shared across the UI. Entries come from three
// sources: the `/ws/logs` stream (server tracing), the browser console
// (patched in `init()`), and explicit calls from session/WS code paths.

// flowdiag no longer rides the log stream — it has its own channel
// (GET /api/flowdiag for node, in-process for browser; see flowStore /
// FlowPanel). The log store stays log-only.

export type LogLevel = 'error' | 'warn' | 'info' | 'debug' | 'trace';
export type LogSource = 'server' | 'client' | 'vite';

export interface LogEntry {
  id: number;
  t: number;
  source: LogSource;
  level: LogLevel;
  text: string;
  /** Tracing target / category prefix on the line (e.g. `decoder::pocsag`,
   *  `flowdiag`, `ferrited::main`). Parsed from `text` at push time so
   *  the panel filter can group by it. `null` for lines with no parseable
   *  prefix (free-text client output, vite messages). */
  category: string | null;
}

/** Match a `target: message` prefix at the start of a line. The target
 *  is whatever the tracing emitter set (`tracing::info!(target: "X", …)`)
 *  or the module path that defaults to. We accept colons inside the
 *  target itself (`decoder::pocsag`, `ferrite_blocks::audio_nr`) by
 *  splitting on the FIRST `: ` (with the trailing space) — `decoder::`
 *  contains `::` not `: `, so the split picks the right boundary. */
const CATEGORY_RE = /^([A-Za-z_][\w:]*)\s*:\s/;

export function categoryOfText(text: string): string | null {
  const m = CATEGORY_RE.exec(text);
  return m ? m[1] : null;
}

const MAX_ENTRIES = 500;
const MUTED_STORAGE_KEY = 'ferrite.logs.mutedCategories';

class LogStore {
  entries = $state<LogEntry[]>([]);
  /** Number of error-level entries logged since the user last acked.
   *  Drives the red dot on the sidebar's Logs tab so errors don't go
   *  unseen while the tab isn't active. Call `ackErrors()` on tab-open. */
  unreadErrors = $state<number>(0);
  /** Categories the user has muted. Mute = drop from on-screen
   *  entries display only; the line is still pushed (so unmuting
   *  later doesn't reveal a hole) and still forwarded to the server.
   *  Persisted to localStorage so a one-time mute (e.g. of
   *  `flowdiag`) survives reloads — otherwise the chatty categories
   *  would flood the panel on every fresh load. */
  mutedCategories = $state<Set<string>>(new Set());
  /** Categories ever seen on this session. The filter dropdown
   *  enumerates this set so the user can mute things they've actually
   *  observed rather than a hard-coded list. */
  knownCategories = $state<Set<string>>(new Set());
  private nextId = 1;

  constructor() {
    if (typeof localStorage === 'undefined') return;
    try {
      const raw = localStorage.getItem(MUTED_STORAGE_KEY);
      if (raw) {
        const arr = JSON.parse(raw) as unknown;
        if (Array.isArray(arr)) {
          this.mutedCategories = new Set(arr.filter((s): s is string => typeof s === 'string'));
        }
      }
    } catch {
      /* corrupt; fall back to empty */
    }
  }

  toggleMute(category: string): void {
    const next = new Set(this.mutedCategories);
    if (next.has(category)) next.delete(category);
    else next.add(category);
    this.mutedCategories = next;
    this.persistMute();
  }

  unmuteAll(): void {
    if (this.mutedCategories.size === 0) return;
    this.mutedCategories = new Set();
    this.persistMute();
  }

  private persistMute(): void {
    if (typeof localStorage === 'undefined') return;
    try {
      localStorage.setItem(MUTED_STORAGE_KEY, JSON.stringify([...this.mutedCategories]));
    } catch {
      /* quota / private mode — silent */
    }
  }

  push(source: LogSource, level: LogLevel, text: string): void {
    const category = categoryOfText(text);
    if (category && !this.knownCategories.has(category)) {
      // Mutate via fresh Set so $state tracks the change.
      const next = new Set(this.knownCategories);
      next.add(category);
      this.knownCategories = next;
    }
    const entry: LogEntry = {
      id: this.nextId++,
      t: Date.now(),
      source,
      level,
      text,
      category,
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
    // Don't clear `knownCategories` — the filter dropdown should keep
    // listing them so the user can re-mute after a clear.
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
