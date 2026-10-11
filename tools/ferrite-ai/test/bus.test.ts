// The session-bus helpers: which conversation each mode continues (read
// back from the `ai` log) and the graphic a tool result points at.

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { analysisIdOf, latestFromRecords } from "../bus.ts";

const rec = (data: Record<string, unknown>) => ({ data });
const user = (session: string, mode: string) => rec({ type: "user", source: "ferrite-ai", session, mode });

describe("latestFromRecords", () => {
  it("is each mode's newest session", () => {
    const m = latestFromRecords([user("a", "explorer"), user("b", "chat"), user("c", "explorer")]);
    assert.deepEqual(Object.fromEntries(m), { explorer: "c", chat: "b" });
  });

  it("a mode reset forgets that mode; an all-modes reset or a clear forgets every mode", () => {
    const base = [user("a", "explorer"), user("b", "chat")];
    const one = latestFromRecords([...base, rec({ type: "reset", source: "ferrite-ai", mode: "chat" })]);
    assert.deepEqual(Object.fromEntries(one), { explorer: "a" });
    const all = latestFromRecords([...base, rec({ type: "reset", source: "ferrite-ai", mode: null })]);
    assert.equal(all.size, 0);
    const cleared = latestFromRecords([...base, rec({ type: "clear" })]);
    assert.equal(cleared.size, 0);
  });

  it("ignores other drivers' records", () => {
    const m = latestFromRecords([
      user("a", "explorer"),
      rec({ type: "user", source: "claude-code", session: "x", mode: "explorer" }),
    ]);
    assert.equal(m.get("explorer"), "a");
  });
});

describe("analysisIdOf", () => {
  it("reads the id from a JSON tool result", () => {
    assert.equal(analysisIdOf('{"analysis_id":"an-1-2","peaks":[]}'), "an-1-2");
  });
  it("survives a clipped result", () => {
    assert.equal(analysisIdOf('{"analysis_id": "sw-9", "candidates": [{"name": "CW"'), "sw-9");
  });
  it("is undefined for plain text or no id", () => {
    assert.equal(analysisIdOf("tuned to 7.025 MHz"), undefined);
    assert.equal(analysisIdOf('{"ok":true}'), undefined);
  });
});
