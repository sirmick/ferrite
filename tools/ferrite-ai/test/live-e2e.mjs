#!/usr/bin/env node
// Live end-to-end check of the in-app AI against a running stack (not
// part of `npm test` — it spends a real, small Claude turn):
//
//   node tools/ferrite-ai/test/live-e2e.mjs [ferrited-url]
//
// One prompt over the REST front door, on a fresh conversation, asking
// for one MCP call. Then the `ai` log must hold, in order and for that
// session: the user prompt, the tool call, its result, a reply, and a
// clean turn end — the records every AI panel renders. Uses Haiku to
// keep it cheap; any radio state works (it only reads `status`).

const base = (process.argv[2] ?? process.env.FERRITE_CTL_CONNECT ?? "http://127.0.0.1:12001").replace(/\/$/, "");

function fail(msg) {
  console.error(`FAIL: ${msg}`);
  process.exit(1);
}

const t0 = Date.now();
const r = await fetch(`${base}/api/ai/prompt`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({
    text: "Call the status tool exactly once, then reply with one short sentence naming the active preset.",
    mode: "explorer",
    model: "claude-haiku-5-5",
    new_session: true,
    wait: true,
  }),
});
const turn = await r.json();
if (!r.ok) fail(`prompt → HTTP ${r.status}: ${JSON.stringify(turn)}`);
if (turn.status !== "done") fail(`turn ended ${turn.status}: ${turn.error ?? ""}`);
console.log(`turn done in ${((Date.now() - t0) / 1000).toFixed(1)} s, session ${turn.session_id}`);
console.log(`reply: ${turn.text.trim()}`);

// The bus is best-effort async; give the last records a moment.
await new Promise((res) => setTimeout(res, 1000));
const snap = await (await fetch(`${base}/api/state`)).json();
const recs = (snap.kinds?.ai?.recent ?? [])
  .map((x) => x.data)
  .filter((d) => d?.session === turn.session_id);
const types = recs.map((d) => d.type);
console.log(`records: ${types.join(" → ")}`);

const at = (type, pred = () => true, from = 0) => {
  const i = recs.findIndex((d, k) => k >= from && d.type === type && pred(d));
  if (i < 0) fail(`no ${type} record${from ? " after the previous step" : ""}`);
  return i;
};
const u = at("user");
const use = at("tool_use", (d) => /status$/.test(d.name ?? ""), u);
const res = at("tool_result", (d) => d.tool_id === recs[use].tool_id, use);
if (recs[res].is_error) fail(`status tool failed: ${recs[res].text}`);
const txt = at("text", () => true, res);
const end = at("turn_end", (d) => d.status === "done", txt);
if (end !== recs.length - 1) fail("records after turn_end");
for (const d of recs) {
  if (d.source !== "ferrite-ai") fail(`record from ${d.source}`);
  if (d.mode !== "explorer") fail(`record mode ${d.mode}`);
}
const thoughts = types.filter((t) => t === "thought").length;
console.log(`OK — prompt, tool call + result, reply, turn end${thoughts ? `, ${thoughts} thinking block(s)` : ""}`);
