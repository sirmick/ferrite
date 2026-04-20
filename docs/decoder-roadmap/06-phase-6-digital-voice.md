# Phase 6 — Digital voice + native-helper protocol

**Sketch.** The two-track answer to digital voice.

## Goal

Two deliverables that are better thought of as sibling work than as
phases. Either can ship independently.

### Track A — Patent-free digital voice in WASM

**M17 and FreeDV** — both use Codec2, both patent-free, both shippable
in the browser.

Vendor `codec2` into `blocks/native/codec2/` using the Phase 2
infrastructure. Write:

| Block            | Vendor source | Notes                              |
|------------------|---------------|------------------------------------|
| `M17Decoder`     | codec2 + M17 framing | 4FSK frame sync + Codec2 voice decode |
| `FreeDv700dDecoder` | codec2       | HF digital voice                    |
| `FreeDv2020Decoder` | codec2       | Higher-rate FreeDV                  |

Chains are `FmDemod → Resample(8k) → M17Decoder → AudioSink` or
`SsbDemod → Resample(8k) → FreeDv700dDecoder → AudioSink`.

**Strategic value:** only browser-based SDR with native digital-voice
decode that ships nothing patent-encumbered. Marketing matters.

### Track B — Native-helper protocol for the patent-encumbered modes

**`ferrite-helper` protocol** — a simple WebSocket service Ferrite can
talk to, running on the same SBC as `ferrited`, that wraps an
external decoder binary (dsd-fme, welle.io DAB, dream DRM, wsjt-x if
ever needed, anything else we don't want to ship in-WASM).

Protocol shape (rough):

```
browser                ferrited              ferrite-helper (local WS)
   |<-- audio --------→|
   |                   |<-- decoded audio + events -- (helper runs dsd-fme)
   |<-- rendered UI --|
```

Ferrite's side:
- Flowgraph can emit a `real_f32` audio stream (or `iq_f32`) into a
  `ferrite-helper://dmr` URL as a sink.
- Helper decodes, streams back PCM + JSON events over local loopback.
- Ferrite treats that as another source + events stream. Flowgraph
  DSL gains a `HelperClient` block.

This is the **legal exit hatch**: anything Ferrite can't ship in the
browser bundle (DMR/P25/NXDN vocoders, DAB HE-AAC, DRM xHE-AAC,
proprietary ham modes) can run as a user-installed native helper
without Ferrite itself distributing patent-encumbered code. The user
installs `dsd-fme` via apt, points Ferrite at it, and gets DMR decode
in the browser UI — with no mbelib in our source tree.

### Architectural considerations

- **The `HelperClient` block is a thin RPC.** Don't let helper-specific
  knowledge leak into blocks/. One generic block with a
  `helper_url` param and a generic event schema.
- **Helper discovery.** Option A: `ferrited` advertises installed
  helpers via its control API (inspects `$PATH` for known binaries at
  startup). Option B: user configures helper paths in ferrited config.
  Start with B — simpler, no probing weirdness.
- **Sandboxing.** Helpers are user-installed OS binaries. `ferrited`
  should spawn them in a locked-down subprocess (no network beyond
  what the protocol needs, filesystem-read-only, etc.). Low priority
  for LAN-trust v0.1 model, but worth a CLAUDE-MD-worth line in
  security docs.

## Ship list

| Thing                            | Via            |
|----------------------------------|----------------|
| M17 voice in-browser             | Track A vendor |
| FreeDV 700D/2020                 | Track A vendor |
| DMR audio + metadata             | Track B helper (dsd-fme) |
| P25 Phase 1/2                    | Track B helper |
| NXDN, D-STAR, YSF                | Track B helper |
| DAB / DAB+                       | Track B helper (welle.io) |
| DRM                              | Track B helper (dream)   |

## Ordering

Track A before Track B. Track A proves Codec2-in-WASM and gives a
user-visible feature. Track B is protocol design work with fewer
demo-able intermediate states; easier once we've done one digital-
voice block well.

## Risks

- **Codec2 size in WASM.** Codec2 is a few thousand LOC of C; not
  large. Bundle with the WASM ecosystem infrastructure from Phase 2.
- **Helper protocol versioning.** Once external helpers speak our
  protocol, we can't change it lightly. Design v1 carefully; use a
  semver-tagged handshake.
- **User experience for "install a helper".** On Linux this is `apt
  install dsd-fme && systemctl enable ferrite-helper`. On Windows /
  macOS it's messier. Consider helper-bundled distribution later; for
  now, Linux-first matches Ferrite's v0.1 target platform anyway
  (`docs/00-context.md`).

## Estimated effort

- Track A: ~3 weeks (codec2 vendor + M17 framing + FreeDV).
- Track B: ~2 weeks protocol design + first helper (dsd-fme DMR) end-
  to-end.

Total Phase 6: ~5 weeks.
