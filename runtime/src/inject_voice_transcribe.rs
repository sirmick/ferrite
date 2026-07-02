//! Runtime injection of a `VoiceTranscribe` tap immediately upstream of
//! every `AudioSink`, so in-browser speech-to-text is available on
//! *any* preset with an audio chain — no per-preset JSON, no
//! hand-authored block. Mirrors [`inject_narrow_fft`](crate::inject_narrow_fft):
//! a compose-time graph splice that runs before `split_for_environment`.
//!
//! ```text
//!   …NR/resamp… ─► audio.in                 (before)
//!
//!   …NR/resamp… ─► __voice_transcribe_audio.in
//!   __voice_transcribe_audio.out ─► audio.in (after)
//! ```
//!
//! `VoiceTranscribe` is a pure RealF32 passthrough (`Placement::WasmOnly`,
//! same browser side as `AudioSink`), so the audio bytes are unchanged
//! and no cross-env wire is created. The tap is injected `mode: "on"`
//! (audio plays and is transcribed) — it only exists when transcription
//! is engaged.
//!
//! **Opt-in via the [`Profile`].** This pass is gated on
//! `profile.transcribe`: it's the same build-time mechanism as the
//! `audio` chain gate (the receiver's Audio control is Off | On |
//! Transcribe). Run *after* `apply_profile`, so when `audio: false`
//! the chain is already stripped and there's nothing to tap; and
//! `transcribe` implies `audio` (UI-enforced), so the chain is present
//! whenever this pass injects.
//!
//! Idempotent and conservative: no-op unless `profile.transcribe`;
//! skips if a `VoiceTranscribe` already exists (a preset opted in by
//! hand), if the synthetic id would collide, or if an `AudioSink` has
//! no producer wire.

use serde_json::json;

use crate::apply_profile::Profile;
use crate::doc::{BlockInstanceDecl, Environment, FlowgraphDoc, Wire};

/// Synthetic-block id prefix. `__` matches the convention `env_split`
/// and `inject_narrow_fft` already use; no registry block starts `__`.
const PREFIX: &str = "__voice_transcribe";

/// Splice **one** `VoiceTranscribe` immediately before a single
/// `AudioSink` in `doc` when `profile.transcribe` is set.
///
/// Transcription only needs one mono feed of post-NR audio, so we tap
/// exactly one sink — the first by id (`blocks` is a `BTreeMap`, so
/// this is deterministic: `audio` for mono, `audio_l` for the stereo
/// presets). Tapping *every* sink would put a mono passthrough in each
/// split channel and spawn a worker per sink — wrong for stereo.
///
/// No-op when transcription isn't engaged (`!profile.transcribe`), when
/// the preset has no `AudioSink` (no audio chain — e.g. a pure
/// decoder/record preset), or when it already carries a
/// `VoiceTranscribe` (hand-authored opt-in: leave the wiring alone).
pub fn inject_voice_transcribe(doc: &mut FlowgraphDoc, profile: &Profile) {
    if !profile.transcribe {
        return;
    }

    let already_present = doc
        .blocks
        .values()
        .any(|b| b.type_name == "VoiceTranscribe" || b.type_name == "SherpaTranscribe")
        || doc.blocks.keys().any(|k| k.starts_with(PREFIX));
    if already_present {
        return;
    }

    let sink_ids: Vec<String> = doc
        .blocks
        .iter()
        .filter(|(_, b)| b.type_name == "AudioSink")
        .map(|(id, _)| id.clone())
        .collect();
    if sink_ids.is_empty() {
        return;
    }

    // Walk sinks in (deterministic) id order; tap the first usable one
    // and stop — exactly one VoiceTranscribe per graph.
    for sink_id in sink_ids {
        let vt_id = format!("{PREFIX}_{sink_id}");
        if doc.blocks.contains_key(&vt_id) {
            // Refuse to clobber an operator-authored block; better to
            // leave transcription absent for this sink than mangle it.
            eprintln!(
                "inject_voice_transcribe: id collision for sink {sink_id:?}; skipping ({vt_id:?})",
            );
            continue;
        }

        // Re-point the AudioSink's producer wire(s) through the new
        // tap: `<producer> → audio.in` becomes
        // `<producer> → __vt.in` and we add `__vt.out → audio.in`.
        let sink_in = format!("{sink_id}.in");
        let mut repointed = false;
        for wire in doc.wires.iter_mut() {
            if wire.dst == sink_in {
                wire.dst = format!("{vt_id}.in");
                repointed = true;
            }
        }
        if !repointed {
            // AudioSink with no producer — nothing to tap. Don't
            // materialise a dangling block.
            continue;
        }
        doc.wires.push(Wire::new(format!("{vt_id}.out"), sink_in));

        // Transcript spots → the UI. `events` rides the same `ui:<name>`
        // path FT8/WSPR use: env_split terminates it in an `EventStore`
        // (kind `transcribe`) that folds into the server store — node
        // writes it directly, browser POSTs — so the panel sees the
        // transcript regardless of which side decodes.
        doc.wires.push(Wire::new(
            format!("{vt_id}.events"),
            "ui:transcribe".to_string(),
        ));

        // Placement follows the audio-spine cut: `Server` → node (the
        // headless path, whisper in `ferrited`); `Browser`/`Balanced` →
        // browser (the legacy in-browser STT). The tap is newly injected
        // with no authored placement, so `Balanced` defaults to browser.
        // Under `Server` the whole upstream audio chain is already node
        // (apply_profile ran first), so a node tap reads node-side audio
        // with no illegal crossing. We still tag `placement_role:
        // "transcribe"` for introspection / a later re-apply. `mode:
        // "on"` — the tap only exists when transcription is engaged.
        let placement = profile.audio_split.tap_placement();
        // Engine follows placement: server-side (node) profiles transcribe
        // with the sherpa-onnx sidecar (`SherpaTranscribe`); browser-placed
        // taps keep whisper (`VoiceTranscribe`, WASM). Same `events` shape
        // and `placement_role`, so everything downstream is unchanged.
        let engine = if placement == Environment::Node {
            "SherpaTranscribe"
        } else {
            "VoiceTranscribe"
        };
        doc.blocks.insert(
            vt_id,
            BlockInstanceDecl {
                type_name: engine.into(),
                params: Some(json!({ "mode": "on" })),
                placement: Some(placement),
                placement_role: Some("transcribe".to_string()),
                ..Default::default()
            },
        );

        // Node (sherpa) path: the tap is node-side and sits inline before
        // the AudioSink, which `apply_profile` (run just before this pass)
        // already places on the browser — `AudioSink` is `WasmOnly`. So the
        // `__vt.out → audio.in` wire crosses the env boundary; `env_split`
        // bridges that RealF32 audio node→browser and the browser plays it.
        // The browser learns the bridge's stream id from the *same* split
        // via `GET /api/flowgraph/browser-half`, so audio AND transcript
        // arrive together with no client-side re-derivation to diverge.
        // Headless (no browser) still works — the bridged audio just has no
        // consumer, and the node-side transcript is unaffected.
        break; // one tap is enough — leave any other sinks untouched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_profile::AudioSplit;
    use crate::doc::{Environment, FlowgraphDoc};

    fn parse(json: &[u8]) -> FlowgraphDoc {
        FlowgraphDoc::from_json(json).expect("doc parses")
    }

    /// Profile with transcription engaged (the receiver's "Transcribe"
    /// state — implies `audio`). `Balanced` split = browser tap, the
    /// default in-browser path.
    fn engaged() -> Profile {
        Profile {
            audio: true,
            transcribe: true,
            audio_split: AudioSplit::Balanced,
        }
    }

    #[test]
    fn disabled_when_transcribe_off() {
        // Same audio preset as `splices_before_audio_sink`, but the
        // profile hasn't engaged transcription → pass is a no-op.
        let mut doc = parse(
            br#"{
                "name": "audio",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":   {"type": "SineSource"},
                    "audio": {"type": "AudioSink", "placement": "browser"}
                },
                "wires": [["src.out", "audio.in"]]
            }"#,
        );
        let (b, w) = (doc.blocks.len(), doc.wires.len());
        inject_voice_transcribe(&mut doc, &Profile::default()); // transcribe: false
        assert_eq!(doc.blocks.len(), b);
        assert_eq!(doc.wires.len(), w);
        assert!(!doc.blocks.keys().any(|k| k.starts_with(PREFIX)));
    }

    #[test]
    fn no_audio_sink_means_no_injection() {
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
        let (b, w) = (doc.blocks.len(), doc.wires.len());
        inject_voice_transcribe(&mut doc, &engaged());
        assert_eq!(doc.blocks.len(), b);
        assert_eq!(doc.wires.len(), w);
    }

    #[test]
    fn splices_before_audio_sink() {
        let mut doc = parse(
            br#"{
                "name": "audio",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":   {"type": "SineSource"},
                    "demod": {"type": "FmDemod"},
                    "nr":    {"type": "AudioNrMono", "placement": "browser"},
                    "audio": {"type": "AudioSink", "placement": "browser"}
                },
                "wires": [
                    ["src.out", "demod.in"],
                    ["demod.out", "nr.in"],
                    ["nr.out", "audio.in"]
                ]
            }"#,
        );
        inject_voice_transcribe(&mut doc, &engaged());

        let vt = "__voice_transcribe_audio";
        assert_eq!(
            doc.blocks.get(vt).map(|b| b.type_name.as_str()),
            Some("VoiceTranscribe"),
        );
        // nr now feeds the tap, the tap feeds the sink.
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "nr.out" && w.dst == format!("{vt}.in")));
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == format!("{vt}.out") && w.dst == "audio.in"));
        // No remaining direct nr→audio wire.
        assert!(!doc
            .wires
            .iter()
            .any(|w| w.src == "nr.out" && w.dst == "audio.in"));

        // The transcript reaches the UI over the events port.
        assert!(
            doc.wires
                .iter()
                .any(|w| w.src == format!("{vt}.events") && w.dst == "ui:transcribe"),
            "events → ui:transcribe wire added"
        );
        // Default placement is Browser (legacy in-browser STT) and the
        // block is tagged with its placement role.
        let b = doc.blocks.get(vt).unwrap();
        assert_eq!(b.placement, Some(Environment::Browser));
        assert_eq!(b.placement_role.as_deref(), Some("transcribe"));
    }

    #[test]
    fn transcribe_placement_node_runs_the_tap_server_side() {
        // The headless path: `AudioSplit::Server` puts the injected
        // VoiceTranscribe on the node side so whisper runs in `ferrited`
        // with no browser. The events wire is unchanged — env_split turns
        // it into a node→browser bridge.
        let mut doc = parse(
            br#"{
                "name": "audio",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":   {"type": "SineSource"},
                    "demod": {"type": "FmDemod"},
                    "audio": {"type": "AudioSink", "placement": "browser"}
                },
                "wires": [
                    ["src.out", "demod.in"],
                    ["demod.out", "audio.in"]
                ]
            }"#,
        );
        let profile = Profile {
            audio: true,
            transcribe: true,
            audio_split: AudioSplit::Server,
        };
        inject_voice_transcribe(&mut doc, &profile);

        let vt = "__voice_transcribe_audio";
        let b = doc.blocks.get(vt).expect("tap injected");
        assert_eq!(
            b.placement,
            Some(Environment::Node),
            "tap runs server-side for headless transcription"
        );
        assert_eq!(
            b.type_name, "SherpaTranscribe",
            "server-side profiles transcribe via the sherpa-onnx sidecar"
        );
        assert_eq!(b.placement_role.as_deref(), Some("transcribe"));
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == format!("{vt}.events") && w.dst == "ui:transcribe"));
    }

    #[test]
    fn node_engine_leaves_audiosink_placement_to_apply_profile() {
        // Server split: SherpaTranscribe runs node-side, inline before the
        // AudioSink. This pass does NOT touch the sink's placement — that's
        // `apply_profile`'s job (it runs first and places the `WasmOnly`
        // AudioSink on the browser). Here we feed a sink already pinned
        // browser, as the real pipeline would, and assert this pass leaves
        // it alone. The resulting `__vt.out → audio.in` wire is the
        // node→browser audio crossing `env_split` bridges; the browser
        // gets the matching Rx via `/api/flowgraph/browser-half`.
        let mut doc = parse(
            br#"{
                "name": "audio",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":   {"type": "SineSource"},
                    "demod": {"type": "FmDemod"},
                    "audio": {"type": "AudioSink", "placement": "browser"}
                },
                "wires": [["src.out", "demod.in"], ["demod.out", "audio.in"]]
            }"#,
        );
        let profile = Profile {
            audio: true,
            transcribe: true,
            audio_split: AudioSplit::Server,
        };
        inject_voice_transcribe(&mut doc, &profile);
        assert_eq!(
            doc.blocks
                .get("__voice_transcribe_audio")
                .unwrap()
                .type_name,
            "SherpaTranscribe"
        );
        assert_eq!(
            doc.blocks.get("audio").unwrap().placement,
            Some(Environment::Browser),
            "sink placement is apply_profile's call; this pass leaves it untouched"
        );
        // The tap sits inline: demod → __vt → audio.
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "__voice_transcribe_audio.out" && w.dst == "audio.in"));
    }

    #[test]
    fn taps_only_one_sink_on_stereo() {
        // Two AudioSinks (the stereo presets: audio_l / audio_r). Only
        // ONE VoiceTranscribe, on the first sink by id (audio_l); the
        // right channel is left wired straight through.
        let mut doc = parse(
            br#"{
                "name": "stereo",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":     {"type": "SineSource"},
                    "nr":      {"type": "AudioNrMono", "placement": "browser"},
                    "audio_l": {"type": "AudioSink", "placement": "browser"},
                    "audio_r": {"type": "AudioSink", "placement": "browser"}
                },
                "wires": [
                    ["src.out", "nr.in"],
                    ["nr.left", "audio_l.in"],
                    ["nr.right", "audio_r.in"]
                ]
            }"#,
        );
        inject_voice_transcribe(&mut doc, &engaged());

        let vt_count = doc
            .blocks
            .values()
            .filter(|b| b.type_name == "VoiceTranscribe")
            .count();
        assert_eq!(vt_count, 1, "exactly one tap for a multi-sink graph");
        assert!(doc.blocks.contains_key("__voice_transcribe_audio_l"));
        assert!(!doc.blocks.contains_key("__voice_transcribe_audio_r"));
        // Right channel untouched: nr.right still feeds audio_r directly.
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "nr.right" && w.dst == "audio_r.in"));
        // Left channel goes through the tap.
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "nr.left" && w.dst == "__voice_transcribe_audio_l.in"));
        assert!(doc
            .wires
            .iter()
            .any(|w| w.src == "__voice_transcribe_audio_l.out" && w.dst == "audio_l.in"));
    }

    #[test]
    fn idempotent_and_respects_hand_authored() {
        let mut doc = parse(
            br#"{
                "name": "audio",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":   {"type": "SineSource"},
                    "audio": {"type": "AudioSink", "placement": "browser"}
                },
                "wires": [["src.out", "audio.in"]]
            }"#,
        );
        inject_voice_transcribe(&mut doc, &engaged());
        let after_first = (doc.blocks.len(), doc.wires.len());
        // Second pass: the `__voice_transcribe_*` already present → no-op.
        inject_voice_transcribe(&mut doc, &engaged());
        assert_eq!((doc.blocks.len(), doc.wires.len()), after_first);

        // Hand-authored VoiceTranscribe → pass leaves it entirely alone.
        let mut hand = parse(
            br#"{
                "name": "hand",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":   {"type": "SineSource"},
                    "vt":    {"type": "VoiceTranscribe", "placement": "browser"},
                    "audio": {"type": "AudioSink", "placement": "browser"}
                },
                "wires": [["src.out", "vt.in"], ["vt.out", "audio.in"]]
            }"#,
        );
        let before = (hand.blocks.len(), hand.wires.len());
        inject_voice_transcribe(&mut hand, &engaged());
        assert_eq!((hand.blocks.len(), hand.wires.len()), before);
    }
}
