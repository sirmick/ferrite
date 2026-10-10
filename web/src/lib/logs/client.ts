// Server log lines arrive on the event bus as `ferrite/state/log/<target>`
// records (`{target, level, message, at_ms}`, one topic level per
// `::` segment — `decoder::pocsag` is `log/decoder/pocsag`). The log
// sidebar subscribes to the whole subtree; the mute list filters on
// display. Anything else (an agent, `mosquitto_sub`) can subscribe to
// just `ferrite/state/log/decoder/#`.

import { eventBus, type BusMessage, type EventBus } from '$lib/events/bus';
import { logs, type LogLevel } from './store.svelte';

export const LOG_TOPIC_FILTER = 'ferrite/state/log/#';

interface LogRecord {
  target: string;
  level: string;
  message: string;
  at_ms?: number;
}

const LEVELS: ReadonlySet<LogLevel> = new Set(['error', 'warn', 'info', 'debug', 'trace']);

/** One log record → one `server` entry, rendered as `target: message`
 *  so the panel's category filter keeps working unchanged. */
export function ingestLogMessage(m: BusMessage): void {
  if (m.payload.length === 0) return;
  let r: LogRecord;
  try {
    r = JSON.parse(m.payload) as LogRecord;
  } catch {
    return;
  }
  if (typeof r?.message !== 'string') return;
  const lvl = String(r.level ?? 'info').toLowerCase() as LogLevel;
  const level: LogLevel = LEVELS.has(lvl) ? lvl : 'info';
  const target = typeof r.target === 'string' && r.target ? `${r.target}: ` : '';
  logs.push('server', level, `${target}${r.message}`);
}

export function connectServerLogs(bus: EventBus = eventBus): () => void {
  const unsub = bus.subscribe(LOG_TOPIC_FILTER, ingestLogMessage);
  const offConnect = bus.onConnect(() => {
    logs.push('client', 'info', 'event bus connected (server logs live)');
  });
  return () => {
    offConnect();
    unsub();
  };
}
