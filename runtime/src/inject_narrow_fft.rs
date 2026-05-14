//! Runtime injection of a narrow-spectrum FFT tap downstream of every
//! Channelizer block.
//!
//! Why: presets carry a single wide FFT chain on the source side
//! (`src → tee → fft → logmag → ui:fft`) that gives the operator a
//! ~146 Hz/bin view of ±5 MHz around the source centre — great for
//! seeing where stations sit in a band, useless for inspecting
//! modulation detail inside a single channel. The channelizer's
//! output is much narrower (typically 240 kHz for analog voice, 12
//! kHz for SSB/CW/FT8), so a parallel FFT on its output yields tens
//! to hundreds of times finer per-bin resolution — enough to see
//! FM sideband structure, AM modulation peaks, SSB voice envelope,
//! and the discrete tones of a FT8 transmission.
//!
//! Rather than authoring this chain into every preset (~15 JSON
//! files at last count), we synthesise it at compose time. Same
//! pattern as `env_split.rs` which injects WS-bridge pairs at the
//! node↔browser boundary, and `compose.rs` which materialises the
//! `src` block from the operator's `SourceConfig`. The shape:
//!
//! ```text
//!   chan.out ─┬─► (existing consumers, e.g. chan_tee.in)
//!             └─► __narrow_tee.in
//!   __narrow_tee.out0 ─► (re-pointed existing consumer)
//!   __narrow_tee.out1 ─► __fft_narrow.in
//!   __fft_narrow.out ─► __logmag_narrow.in
//!   __logmag_narrow.out ─► ui:fft_narrow
//! ```
//!
//! When the channelizer's existing consumer is a `TeeIqF32` (as it is
//! in every shipped preset — `chan_tee`), the injected `__narrow_tee`
//! sits BEFORE that tee. The original `chan_tee` keeps fanning out to
//! its two consumers (`rssi`, `demod_rds`, etc.); we just route the
//! channelizer's output through an extra fork on the way in.
//!
//! If the channelizer feeds a single consumer directly, we still
//! insert the tee — `__narrow_tee.out0` takes over the original wire,
//! and `__narrow_tee.out1` feeds the new FFT chain.
//!
//! Pass timing: AFTER `compose_source` (so source-side block IDs are
//! stable and the doc has its final block set), BEFORE
//! `split_for_environment` (so `env_split` sees the new `ui:fft_narrow`
//! sink and auto-inserts a `WsBridgeTxFftU8` for it the same way it
//! does for `ui:fft` today). No preset JSON edits needed.

use std::collections::BTreeSet;

use serde_json::json;

use crate::doc::{BlockInstanceDecl, Environment, FlowgraphDoc, Wire};

/// Naming prefix for synthesised narrow-FFT blocks. The `__` is the
/// convention `env_split.rs` already uses for its bridge inserts; the
/// rest of the registry's blocks don't start with `__`.
const PREFIX: &str = "__narrow";

// FFT block emits one output frame every `size` input samples, so the
// natural per-pane refresh rate is `output_rate_hz / size`. Pinning a
// single size across every preset would make narrow FFT8 channels
// crawl (12 kHz / 4096 ≈ 3 fps) while WBFM ran at 30 fps. We instead
// pick a power-of-two size that keeps the natural fps within the
// target window — fast enough to feel live, slow enough that the
// per-bin resolution stays usable for whatever the channel carries.
const NARROW_TARGET_FPS: f64 = 15.0;
const NARROW_FFT_SIZE_MIN: u32 = 256;
const NARROW_FFT_SIZE_MAX: u32 = 16_384;
const NARROW_MAX_FPS: u32 = 30;
const NARROW_LOGMAG_ALPHA: f64 = 0.3;
const NARROW_LOGMAG_FLOOR_DBFS: f64 = -100.0;
const NARROW_LOGMAG_CEIL_DBFS: f64 = 0.0;

/// Largest power-of-two FFT size keeping `output_rate_hz / size`
/// ≥ [`NARROW_TARGET_FPS`], clamped to [`NARROW_FFT_SIZE_MIN`] /
/// [`NARROW_FFT_SIZE_MAX`]. Examples:
///   - 240 kHz (WBFM)   → 16384 (~14.6 fps, ~14.6 Hz/bin)
///   -  48 kHz (NBFM)   →  2048 (~23.4 fps, ~23.4 Hz/bin)
///   -  12 kHz (FT8)    →   512 (~23.4 fps, ~23.4 Hz/bin)
///   -   3 kHz (SSB)    →   256 (~11.7 fps, ~11.7 Hz/bin)
fn narrow_fft_size_for_rate(rate_hz: f64) -> u32 {
    if !rate_hz.is_finite() || rate_hz <= 0.0 {
        return NARROW_FFT_SIZE_MIN;
    }
    let raw = (rate_hz / NARROW_TARGET_FPS).clamp(
        f64::from(NARROW_FFT_SIZE_MIN),
        f64::from(NARROW_FFT_SIZE_MAX),
    );
    // Floor to the nearest power of two so the FFT planner can pick
    // the radix-2 fast path. Doubling check avoids u32 overflow at
    // the ceiling.
    let mut pow2 = NARROW_FFT_SIZE_MIN;
    while pow2 < NARROW_FFT_SIZE_MAX && f64::from(pow2 * 2) <= raw {
        pow2 *= 2;
    }
    pow2
}

/// Walk every block in `doc`, and for each `Channelizer` insert a
/// `__narrow_tee_<id>` + `__fft_narrow_<id>` + `__logmag_narrow_<id>`
/// chain that taps the channelizer's output to a new `ui:fft_narrow_<id>`
/// sink. Single-channelizer presets get a plain `ui:fft_narrow` for
/// readability; multi-channelizer presets disambiguate per-channelizer.
///
/// Idempotent: if the doc already has any block whose id starts with
/// `__narrow`, the pass is a no-op (assume a previous run injected,
/// nothing to do). This lets `apply_pending_doc` paths re-run compose
/// + inject without doubling up.
pub fn inject_narrow_fft_taps(doc: &mut FlowgraphDoc) {
    if doc.blocks.keys().any(|k| k.starts_with(PREFIX)) {
        // Already injected (e.g. by a prior pass in a reconfigure
        // cycle). Leave it alone — a second tee would double-fork the
        // signal and confuse `env_split`.
        return;
    }

    let channelizer_ids: Vec<String> = doc
        .blocks
        .iter()
        .filter(|(_, b)| b.type_name == "Channelizer")
        .map(|(id, _)| id.clone())
        .collect();
    if channelizer_ids.is_empty() {
        return;
    }

    let only_one = channelizer_ids.len() == 1;
    for chan_id in channelizer_ids {
        inject_for_channelizer(doc, &chan_id, only_one);
    }
}

fn inject_for_channelizer(doc: &mut FlowgraphDoc, chan_id: &str, only_one: bool) {
    let tee_id = format!("{PREFIX}_tee_{chan_id}");
    let fft_id = format!("{PREFIX}_fft_{chan_id}");
    let logmag_id = format!("{PREFIX}_logmag_{chan_id}");

    // Pick the UI sink name. Single-channelizer presets (every shipped
    // analog preset today) get `ui:fft_narrow`; multi-channelizer
    // presets disambiguate with the channelizer's id so the UI can
    // tell them apart.
    let ui_sink = if only_one {
        "ui:fft_narrow".to_string()
    } else {
        format!("ui:fft_narrow_{chan_id}")
    };

    // Guard against id collisions with operator-authored blocks.
    let conflicting: BTreeSet<&str> = [tee_id.as_str(), fft_id.as_str(), logmag_id.as_str()]
        .into_iter()
        .filter(|id| doc.blocks.contains_key(*id))
        .collect();
    if !conflicting.is_empty() {
        // Refuse to clobber. Better to leave the narrow chain absent
        // than to mangle an operator's hand-authored override.
        // (runtime has no `tracing` dep — fall back to stderr.)
        eprintln!(
            "inject_narrow_fft: id collision for channelizer {chan_id:?}; \
             skipping (would clobber: {conflicting:?})",
        );
        return;
    }

    // Read the channelizer's `output_rate_hz` param so the FFT +
    // LogMagU8 stamps the right rate on their PortMeta. Default to
    // 240 kHz (the wbfm convention) if absent — most presets specify
    // it, but `Channelizer::Params` defaults to that too so this is
    // the right fallback.
    let output_rate_hz = doc
        .blocks
        .get(chan_id)
        .and_then(|b| b.params.as_ref())
        .and_then(|p| p.get("output_rate_hz"))
        .and_then(|v| v.as_f64())
        .unwrap_or(240_000.0);

    // Re-point every wire whose src is `<chan_id>.out` to flow through
    // the new tee instead. The tee's `out0` takes over the original
    // downstream wire; `out1` feeds the new FFT chain.
    let chan_out = format!("{chan_id}.out");
    let tee_out0 = format!("{tee_id}.out0");
    for wire in doc.wires.iter_mut() {
        if wire.src == chan_out {
            wire.src = tee_out0.clone();
        }
    }
    // Add the chan→tee wire (channelizer feeds the new tee).
    doc.wires
        .push(Wire::new(chan_out.clone(), format!("{tee_id}.in")));
    // Tee's second branch goes to the narrow FFT.
    doc.wires
        .push(Wire::new(format!("{tee_id}.out1"), format!("{fft_id}.in")));
    // Standard FFT → LogMagU8 → ui:fft_narrow chain.
    doc.wires.push(Wire::new(
        format!("{fft_id}.out"),
        format!("{logmag_id}.in"),
    ));
    doc.wires
        .push(Wire::new(format!("{logmag_id}.out"), ui_sink));

    // Materialise the three new blocks. All run on the node side
    // (where the channelizer lives); env_split will insert a
    // WsBridgeTxFftU8 between the logmag and `ui:fft_narrow` because
    // that sink is browser-terminated, same as `ui:fft`.
    doc.blocks.insert(
        tee_id.clone(),
        BlockInstanceDecl {
            type_name: "TeeIqF32".into(),
            params: None,
            force_params: None,
            placement: Some(Environment::Node),
        },
    );
    let fft_size = narrow_fft_size_for_rate(output_rate_hz);
    doc.blocks.insert(
        fft_id.clone(),
        BlockInstanceDecl {
            type_name: "FFT".into(),
            params: Some(json!({
                "size": fft_size,
                "window": "hann",
                "max_frames_per_second": NARROW_MAX_FPS,
                // FFT also accepts a sample_rate_hz param to stamp
                // onto its output PortMeta; the channelizer's output
                // rate flows through automatically via runtime rate
                // negotiation, but setting it explicitly here avoids
                // a "rate unknown" warning if the channelizer hasn't
                // initialised yet at the moment env_split runs.
                "sample_rate_hz": output_rate_hz,
            })),
            force_params: None,
            placement: Some(Environment::Node),
        },
    );
    doc.blocks.insert(
        logmag_id.clone(),
        BlockInstanceDecl {
            type_name: "LogMagU8".into(),
            params: Some(json!({
                "size": fft_size,
                "alpha": NARROW_LOGMAG_ALPHA,
                "floor_dbfs": NARROW_LOGMAG_FLOOR_DBFS,
                "ceil_dbfs": NARROW_LOGMAG_CEIL_DBFS,
            })),
            force_params: None,
            placement: Some(Environment::Node),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::FlowgraphDoc;

    const WBFM: &[u8] = include_bytes!("../../flowgraphs/wbfm.json");

    fn parse(json: &[u8]) -> FlowgraphDoc {
        FlowgraphDoc::from_json(json).expect("doc parses")
    }

    #[test]
    fn no_channelizer_means_no_injection() {
        let mut doc = parse(
            br#"{
                "name": "noop",
                "environments": ["node"],
                "blocks": {
                    "src":  {"type": "SineSource"},
                    "sink": {"type": "FileIqSink"}
                },
                "wires": [["src.out", "sink.in"]]
            }"#,
        );
        let before_blocks = doc.blocks.len();
        let before_wires = doc.wires.len();
        inject_narrow_fft_taps(&mut doc);
        assert_eq!(doc.blocks.len(), before_blocks);
        assert_eq!(doc.wires.len(), before_wires);
    }

    #[test]
    fn injects_narrow_chain_for_wbfm() {
        let mut doc = parse(WBFM);
        inject_narrow_fft_taps(&mut doc);

        // Three new blocks materialised.
        assert!(doc.blocks.contains_key("__narrow_tee_chan"));
        assert!(doc.blocks.contains_key("__narrow_fft_chan"));
        assert!(doc.blocks.contains_key("__narrow_logmag_chan"));

        // New `ui:fft_narrow` sink wired in.
        let has_narrow_sink = doc
            .wires
            .iter()
            .any(|w| w.src == "__narrow_logmag_chan.out" && w.dst == "ui:fft_narrow");
        assert!(has_narrow_sink, "narrow ui sink not wired");

        // Original `chan.out → rssi.in` wire was re-pointed to
        // `__narrow_tee_chan.out0 → rssi.in` (wbfm's first consumer of
        // chan output is `rssi` after the Phase B2 collapse).
        let has_repoint = doc
            .wires
            .iter()
            .any(|w| w.src == "__narrow_tee_chan.out0" && w.dst == "rssi.in");
        assert!(has_repoint, "rssi.in not re-pointed through narrow tee");

        // Channelizer now feeds the new tee instead of rssi.
        let has_chan_to_narrow_tee = doc
            .wires
            .iter()
            .any(|w| w.src == "chan.out" && w.dst == "__narrow_tee_chan.in");
        assert!(
            has_chan_to_narrow_tee,
            "chan.out → __narrow_tee_chan.in missing"
        );

        // No leftover direct wire from chan.out to rssi.in.
        let still_direct = doc
            .wires
            .iter()
            .any(|w| w.src == "chan.out" && w.dst == "rssi.in");
        assert!(
            !still_direct,
            "chan.out → chan_tee.in should have been re-pointed"
        );
    }

    #[test]
    fn idempotent_on_double_inject() {
        let mut doc = parse(WBFM);
        inject_narrow_fft_taps(&mut doc);
        let blocks_after_first = doc.blocks.len();
        let wires_after_first = doc.wires.len();
        inject_narrow_fft_taps(&mut doc);
        assert_eq!(doc.blocks.len(), blocks_after_first);
        assert_eq!(doc.wires.len(), wires_after_first);
    }

    #[test]
    fn fft_size_scales_with_channel_rate() {
        // The picker takes the largest power-of-two strictly less than
        // `rate / TARGET_FPS = rate / 15` so the natural fps stays at
        // or above the target.
        // 240 kHz → 240000/15 = 16000; largest power-of-two ≤ 16000 is
        // 8192 → ~29 fps, ~29 Hz/bin.
        assert_eq!(narrow_fft_size_for_rate(240_000.0), 8192);
        // 48 kHz → 48000/15 = 3200; largest pow2 ≤ 3200 is 2048
        // → ~23 fps, ~23 Hz/bin.
        assert_eq!(narrow_fft_size_for_rate(48_000.0), 2048);
        // 12 kHz FT8 / CW channel — the original 4096-bin pin
        // produced ~3 fps; 512 brings it to ~23 fps with ~23 Hz/bin.
        assert_eq!(narrow_fft_size_for_rate(12_000.0), 512);
        // Below the floor: clamp to NARROW_FFT_SIZE_MIN.
        assert_eq!(narrow_fft_size_for_rate(1_000.0), 256);
        assert_eq!(narrow_fft_size_for_rate(0.0), 256);
        assert_eq!(narrow_fft_size_for_rate(-1.0), 256);
        // Above the ceiling: clamp to NARROW_FFT_SIZE_MAX.
        assert_eq!(narrow_fft_size_for_rate(10_000_000.0), 16384);
    }

    #[test]
    fn id_collision_is_a_no_op_for_that_channelizer() {
        // Operator hand-authored a block with the synthesised id.
        // Pass refuses to clobber it.
        let mut doc = parse(WBFM);
        doc.blocks.insert(
            "__narrow_fft_chan".into(),
            BlockInstanceDecl {
                type_name: "FFT".into(),
                params: None,
                force_params: None,
                placement: None,
            },
        );
        // Since `__narrow_fft_chan` already exists, the early bail
        // at the top of `inject_narrow_fft_taps` triggers (any block
        // starting with `__narrow`); the pass is a no-op.
        let blocks_before = doc.blocks.len();
        let wires_before = doc.wires.len();
        inject_narrow_fft_taps(&mut doc);
        assert_eq!(doc.blocks.len(), blocks_before);
        assert_eq!(doc.wires.len(), wires_before);
    }
}
