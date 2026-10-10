import { describe, expect, it } from 'vitest';

import { ingestLogMessage } from './client';
import { logs } from './store.svelte';

describe('ingestLogMessage', () => {
  it('renders a log record as a categorised server entry', () => {
    logs.clear();
    ingestLogMessage({
      topic: 'ferrite/state/log/decoder/pocsag',
      payload: JSON.stringify({
        target: 'decoder::pocsag',
        level: 'WARN',
        message: 'bad crc',
        at_ms: 1,
      }),
      retain: false,
    });
    const e = logs.entries.at(-1)!;
    expect(e.source).toBe('server');
    expect(e.level).toBe('warn');
    expect(e.category).toBe('decoder::pocsag');
    expect(e.text).toBe('decoder::pocsag: bad crc');
  });

  it('drops malformed payloads without logging', () => {
    logs.clear();
    ingestLogMessage({ topic: 'ferrite/state/log/x', payload: 'not json', retain: false });
    ingestLogMessage({ topic: 'ferrite/state/log/x', payload: '', retain: false });
    expect(logs.entries).toHaveLength(0);
  });
});
