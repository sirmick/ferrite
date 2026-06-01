//! Pre-split rewrite pass — applies a runtime [`Profile`] to a flowgraph
//! doc before [`split_for_environment`](crate::env_split) runs.
//!
//! Two effects, in order:
//!
//! 1. **Gate.** Each block carrying a `"when": { … }` map is checked
//!    against the active profile. Any well-known key whose value
//!    doesn't match the profile drops the block and every wire that
//!    touches it. Unknown keys are treated as non-gating so a future
//!    profile field doesn't strip blocks tagged for it on an older
//!    runtime.
//!
//! 2. **Audio-spine cut.** Blocks on the audio path carry a
//!    `"placement_role"` (`demod` / `audio` / `nr` / `transcribe`). The
//!    profile's [`AudioSplit`] slides a single node↔browser cut along
//!    that chain: `Server` pulls the whole spine node-side (headless
//!    transcription), `Browser` pushes it browser-side, `Balanced`
//!    leaves the authored placement. One ordered knob instead of three
//!    independent per-block overrides — which keeps the boundary a
//!    single monotonic node→browser crossing `env_split` can realise.
//!
//! The pass runs after [`compose_source`](crate::compose) and
//! [`inject_narrow_fft_taps`](crate::inject_narrow_fft) and before
//! [`split_for_environment`](crate::env_split). Pruning blocks before
//! the split means orphan wires never even reach the bridge-insertion
//! logic.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::doc::{BlockInstanceDecl, Environment, FlowgraphDoc};
use crate::validate::split_endpoint;

/// Where the audio-processing spine is cut between the node daemon and
/// the browser. The post-source audio path is a linear chain —
/// `demod → audio-resample → noise-reduce → AudioSink` — and `env_split`
/// only supports a single **node→browser** crossing along it. So rather
/// than three independent per-block placement knobs (which can encode an
/// illegal browser→node crossing — e.g. "NR browser, transcribe node"),
/// one ordered setting slides that single cut down the chain. Every
/// spine block before the cut runs node; everything at or after it runs
/// browser. Because the chain is topologically ordered, every crossing
/// is node→browser by construction — illegal states are unrepresentable.
///
/// `AudioSink` is `Placement::WasmOnly`, so the cut can never pass it:
/// the final WebAudio sink always runs browser-side. "Server" therefore
/// means "everything up to (and feeding) the sink runs node".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioSplit {
    /// Thin server: push the whole spine (demod onward) browser-side.
    /// IQ is streamed across and demodulated in the browser. Heaviest on
    /// the client, lightest on the daemon.
    Browser,
    /// Leave each spine block at its preset-authored placement (the
    /// default). Demod typically node, audio-render + NR browser — the
    /// historical behaviour, preserved exactly so this is a no-op.
    Balanced,
    /// Headless: pull the whole spine node-side. Demod, resample, and NR
    /// run in `ferrited`; a node-placed `VoiceTranscribe` tap therefore
    /// has node-side audio to read, so whisper runs with no browser. The
    /// (forced-browser) `AudioSink` still receives finished audio over
    /// the one remaining node→browser crossing.
    Server,
}

impl AudioSplit {
    /// The placement a spine block should adopt under this split, or
    /// `None` to leave its authored placement (Balanced). Applied
    /// uniformly to every spine role, which is what keeps the cut a
    /// single monotonic boundary.
    fn override_env(self) -> Option<Environment> {
        match self {
            Self::Browser => Some(Environment::Browser),
            Self::Server => Some(Environment::Node),
            Self::Balanced => None,
        }
    }

    /// Placement for a *newly injected* spine block (the `VoiceTranscribe`
    /// tap) that has no authored placement to fall back on. `Server` →
    /// node (headless); everything else → browser (the default audio
    /// side). Distinct from [`override_env`] only in that `Balanced`
    /// resolves to a concrete side rather than "leave as authored",
    /// because there's nothing authored to leave.
    #[must_use]
    pub fn tap_placement(self) -> Environment {
        match self {
            Self::Server => Environment::Node,
            Self::Browser | Self::Balanced => Environment::Browser,
        }
    }
}

/// User-facing runtime knobs that affect doc shape independently of
/// preset authoring. `Default` keeps the audio chain enabled and leaves
/// placement at the preset author's choice (`AudioSplit::Balanced`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// When false, blocks tagged `"when": { "audio": true }` (and their
    /// wires) are removed before split. The most useful application is
    /// stripping the audio chain on digital modes a user only wants to
    /// decode — frees WS bandwidth and stops the browser AudioWorklet
    /// from being fed silence.
    pub audio: bool,
    /// When true, a `VoiceTranscribe` tap is spliced before every
    /// `AudioSink` (speech-to-text). Default false — transcription is
    /// opt-in via the receiver's Audio control, the same build-time
    /// mechanism as `audio`. Implies `audio` (the tap sits on the audio
    /// chain); the UI never sets it without `audio`. The tap follows
    /// `audio_split`: node-side (headless) under `Server`, else browser.
    pub transcribe: bool,
    /// Where the audio spine is cut between daemon and browser — one
    /// ordered setting over what used to be three independent per-block
    /// placement overrides. See [`AudioSplit`].
    pub audio_split: AudioSplit,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            audio: true,
            transcribe: false,
            audio_split: AudioSplit::Balanced,
        }
    }
}

impl Profile {
    /// Look up the well-known profile field that corresponds to a
    /// `when`-clause key. Returns `None` for keys the profile doesn't
    /// recognize, which is the forward-compat hook: such keys don't
    /// gate (block stays).
    fn value(&self, key: &str) -> Option<Value> {
        match key {
            "audio" => Some(Value::Bool(self.audio)),
            "transcribe" => Some(Value::Bool(self.transcribe)),
            _ => None,
        }
    }
}

/// Apply `profile` to `doc` in place. Idempotent within a single
/// profile — calling twice with the same input yields the same output.
pub fn apply_profile(doc: &mut FlowgraphDoc, profile: &Profile) {
    let dropped: BTreeSet<String> = doc
        .blocks
        .iter()
        .filter(|(_, b)| !block_passes(b, profile))
        .map(|(id, _)| id.clone())
        .collect();

    if !dropped.is_empty() {
        doc.blocks.retain(|id, _| !dropped.contains(id));
        // `ui:<name>` wires don't reference a block on the destination
        // side, so they only need the source check; everything else
        // gets dropped if either endpoint refers to a removed block.
        doc.wires.retain(|w| {
            let (s, _) = split_endpoint(&w.src);
            if dropped.contains(s) {
                return false;
            }
            if w.ui_sink_name().is_some() {
                return true;
            }
            let (d, _) = split_endpoint(&w.dst);
            !dropped.contains(d)
        });
    }

    // Audio-spine cut: every block on the audio path carries a
    // `placement_role` (`demod` / `audio` / `nr` / `transcribe`), and
    // `audio_split` moves them all to the same side — node before the
    // cut, browser after. Applying one override uniformly to every spine
    // role is what keeps the boundary a single monotonic node→browser
    // crossing: there's no way to express a browser-upstream/node-
    // downstream pair, so `env_split` can't be handed an illegal
    // crossing. `Balanced` is `None` → authored placement preserved.
    if let Some(env) = profile.audio_split.override_env() {
        for b in doc.blocks.values_mut() {
            if is_spine_role(b.placement_role.as_deref()) {
                b.placement = Some(env);
            }
        }
    }
}

/// Whether a `placement_role` tag names a block on the audio spine —
/// the linear chain the `audio_split` cut slides along. New audio-path
/// blocks join the cut just by carrying one of these roles.
fn is_spine_role(role: Option<&str>) -> bool {
    matches!(role, Some("demod" | "audio" | "nr" | "transcribe"))
}

fn block_passes(b: &BlockInstanceDecl, p: &Profile) -> bool {
    let Some(when) = &b.when else {
        return true;
    };
    for (key, expected) in when {
        if let Some(actual) = p.value(key) {
            if &actual != expected {
                return false;
            }
        }
        // Unknown profile key → forward-compat: don't gate on it.
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc_from(json: &str) -> FlowgraphDoc {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn audio_false_drops_blocks_with_when_audio_true() {
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "src":   {"type": "HwSrc", "placement": "node"},
                    "audio": {"type": "AudioSink", "placement": "node",
                              "when": {"audio": true}}
                },
                "wires": [["src.out", "audio.in"]]
            }"#,
        );
        apply_profile(
            &mut doc,
            &Profile {
                audio: false,
                transcribe: false,
                audio_split: AudioSplit::Balanced,
            },
        );
        assert!(doc.blocks.contains_key("src"));
        assert!(!doc.blocks.contains_key("audio"));
        // The wire into the dropped block must go too.
        assert!(doc.wires.is_empty());
    }

    #[test]
    fn audio_true_keeps_when_audio_true_blocks() {
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "src":   {"type": "HwSrc", "placement": "node"},
                    "audio": {"type": "AudioSink", "placement": "node",
                              "when": {"audio": true}}
                },
                "wires": [["src.out", "audio.in"]]
            }"#,
        );
        apply_profile(&mut doc, &Profile::default());
        assert!(doc.blocks.contains_key("audio"));
        assert_eq!(doc.wires.len(), 1);
    }

    #[test]
    fn unknown_when_key_does_not_gate() {
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "future": {"type": "X", "placement": "node",
                               "when": {"freshness_v9": "high"}}
                },
                "wires": []
            }"#,
        );
        apply_profile(
            &mut doc,
            &Profile {
                audio: false,
                transcribe: false,
                audio_split: AudioSplit::Balanced,
            },
        );
        // The "freshness_v9" key isn't on Profile, so the block is kept.
        assert!(doc.blocks.contains_key("future"));
    }

    /// A representative audio spine: demod (node) → audio-resample
    /// (browser) → NR (browser) → transcribe tap (browser) → AudioSink
    /// (browser, the fixed `WasmOnly` floor — untagged, never moves),
    /// plus an unrelated untagged block. Used by the `audio_split` cut
    /// tests. The movable spine = the four role-tagged blocks.
    fn spine_doc() -> FlowgraphDoc {
        doc_from(
            r#"{
                "name": "t",
                "environments": ["node", "browser"],
                "blocks": {
                    "demod":  {"type": "FmDemod", "placement": "node",
                               "placement_role": "demod"},
                    "resamp": {"type": "RealF32Resamp", "placement": "browser",
                               "placement_role": "audio"},
                    "nr":     {"type": "AudioNrMono", "placement": "browser",
                               "placement_role": "nr"},
                    "vt":     {"type": "VoiceTranscribe", "placement": "browser",
                               "placement_role": "transcribe"},
                    "audio":  {"type": "AudioSink", "placement": "browser"},
                    "other":  {"type": "X", "placement": "node"}
                },
                "wires": []
            }"#,
        )
    }

    /// The movable spine, in flow order. The AudioSink is the fixed
    /// browser floor and is deliberately not in this list.
    const SPINE: [&str; 4] = ["demod", "resamp", "nr", "vt"];

    fn split_profile(split: AudioSplit) -> Profile {
        Profile {
            audio: true,
            transcribe: false,
            audio_split: split,
        }
    }

    #[test]
    fn server_split_pulls_whole_spine_node_side() {
        // The headless cut: every movable spine block → node, so a
        // node-placed transcribe tap has node-side audio to read. The
        // WasmOnly sink stays browser; untagged blocks aren't touched.
        let mut doc = spine_doc();
        apply_profile(&mut doc, &split_profile(AudioSplit::Server));
        for id in SPINE {
            assert_eq!(
                doc.blocks[id].placement,
                Some(Environment::Node),
                "{id} pulled node-side"
            );
        }
        assert_eq!(
            doc.blocks["audio"].placement,
            Some(Environment::Browser),
            "AudioSink stays browser (WasmOnly floor)"
        );
    }

    #[test]
    fn browser_split_pushes_whole_spine_browser_side() {
        let mut doc = spine_doc();
        apply_profile(&mut doc, &split_profile(AudioSplit::Browser));
        for id in SPINE {
            assert_eq!(
                doc.blocks[id].placement,
                Some(Environment::Browser),
                "{id} pushed browser-side"
            );
        }
        // Untagged block keeps its authored side — the cut only moves
        // the spine.
        assert_eq!(doc.blocks["other"].placement, Some(Environment::Node));
    }

    #[test]
    fn balanced_split_leaves_authored_placement() {
        // The default: no spine block moves — every placement is exactly
        // what the preset authored. This is the property that makes the
        // collapse a no-op for existing presets.
        let mut doc = spine_doc();
        apply_profile(&mut doc, &split_profile(AudioSplit::Balanced));
        assert_eq!(doc.blocks["demod"].placement, Some(Environment::Node));
        assert_eq!(doc.blocks["resamp"].placement, Some(Environment::Browser));
        assert_eq!(doc.blocks["nr"].placement, Some(Environment::Browser));
        assert_eq!(doc.blocks["vt"].placement, Some(Environment::Browser));
        assert_eq!(doc.blocks["audio"].placement, Some(Environment::Browser));
    }

    #[test]
    fn split_never_creates_a_browser_to_node_crossing() {
        // The core safety property: whatever the split, the chain stays
        // monotonic node→…→browser, so no upstream block is browser
        // while a downstream one is node — `env_split` is never handed
        // an illegal crossing. Check the full flow order incl. the sink.
        for split in [
            AudioSplit::Browser,
            AudioSplit::Balanced,
            AudioSplit::Server,
        ] {
            let mut doc = spine_doc();
            apply_profile(&mut doc, &split_profile(split));
            let mut seen_browser = false;
            for id in ["demod", "resamp", "nr", "vt", "audio"] {
                if doc.blocks[id].placement == Some(Environment::Browser) {
                    seen_browser = true;
                } else {
                    assert!(
                        !seen_browser,
                        "{split:?}: node block {id} downstream of a browser block — illegal crossing"
                    );
                }
            }
        }
    }

    #[test]
    fn ui_sink_wire_survives_when_source_block_is_kept() {
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "src": {"type": "HwSrc", "placement": "node"}
                },
                "wires": [["src.out", "ui:fft"]]
            }"#,
        );
        apply_profile(&mut doc, &Profile::default());
        // ui:<name> wires have no dst block to check; the source
        // survived, so the wire survives.
        assert_eq!(doc.wires.len(), 1);
    }

    #[test]
    fn ui_sink_wire_dropped_when_source_block_is_pruned() {
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "src": {"type": "HwSrc", "placement": "node",
                            "when": {"audio": true}}
                },
                "wires": [["src.out", "ui:audio"]]
            }"#,
        );
        apply_profile(
            &mut doc,
            &Profile {
                audio: false,
                transcribe: false,
                audio_split: AudioSplit::Balanced,
            },
        );
        assert!(doc.blocks.is_empty());
        assert!(doc.wires.is_empty());
    }

    #[test]
    fn defaults_are_a_no_op() {
        // A profile that says "audio on, demod stays where authored"
        // must leave the doc structurally identical.
        let original = doc_from(
            r#"{
                "name": "t",
                "environments": ["node", "browser"],
                "blocks": {
                    "src": {"type": "HwSrc", "placement": "node",
                            "placement_role": "demod"},
                    "audio": {"type": "AudioSink", "placement": "browser",
                              "when": {"audio": true}}
                },
                "wires": [["src.out", "audio.in"]]
            }"#,
        );
        let mut doc = original.clone();
        apply_profile(&mut doc, &Profile::default());
        assert_eq!(doc.blocks.len(), original.blocks.len());
        assert_eq!(doc.wires, original.wires);
        // Authored placement preserved (Balanced split = no override).
        assert_eq!(doc.blocks["src"].placement, Some(Environment::Node));
    }

    #[test]
    fn profile_round_trips_through_json() {
        let p = Profile {
            audio: false,
            transcribe: true,
            audio_split: AudioSplit::Server,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: Profile = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
        // `audio_split` serializes as a lowercase string tag.
        assert!(s.contains("\"audio_split\":\"server\""), "got {s}");
        // Default-on-deserialize: an empty `{}` gives the defaults so
        // the API surface can ship an optional `profile` field without
        // breaking older clients.
        let empty: Profile = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Profile::default());
    }

    #[test]
    fn audio_value_uses_when_expected_value_matches_profile() {
        // `when: { "audio": true }` + `profile.audio = true` keeps.
        // `when: { "audio": false }` + `profile.audio = true` drops —
        // i.e. blocks gated on audio-off get pruned when audio is on.
        // Useful for "audio-off-only" probes (silence detection etc.).
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "silence_probe": {"type": "X", "placement": "node",
                                      "when": {"audio": false}}
                },
                "wires": []
            }"#,
        );
        apply_profile(&mut doc, &Profile::default());
        assert!(!doc.blocks.contains_key("silence_probe"));
    }

    #[test]
    fn idempotent_under_repeated_application() {
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "src":   {"type": "HwSrc", "placement": "node"},
                    "audio": {"type": "AudioSink", "placement": "node",
                              "when": {"audio": true}}
                },
                "wires": [["src.out", "audio.in"]]
            }"#,
        );
        let profile = Profile {
            audio: false,
            transcribe: false,
            audio_split: AudioSplit::Balanced,
        };
        apply_profile(&mut doc, &profile);
        let snapshot = serde_json::to_value(&doc).unwrap();
        apply_profile(&mut doc, &profile);
        let twice = serde_json::to_value(&doc).unwrap();
        assert_eq!(snapshot, twice);
        // sanity: actually pruned
        assert!(!snapshot["blocks"]
            .as_object()
            .unwrap()
            .contains_key("audio"));
        // suppress unused json! warning if serde_json::json! gets
        // tree-shaken at the binary level
        let _ = json!(0);
    }
}
