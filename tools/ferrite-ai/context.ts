// What the AI knows about the station, built here from ferrited's own
// state so every driver — the AI panel, `ferrite-ctl ai-prompt`, a REST
// caller — gets the same context. (The panel used to assemble it in the
// browser, so the CLI's turns ran blind.)
//
//   system prompt  = mode prompt → driver notes (what the SDR can do)
//                    → station notes (what the operator did with it).
//                    Fixed for a session's life.
//   turn context   = live source state → preset / block notes (only when
//                    they changed since the session last saw them)
//                    → the clock. Sent ahead of each user message.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export interface ContextDeps {
  /** ferrited base URL, e.g. `http://127.0.0.1:12001`. */
  ferrited: string;
  /** Repo root — driver notes live in the web app's SDR presets. */
  home: string;
  /** Station notes file (operator-edited, shared by every driver). */
  setupFile: string;
}

// ── station notes ──────────────────────────────────────────────────────

export function readSetup(file: string): string {
  try {
    return existsSync(file) ? readFileSync(file, "utf-8") : "";
  } catch {
    return "";
  }
}

export function writeSetup(file: string, text: string): void {
  writeFileSync(file, text.endsWith("\n") || !text ? text : `${text}\n`);
}

// ── ferrited reads ─────────────────────────────────────────────────────

async function getJson(base: string, path: string): Promise<unknown> {
  try {
    const r = await fetch(`${base}${path}`, { signal: AbortSignal.timeout(5000) });
    return r.ok ? await r.json() : null;
  } catch {
    return null;
  }
}

type Obj = Record<string, unknown>;
const obj = (v: unknown): Obj => (v && typeof v === "object" && !Array.isArray(v) ? (v as Obj) : {});
const str = (v: unknown): string => (typeof v === "string" ? v : "");
const num = (v: unknown): number | undefined => (typeof v === "number" && Number.isFinite(v) ? v : undefined);

/** The driver's operator notes (`web/src/lib/controls/sdr-presets/<driver>.json`
 *  `ai_operator_notes` — the same file the UI tooltips read). */
export function driverNotes(home: string, driverKey: string): string {
  if (!driverKey) return "";
  const file = join(home, "web/src/lib/controls/sdr-presets", `${driverKey.toLowerCase()}.json`);
  try {
    return str(obj(JSON.parse(readFileSync(file, "utf-8")))["ai_operator_notes"]).trim();
  } catch {
    return "";
  }
}

export interface Station {
  driverKey: string;
  deviceLabel: string;
  source: Obj | null;
  presetNotes: string;
  blockNotes: BlockNote[];
}

export interface BlockNote {
  id: string;
  type_name: string;
  ai_notes: string;
  params: Array<{ key: string; label: string; ai_notes: string }>;
}

/** One read of everything the context needs. Missing pieces are empty,
 *  never an error — a turn must not fail because a read did. */
export async function readStation(base: string): Promise<Station> {
  const [caps, source, flowgraph, blocks] = await Promise.all([
    getJson(base, "/api/source/capabilities"),
    getJson(base, "/api/source"),
    getJson(base, "/api/flowgraph"),
    getJson(base, "/api/pipeline/blocks"),
  ]);
  const c = obj(caps);
  const hw = str(c["kind"]) === "hardware" ? obj(c["capabilities"]) : {};
  const blockNotes: BlockNote[] = (Array.isArray(blocks) ? blocks : [])
    .map((b) => {
      const spec = obj(obj(b)["spec"]);
      return {
        id: str(obj(b)["id"]),
        type_name: str(obj(b)["type_name"]),
        ai_notes: str(spec["ai_notes"]).trim(),
        params: (Array.isArray(spec["params"]) ? spec["params"] : []).map((p) => ({
          key: str(obj(p)["key"]),
          label: str(obj(p)["label"]),
          ai_notes: str(obj(p)["ai_notes"]).trim(),
        })),
      };
    })
    // Internal taps (`__narrow_fft_chan`, …) aren't the operator's blocks.
    .filter((b) => !b.id.startsWith("__"));
  return {
    driverKey: str(hw["driver_key"]),
    deviceLabel: str(obj(hw["info"])["label"]),
    source: source ? obj(source) : null,
    presetNotes: str(obj(flowgraph)["ai_notes"]).trim(),
    blockNotes,
  };
}

// ── rendering ──────────────────────────────────────────────────────────

export function systemPrompt(modePrompt: string, driver: string, setup: string): string {
  let out = modePrompt;
  if (driver.trim()) out += `\n\n## Active SDR — driver-specific operator notes\n\n${driver.trim()}\n`;
  if (setup.trim()) out += `\n\n## Operator-supplied radio setup\n\n${setup.trim()}\n`;
  return out;
}

const MHz = (hz: number) => `${(hz / 1e6).toFixed(3)} MHz`;

export function renderSource(st: Station): string {
  if (!st.source) return "";
  const p = obj(st.source["params"]);
  const lines: string[] = [];
  if (st.deviceLabel) lines.push(`- Device: ${st.deviceLabel}`);
  lines.push(`- Source type: \`${str(st.source["type"]) || "?"}\``);
  const f = num(p["center_freq_hz"]);
  if (f !== undefined) lines.push(`- Centre frequency: **${MHz(f)}**`);
  const rate = num(p["sample_rate_hz"]);
  if (rate !== undefined) lines.push(`- Sample rate: ${(rate / 1e6).toFixed(3)} MS/s`);
  const bw = num(p["bandwidth_hz"]);
  if (bw !== undefined) lines.push(`- Analog bandwidth: ${MHz(bw)}`);
  if (str(p["antenna"])) lines.push(`- Antenna: ${str(p["antenna"])}`);
  const gain = num(p["gain_db"]);
  if (gain !== undefined) lines.push(`- Gain: ${gain.toFixed(1)} dB`);
  if (typeof p["agc"] === "boolean") {
    lines.push(`- AGC: ${p["agc"] ? "enabled (driver manages gain)" : "disabled (manual gain in effect)"}`);
  }
  return (
    `## Current radio state — live snapshot from ferrited\n\n` +
    `The source state as this turn starts. Reason from these values, not ` +
    `from memory of earlier tune commands — preset loads or other callers ` +
    `may have changed it since.\n\n${lines.join("\n")}\n`
  );
}

export function renderNotes(st: Station): string {
  let out = st.presetNotes ? `## Active preset — AI notes\n\n${st.presetNotes}\n` : "";
  const sections: string[] = [];
  for (const b of st.blockNotes) {
    const params = b.params.filter((p) => p.ai_notes);
    if (!b.ai_notes && params.length === 0) continue;
    let body = `### \`${b.id}\` — ${b.type_name}\n`;
    if (b.ai_notes) body += `\n${b.ai_notes}\n`;
    if (params.length) {
      body += `\nParams:\n`;
      for (const p of params) body += `- \`${p.key}\`${p.label ? ` (${p.label})` : ""} — ${p.ai_notes}\n`;
    }
    sections.push(body);
  }
  if (sections.length) out += `${out ? "\n" : ""}## Active pipeline — block notes\n\n${sections.join("\n")}`;
  return out;
}

export function renderClock(now: Date, timeZone: string | undefined): string {
  const tz = timeZone || Intl.DateTimeFormat().resolvedOptions().timeZone;
  let local = "";
  try {
    local = now.toLocaleString("en-GB", {
      timeZone: tz,
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      hour12: false,
      timeZoneName: "short",
    });
  } catch {
    /* unknown zone: UTC line only */
  }
  return (
    `## Local clock + region\n\n` +
    (local ? `- Operator's local time: ${local}\n` : "") +
    `- UTC: ${now.toISOString()}\n` +
    `- IANA timezone: ${tz} (approximate region — use it for propagation, ` +
    `MUF, broadcast-band makeup and band-condition reasoning)\n`
  );
}

/** The `<turn-context>` block sent ahead of the user's words. `seen`
 *  is per session: preset / block notes ride along only when they
 *  changed since the session last saw them. */
export function turnContext(
  st: Station,
  seen: { notes?: string },
  now: Date,
  timeZone: string | undefined,
): string {
  const parts = [renderSource(st)];
  const notes = renderNotes(st);
  if (notes && notes !== seen.notes) {
    parts.push(notes);
    seen.notes = notes;
  }
  parts.push(renderClock(now, timeZone));
  return `<turn-context>\n${parts.filter(Boolean).join("\n").trim()}\n</turn-context>`;
}
