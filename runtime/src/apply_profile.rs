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
//! 2. **Placement rewrite.** Blocks carrying a
//!    `"placement_role": "demod"` tag adopt the profile's
//!    `demod_placement` (when set) regardless of the authored
//!    placement. Lets a user flip the demod between node and browser
//!    without editing the preset JSON; the auto-inserted F32 bridge
//!    appears or disappears at split time.
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

/// User-facing runtime knobs that affect doc shape independently of
/// preset authoring. `Default` keeps the audio chain enabled and
/// leaves placement decisions to the preset author.
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
    /// `AudioSink` (browser-side speech-to-text). Default false —
    /// transcription is opt-in via the receiver's Audio control, the
    /// same build-time mechanism as `audio`. Implies `audio` (the tap
    /// sits on the audio chain); the UI never sets it without `audio`.
    pub transcribe: bool,
    /// Override placement for blocks tagged `"placement_role": "demod"`.
    /// `None` leaves the preset's authored placement in effect; on
    /// flip, the bridge crossing relocates automatically because the
    /// post-split env_split pass dispatches on the source port's type.
    pub demod_placement: Option<Environment>,
    /// Override placement for blocks tagged `"placement_role": "nr"`
    /// (the audio noise-reduction chain). Same mechanism as
    /// `demod_placement`: `None` leaves the authored side in effect.
    /// `AudioNrMono`/`AudioNrStereo` are `Placement::Either` and pure
    /// Rust (wasm-clean), so they run on either half; flipping this
    /// relocates the F32 audio bridge automatically at split time.
    /// Pushing NR node-side lets the headless daemon clean audio for
    /// node-side consumers (e.g. a future node-side transcribe) without
    /// a browser; leaving it browser-side keeps the server lighter.
    pub nr_placement: Option<Environment>,

    /// Transcription side: which side runs `VoiceTranscribe` (the STT
    /// tap). `placement_role: "transcribe"`-tagged blocks adopt this.
    /// `Some(Node)` is the headless path — whisper runs in `ferrited`,
    /// no browser required. `None` leaves the authored side in effect.
    #[serde(default)]
    pub transcribe_placement: Option<Environment>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            audio: true,
            transcribe: false,
            demod_placement: None,
            nr_placement: None,
            transcribe_placement: None,
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

    // Placement-role overrides: a `placement_role`-tagged block adopts
    // the matching profile field when set. Same mechanism for every
    // role; `env_split` relocates the inserted bridge by port type, so
    // adding a role is just a tag + a `Profile` field + one arm here.
    for b in doc.blocks.values_mut() {
        let role = b.placement_role.as_deref();
        let override_env = match role {
            Some("demod") => profile.demod_placement,
            Some("nr") => profile.nr_placement,
            Some("transcribe") => profile.transcribe_placement,
            _ => None,
        };
        if let Some(env) = override_env {
            b.placement = Some(env);
        }
    }
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
                demod_placement: None,
                nr_placement: None,
                transcribe_placement: None,
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
                demod_placement: None,
                nr_placement: None,
                transcribe_placement: None,
            },
        );
        // The "freshness_v9" key isn't on Profile, so the block is kept.
        assert!(doc.blocks.contains_key("future"));
    }

    #[test]
    fn demod_placement_rewrites_role_tagged_blocks_only() {
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node", "browser"],
                "blocks": {
                    "demod": {"type": "FmDemod", "placement": "node",
                              "placement_role": "demod"},
                    "other": {"type": "X", "placement": "node"}
                },
                "wires": []
            }"#,
        );
        apply_profile(
            &mut doc,
            &Profile {
                audio: true,
                transcribe: false,
                demod_placement: Some(Environment::Browser),
                nr_placement: None,
                transcribe_placement: None,
            },
        );
        assert_eq!(
            doc.blocks["demod"].placement,
            Some(Environment::Browser),
            "demod-tagged block moved"
        );
        assert_eq!(
            doc.blocks["other"].placement,
            Some(Environment::Node),
            "untagged block untouched"
        );
    }

    #[test]
    fn nr_placement_rewrites_nr_role_only_and_is_independent_of_demod() {
        // `nr_placement` moves the NR-tagged block; demod stays where
        // authored (its own override is None). Proves the two roles are
        // dispatched independently by the generalized rewrite loop.
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node", "browser"],
                "blocks": {
                    "demod":   {"type": "FmDemod", "placement": "node",
                                "placement_role": "demod"},
                    "audio_nr":{"type": "AudioNrMono", "placement": "browser",
                                "placement_role": "nr"}
                },
                "wires": []
            }"#,
        );
        apply_profile(
            &mut doc,
            &Profile {
                audio: true,
                transcribe: false,
                demod_placement: None,
                nr_placement: Some(Environment::Node),
                transcribe_placement: None,
            },
        );
        assert_eq!(
            doc.blocks["audio_nr"].placement,
            Some(Environment::Node),
            "nr-tagged block moved node-side"
        );
        assert_eq!(
            doc.blocks["demod"].placement,
            Some(Environment::Node),
            "demod left at its authored placement (no demod override)"
        );
    }

    #[test]
    fn transcribe_placement_rewrites_transcribe_role_only() {
        // `transcribe_placement` moves the STT-tagged block node-side
        // (the headless path) and leaves other roles where authored.
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node", "browser"],
                "blocks": {
                    "vt":  {"type": "VoiceTranscribe", "placement": "browser",
                            "placement_role": "transcribe"},
                    "nr":  {"type": "AudioNrMono", "placement": "browser",
                            "placement_role": "nr"}
                },
                "wires": []
            }"#,
        );
        apply_profile(
            &mut doc,
            &Profile {
                audio: true,
                transcribe: false,
                demod_placement: None,
                nr_placement: None,
                transcribe_placement: Some(Environment::Node),
            },
        );
        assert_eq!(
            doc.blocks["vt"].placement,
            Some(Environment::Node),
            "transcribe-tagged block moved node-side"
        );
        assert_eq!(
            doc.blocks["nr"].placement,
            Some(Environment::Browser),
            "nr left at authored placement (no nr override)"
        );
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
                demod_placement: None,
                nr_placement: None,
                transcribe_placement: None,
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
        // Authored placement preserved (no demod_placement override).
        assert_eq!(doc.blocks["src"].placement, Some(Environment::Node));
    }

    #[test]
    fn profile_round_trips_through_json() {
        let p = Profile {
            audio: false,
            transcribe: false,
            demod_placement: Some(Environment::Browser),
            nr_placement: None,
            transcribe_placement: None,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: Profile = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
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
            demod_placement: None,
            nr_placement: None,
            transcribe_placement: None,
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
