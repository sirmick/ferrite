// The AI's context is built from ferrited's state, the same for every
// driver. These pin the rendering (pure) — the reads are thin fetches.

import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, it } from "node:test";
import { fileURLToPath } from "node:url";

import {
  driverNotes,
  readSetup,
  renderSource,
  systemPrompt,
  turnContext,
  writeSetup,
  type Station,
} from "../context.ts";

const HOME = resolve(fileURLToPath(import.meta.url), "../../../..");

const station = (over: Partial<Station> = {}): Station => ({
  driverKey: "SDRplay",
  deviceLabel: "SDRplay RSPdx",
  source: {
    type: "SoapySource",
    params: { center_freq_hz: 7_025_800, sample_rate_hz: 2_400_000, antenna: "Antenna C", gain_db: 50, agc: false },
  },
  presetNotes: "CW lives in the bottom 70 kHz.",
  blockNotes: [{ id: "cw", type_name: "CwDemod", ai_notes: "", params: [{ key: "wpm", label: "WPM", ai_notes: "speed" }] }],
  ...over,
});

describe("turn context", () => {
  it("carries the live source state", () => {
    const s = renderSource(station());
    assert.match(s, /Device: SDRplay RSPdx/);
    assert.match(s, /\*\*7\.026 MHz\*\*/);
    assert.match(s, /Antenna: Antenna C/);
    assert.match(s, /AGC: disabled/);
  });

  it("sends preset and block notes only when they changed", () => {
    const seen = {};
    const now = new Date("2026-10-11T15:00:00Z");
    const first = turnContext(station(), seen, now, "UTC");
    assert.match(first, /Active preset — AI notes/);
    assert.match(first, /`wpm` \(WPM\) — speed/);
    const again = turnContext(station(), seen, now, "UTC");
    assert.doesNotMatch(again, /Active preset/);
    const changed = turnContext(station({ presetNotes: "different" }), seen, now, "UTC");
    assert.match(changed, /different/);
  });

  it("has a clock in the caller's zone", () => {
    const t = turnContext(station(), {}, new Date("2026-10-11T15:00:00Z"), "America/Los_Angeles");
    assert.match(t, /IANA timezone: America\/Los_Angeles/);
    assert.match(t, /UTC: 2026-10-11T15:00:00\.000Z/);
  });
});

describe("system prompt", () => {
  it("is mode prompt, then driver notes, then station notes", () => {
    const p = systemPrompt("MODE", "DRIVER", "STATION");
    assert.ok(p.indexOf("MODE") < p.indexOf("DRIVER") && p.indexOf("DRIVER") < p.indexOf("STATION"));
    assert.equal(systemPrompt("MODE", "", " "), "MODE");
  });

  it("reads driver notes from the web app's SDR preset, case-insensitively", () => {
    assert.ok(driverNotes(HOME, "SDRplay").length > 0);
    assert.equal(driverNotes(HOME, "no-such-driver"), "");
    assert.equal(driverNotes(HOME, ""), "");
  });
});

describe("station notes", () => {
  it("round-trip through the one file", () => {
    const f = join(mkdtempSync(join(tmpdir(), "ferrite-ai-")), "setup.md");
    assert.equal(readSetup(f), "");
    writeSetup(f, "Antenna C: longwire");
    assert.equal(readSetup(f), "Antenna C: longwire\n");
  });
});
