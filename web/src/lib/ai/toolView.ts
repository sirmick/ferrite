// How a tool call reads in the AI transcript: a short name, a one-line
// summary of what it was asked, an optional "why" line, and a one-line
// outcome. Pure, so the panel stays markup and these stay tested.
//
// Two kinds of call show up: the in-app AI's MCP tools
// (`mcp__ferrite__tune`, …) and, from a coding agent mirrored onto the
// bus, built-in tools such as `Bash` (a shell command) or `Read`.

/** `mcp__ferrite__tune` → `tune`; another server's tool keeps its server. */
export function toolLabel(name: string): string {
  const m = /^mcp__([^_]+(?:_[^_]+)*)__(.+)$/.exec(name);
  if (!m) return name;
  return m[1] === 'ferrite' ? m[2] : `${m[1]}:${m[2]}`;
}

/** Is this a shell command (rendered `$ …`)? */
export function isShell(name: string): boolean {
  return name === 'Bash';
}

function parse(s: string): Record<string, unknown> | null {
  if (!s) return null;
  try {
    const v = JSON.parse(s) as unknown;
    return v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/** 7025800 → `7.0258 MHz`, 1200 → `1.2 kHz`, 700 → `700 Hz`. */
export function fmtHz(hz: number): string {
  const a = Math.abs(hz);
  const trim = (x: number, d: number) => String(Number(x.toFixed(d)));
  if (a >= 1e9) return `${trim(hz / 1e9, 6)} GHz`;
  if (a >= 1e6) return `${trim(hz / 1e6, 4)} MHz`;
  if (a >= 1e3) return `${trim(hz / 1e3, 3)} kHz`;
  return `${trim(hz, 1)} Hz`;
}

/** Args shown in the summary — the "why" goes on its own line. */
const WHY_KEYS = ['note', 'description', 'why'];

function fmtValue(key: string, v: unknown): string {
  if (typeof v === 'number' && /_hz$/.test(key)) return fmtHz(v);
  if (typeof v === 'string') return v.length > 48 ? `${v.slice(0, 47)}…` : v;
  if (typeof v === 'number' || typeof v === 'boolean') return String(v);
  const j = JSON.stringify(v);
  return j.length > 48 ? `${j.slice(0, 47)}…` : j;
}

/** One line: what the call was asked to do. */
export function toolSummary(name: string, input: string): string {
  const o = parse(input);
  if (!o) return input.replace(/\s+/g, ' ');
  if (isShell(name) && typeof o.command === 'string') return `$ ${o.command}`;
  if (name === 'Read' && typeof o.file_path === 'string') return o.file_path;
  if ((name === 'Glob' || name === 'Grep') && typeof o.pattern === 'string') {
    return `/${o.pattern}/${typeof o.path === 'string' ? ` in ${o.path}` : ''}`;
  }
  if ((name === 'Edit' || name === 'Write') && typeof o.file_path === 'string') return o.file_path;
  const parts: string[] = [];
  for (const [k, v] of Object.entries(o)) {
    if (WHY_KEYS.includes(k) || v === null || v === undefined || v === '') continue;
    // A lone `name`/`query`/`kind`-style arg reads fine bare.
    parts.push(`${k.replace(/_hz$/, '')}=${fmtValue(k, v)}`);
  }
  return parts.join('  ');
}

/** The optional second line: why the call was made. */
export function toolWhy(input: string): string | null {
  const o = parse(input);
  if (!o) return null;
  for (const k of WHY_KEYS) if (typeof o[k] === 'string' && o[k]) return o[k] as string;
  return null;
}

/** One line: how it went. Errors say what failed; a JSON result with a
 *  `status`/`ok`/`message` says that; anything else stays in the
 *  expander. */
export function resultSummary(result: string | undefined, error: boolean): string | null {
  if (result === undefined) return null;
  const t = result.trim();
  if (!t) return null;
  const o = parse(t);
  if (error) {
    const msg = o ? (o.error ?? o.message) : null;
    const line = typeof msg === 'string' ? msg : t.split('\n')[0];
    return line.length > 160 ? `${line.slice(0, 159)}…` : line;
  }
  if (o) {
    if (typeof o.message === 'string') return o.message;
    if (typeof o.status === 'string') return o.status;
    return null;
  }
  const first = t.split('\n')[0];
  return first.length > 160 ? `${first.slice(0, 159)}…` : first;
}

export interface SignalRow {
  freq_hz: number;
  power_db?: number;
  snr_db?: number;
  bw_hz?: number;
}

/** The `signals` tool's watchlist, for a small table in its card. */
export function signalsOf(name: string, result: string | undefined): SignalRow[] | null {
  if (toolLabel(name) !== 'signals' || !result) return null;
  const o = parse(result.trim());
  const rows = o?.signals;
  if (!Array.isArray(rows)) return null;
  return rows
    .filter((r): r is SignalRow => !!r && typeof (r as SignalRow).freq_hz === 'number')
    .slice(0, 10);
}
