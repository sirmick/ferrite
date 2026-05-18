# Session kickoff: collapse three dual-authority "drift class" bugs

> Paste this as the opening prompt for the implementation session.
> Source of record: `docs/08-roadmap.md` → "Forward work — 1.0" →
> "Structural simplifications (from the 2026-05-17 cleanup audit)".

---

# Session goal: collapse three dual-authority "drift class" bugs to single sources of truth

Context: docs/08-roadmap.md → "Forward work — 1.0" → "Structural simplifications
(from the 2026-05-17 cleanup audit)" lists three refactors that each kill a
*category* of desync bug, not an instance. Do all three this session. They are
independent — commit each as its own conventional commit; do not interleave.

These are the SAME refactor pattern three times: two stores hold one truth, a
mirror/dual-write drifts, the symptom you hit is one of many. Fix = one writer,
one reader-of-record. Reuse existing constructs; invent no new transport.

────────────────────────────────────────────────────────────────────────
WORKSTREAM 1 — Single source of truth for per-driver / preset tables
────────────────────────────────────────────────────────────────────────
Problem: the SDRplay IF-filter ladder is hand-duplicated across the Rust
server, ferrite-ctl (which self-flags it `DUPLICATE-OF`), and web
`sdr-presets` JSON; sample slugs are duplicated too (see the
sample-consolidation note / project memory).
Approach: define each table ONCE in Rust; generate the CLI + web copies via
build script / serde export (a `build.rs` or an xtask that serializes the Rust
table to the JSON the web side already consumes — keep the consumed shape
identical so the web diff is zero). Grep every `DUPLICATE-OF` marker; each must
end up generated, not hand-kept. Add a CI check that the generated artifact is
up to date (fails if someone edits Rust without regenerating).
Done when: one Rust definition; CLI + web read generated output; no
`DUPLICATE-OF` markers remain; regeneration is a documented one-liner.

────────────────────────────────────────────────────────────────────────
WORKSTREAM 2 — Running pipeline = single doc authority
────────────────────────────────────────────────────────────────────────
Problem: `AppState.preset_doc` and the runtime's `applied_doc` both hold the
flowgraph; live reconfigure mutates the runtime then mirrors the delta back
under two locks. The `apply_block_params` desync window patched in the audit
was one symptom of this dual-write.
Approach: while a pipeline is LIVE, `list_blocks` / `GET /api/flowgraph` read
*through* the runtime's `applied_doc` (one reader-of-record); fall back to
`preset_doc` only when stopped. Remove the delta-mirror-back path and the
second lock. Audit every `preset_doc` read for "should this be applied_doc
when running?".
Constraint: the runtime is compiled to wasm too — after any runtime/blocks
change run `pnpm wasm:build`, else the browser half breaks with a confusing
WireTypeMatch. Server change ⇒ `cargo build --release -p ferrited` + restart
(run.sh uses the prebuilt release binary; it will NOT rebuild on its own).
Done when: one writer, one reader-of-record; dual-write + lock-ordering
hazard gone; node↔browser block params still consistent (the
`pipeline.blocks` merge still works); existing reconfigure/e2e tests green.

────────────────────────────────────────────────────────────────────────
WORKSTREAM 3 — Single-authority AI conversation state
                (transcript lifecycle → server-side; AI context lifecycle)
────────────────────────────────────────────────────────────────────────
Problem: the visible transcript (browser localStorage, 200-turn cap) and the
LLM reasoning context (sidecar SDK `session_id`, per-connection, memory-only)
have independent lifetimes and no binding. Sidecar restart / mode change / UI
`/clear` silently desyncs them; the browser replays a stale
`resume_session_id`.

HARD INVARIANTS (do not violate — these are why the design is shaped this way):
  • The transcript log MUST live in the same place as the LLM context and
    reset/resume WITH it. The SDK session physically lives in the sidecar
    (~/.claude); therefore the authority is the SIDECAR, keyed by
    `session_id`. "Server-side" here means *off the browser, into the
    sidecar* — NOT a ferrited-owned store. A ferrited store would be a
    second authority = the bug. ferrited stays a transparent /ws/chat proxy.
  • The browser is a pure VIEW. localStorage demotes to a first-paint cache
    that the authoritative snapshot overwrites.

Approach (phased; P1 alone kills the user-visible confusion):
  P1 — Sidecar persists the raw event stream per session to
       `${FERRITE_AI_STATE_DIR}/<session_id>.jsonl`. On /ws/chat connect
       (and on an explicit `request_snapshot` control), the sidecar replays
       the COMPLETE current-session transcript as a `conversation_snapshot`
       before going live. The browser store folds it with its EXISTING
       reducer and *replaces* local turns (do not write a second renderer).
       An unresumable session emits `session_reset`; the UI clears
       coherently and shows an honest "reasoning context reset — history
       preserved, assistant won't remember the above" banner instead of a
       stale log. Reuse the existing `meta` Chunk type for the banner.
  P2 — Boot reload of the session file; `FERRITE_AI_STATE_DIR` wired in
       run.sh + .gitignore, mirroring the FERRITE_SCREENSHOTS_DIR
       convention exactly. Retire the per-process /tmp transcript.
  P3 — Unified reset: one control that drops the SDK session AND rolls a
       new transcript file together (mode change + `/clear` both route
       here); browser stops sending its own `resume_session_id`; the
       sidecar owns the binding.
Files: tools/ferrite-ai/index.ts (authority, snapshot, reset),
       web/src/lib/ai/store.svelte.ts (hydrate-and-replace, cache demotion,
       /clear → control), server/src/routes.rs ws_chat_proxy (stays
       transparent — verify a pre-live snapshot burst forwards fine),
       run.sh + .gitignore.
Done when: clear the UI or restart the sidecar → the panel reflects the
sidecar's record (snapshot replay or honest reset), never a silent stale
divergence; transcript and context cannot reset independently.

────────────────────────────────────────────────────────────────────────
GATES (every workstream)
────────────────────────────────────────────────────────────────────────
• Tests first where a contract changes (reconfigure path, snapshot fold,
  table regen check).
• runtime/blocks touched ⇒ `pnpm wasm:build`. Server Rust touched ⇒
  `cargo build --release -p ferrited` then restart the stack (./stop.sh +
  ./run.sh; clean relaunch clears any USB-wedge from a hard kill).
• Before any push: run the full GitHub Actions matrix locally and get it
  green (non-negotiable).
• One conventional commit per workstream, Co-Authored-By trailer. Do not
  push unless asked.
• If a workstream balloons, ship WS3-P1 first (it's the user-facing pain)
  and re-scope the rest — say so explicitly rather than half-finishing all
  three.
