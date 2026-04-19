// Ring-buffered log store shared across the UI. Entries come from three
// sources: the `/ws/logs` stream (server tracing), the browser console
// (patched in `init()`), and explicit calls from session/WS code paths.

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
  private nextId = 1;

  push(source: LogSource, level: LogLevel, text: string): void {
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
  }

  clear(): void {
    this.entries = [];
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
  for (const level of ['error', 'warn'] as const) {
    const original = console[level].bind(console);
    console[level] = (...args: unknown[]) => {
      original(...args);
      logs.push('client', level, args.map(formatArg).join(' '));
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

function formatArg(v: unknown): string {
  if (v instanceof Error) return v.stack ?? v.message;
  if (typeof v === 'string') return v;
  try {
    return JSON.stringify(v);
  } catch {
    return String(v);
  }
}
