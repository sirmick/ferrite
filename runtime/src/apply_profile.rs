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
//! 2. **Placement cut.** The [`AudioSplit`] picks where the node↔browser
//!    boundary falls, as a *graph* cut rather than a per-block role list:
//!
//!    * **Balanced** — leave every block at its preset-authored
//!      placement (the historical default; a no-op).
//!    * **Server** — pull everything node-side except `WasmOnly` blocks
//!      (the WebAudio `AudioSink`, which must run in the tab). Headless:
//!      demod, decode, audio DSP all run in `ferrited`.
//!    * **Browser** — cut at the **channelizer output**. Everything
//!      reachable downstream of a `Channelizer` runs browser-side, so the
//!      daemon streams a narrowband channel (tens of kHz) and the tab
//!      does the per-channel demod / decode / audio. The wideband display
//!      path (`fft → logmag → signals`) hangs off the source *upstream*
//!      of the channelizer, so it always stays node and streams to the
//!      tab as before. Cutting at the channelizer — not the source —
//!      keeps the cross-link small (the raw wideband IQ never leaves the
//!      daemon).
//!
//! The Browser cut respects `NativeOnly` blocks: any subchain that leads
//! to one (e.g. the AIS decoder) stays node, since a `NativeOnly` leaf
//! pins node and everything feeding it must too. Because the channelizer
//! itself stays node and the cut only ever sends node→browser, an illegal
//! browser→node crossing is unrepresentable — the same safety property
//! the old linear-spine model had, but now covering *branches* (decode +
//! audio off one tee), which is what the spine model couldn't express.
//!
//! The pass runs after [`compose_source`](crate::compose),
//! [`inject_dc_block_taps`](crate::inject_dc_block),
//! [`inject_narrow_fft_taps`](crate::inject_narrow_fft) and
//! [`inject_signal_list_taps`](crate::inject_signal_list) (so the doc has
//! its final source-side shape, including the narrow-FFT tee on the
//! channelizer output) and before
//! [`split_for_environment`](crate::env_split). It needs a
//! [`SpecRegistry`] to know each block's placement capability
//! (`Either` / `NativeOnly` / `WasmOnly`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use ferrite_blocks::Placement;

use crate::doc::{BlockInstanceDecl, Environment, FlowgraphDoc};
use crate::instantiate::SpecRegistry;
use crate::validate::split_endpoint;

/// Block type that marks the wideband→narrowband boundary. The Browser
/// cut is anchored at its output.
const CHANNELIZER: &str = "Channelizer";

/// Where the node↔browser boundary falls. See the module docs for what
/// each variant does as a graph cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioSplit {
    /// Thin server: cut at the channelizer output — everything downstream
    /// runs browser-side. IQ for one channel is streamed across and
    /// demod/decode/audio all happen in the tab.
    Browser,
    /// Leave each block at its preset-authored placement (the default).
    Balanced,
    /// Headless: pull everything node-side except the `WasmOnly`
    /// `AudioSink`. Demod, decode and audio DSP run in `ferrited`; a
    /// node-placed `VoiceTranscribe` tap therefore has node-side audio to
    /// read, so whisper runs with no browser.
    Server,
}

impl AudioSplit {
    /// Placement for a *newly injected* audio-spine block (the
    /// `VoiceTranscribe` tap) that has no authored placement to fall back
    /// on. `Server` → node (headless); everything else → browser (the
    /// default audio side, and where the Browser cut puts the audio
    /// chain too).
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
    /// Where the node↔browser boundary is cut. See [`AudioSplit`].
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
/// `registry` supplies each block type's placement capability, which the
/// placement cut needs to keep `NativeOnly`/`WasmOnly` blocks on the side
/// they can actually run.
pub fn apply_profile(doc: &mut FlowgraphDoc, profile: &Profile, registry: &dyn SpecRegistry) {
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

    match profile.audio_split {
        AudioSplit::Balanced => {} // leave authored placement
        AudioSplit::Server => place_server(doc, registry),
        AudioSplit::Browser => place_browser(doc, registry),
    }
}

/// Placement capability of a block type, or `None` when the registry
/// doesn't know it (custom/injected types the cut can leave alone).
fn capability(registry: &dyn SpecRegistry, type_name: &str) -> Option<Placement> {
    registry.get(type_name).map(|s| s.placement)
}

/// `Server`: everything node-side except `WasmOnly` blocks, which must
/// run in the tab (the WebAudio sink). The single remaining node→browser
/// crossing feeds that sink.
fn place_server(doc: &mut FlowgraphDoc, registry: &dyn SpecRegistry) {
    for b in doc.blocks.values_mut() {
        let env = match capability(registry, &b.type_name) {
            Some(Placement::WasmOnly) => Environment::Browser,
            _ => Environment::Node,
        };
        b.placement = Some(env);
    }
}

/// `Browser`: cut at the channelizer output. Every block reachable
/// downstream of a `Channelizer` goes browser-side, except any subchain
/// that leads to a `NativeOnly` block (which stays node). The channelizer
/// and everything upstream of it keep their authored placement (node).
fn place_browser(doc: &mut FlowgraphDoc, registry: &dyn SpecRegistry) {
    let channelizers: BTreeSet<String> = doc
        .blocks
        .iter()
        .filter(|(_, b)| b.type_name == CHANNELIZER)
        .map(|(id, _)| id.clone())
        .collect();
    // No channelizer → no defined cut point. Leave authored placement;
    // the UI shouldn't offer Browser for such presets.
    if channelizers.is_empty() {
        return;
    }

    // Forward + reverse adjacency over real (non-`ui:`) wires.
    let mut fwd: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut rev: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for w in &doc.wires {
        if w.ui_sink_name().is_some() {
            continue;
        }
        let s = split_endpoint(&w.src).0.to_string();
        let d = split_endpoint(&w.dst).0.to_string();
        fwd.entry(s.clone()).or_default().push(d.clone());
        rev.entry(d).or_default().push(s);
    }

    // Downstream set: everything reachable forward from a channelizer's
    // successors (the channelizers themselves excluded — they stay node).
    let mut downstream: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = channelizers
        .iter()
        .filter_map(|c| fwd.get(c))
        .flatten()
        .cloned()
        .collect();
    while let Some(n) = stack.pop() {
        if channelizers.contains(&n) || !downstream.insert(n.clone()) {
            continue;
        }
        if let Some(succ) = fwd.get(&n) {
            stack.extend(succ.iter().cloned());
        }
    }

    // Tainted set: every `NativeOnly` block plus all of its ancestors —
    // anything that can reach a `NativeOnly` block downstream must stay
    // node to avoid a browser→node crossing.
    let mut tainted: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = doc
        .blocks
        .iter()
        .filter(|(_, b)| {
            matches!(
                capability(registry, &b.type_name),
                Some(Placement::NativeOnly)
            )
        })
        .map(|(id, _)| id.clone())
        .collect();
    while let Some(n) = stack.pop() {
        if !tainted.insert(n.clone()) {
            continue;
        }
        if let Some(preds) = rev.get(&n) {
            stack.extend(preds.iter().cloned());
        }
    }

    for id in &downstream {
        let Some(b) = doc.blocks.get_mut(id) else {
            continue;
        };
        // WasmOnly must be browser regardless; otherwise a tainted block
        // (feeds a NativeOnly leaf) stays node, everything else → browser.
        let env = if matches!(
            capability(registry, &b.type_name),
            Some(Placement::WasmOnly)
        ) {
            Environment::Browser
        } else if tainted.contains(id) {
            Environment::Node
        } else {
            Environment::Browser
        };
        b.placement = Some(env);
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
    use ferrite_blocks::{BlockSpec, ParamSpec, PortSpec};
    use serde_json::json;

    /// Stub registry: placement capability keyed off type name. The
    /// returned `BlockSpec`'s other fields are inert — `apply_profile`
    /// only reads `.placement`.
    struct Reg;
    impl SpecRegistry for Reg {
        fn get(&self, type_name: &str) -> Option<BlockSpec> {
            const NO_PORTS: &[PortSpec] = &[];
            const NO_PARAMS: &[ParamSpec] = &[];
            let placement = match type_name {
                "Source" | "SoapySource" | "AisDecoder" => Placement::NativeOnly,
                "AudioSink" => Placement::WasmOnly,
                _ => Placement::Either,
            };
            Some(BlockSpec {
                type_name: "stub",
                placement,
                inputs: NO_PORTS,
                outputs: NO_PORTS,
                params: NO_PARAMS,
                ai_notes: "",
            })
        }
    }

    fn doc_from(json: &str) -> FlowgraphDoc {
        serde_json::from_str(json).unwrap()
    }

    fn place(doc: &FlowgraphDoc, id: &str) -> Option<Environment> {
        doc.blocks[id].placement
    }

    // ---- gating (unchanged behaviour) -----------------------------------

    #[test]
    fn audio_false_drops_blocks_with_when_audio_true() {
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "src":   {"type": "Source", "placement": "node"},
                    "audio": {"type": "AudioSink", "placement": "browser",
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
            &Reg,
        );
        assert!(doc.blocks.contains_key("src"));
        assert!(!doc.blocks.contains_key("audio"));
        assert!(doc.wires.is_empty());
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
        apply_profile(&mut doc, &Profile::default(), &Reg);
        assert!(doc.blocks.contains_key("future"));
    }

    // ---- the channelizer cut --------------------------------------------

    /// A packet-style preset: wideband display off the tee, a channelizer
    /// feeding a demod that fans (via a tee) into a node-or-browser
    /// decoder branch and a browser AudioSink branch.
    fn packet_doc() -> FlowgraphDoc {
        doc_from(
            r#"{
                "name": "packet",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":     {"type": "Source", "placement": "node"},
                    "tee":     {"type": "TeeIqF32", "placement": "node"},
                    "fft":     {"type": "FFT", "placement": "node"},
                    "logmag":  {"type": "LogMagU8", "placement": "node"},
                    "signals": {"type": "SignalList", "placement": "node"},
                    "chan":    {"type": "Channelizer", "placement": "node"},
                    "demod":   {"type": "FmDemod", "placement": "node"},
                    "atee":    {"type": "TeeRealF32", "placement": "node"},
                    "resamp":  {"type": "RealF32Resamp", "placement": "node"},
                    "packet":  {"type": "PacketDemod", "placement": "node"},
                    "aresamp": {"type": "RealF32Resamp", "placement": "browser"},
                    "audio":   {"type": "AudioSink", "placement": "browser"}
                },
                "wires": [
                    ["src.out", "tee.in"],
                    ["tee.out0", "chan.in"],
                    ["tee.out1", "fft.in"],
                    ["fft.out", "logmag.in"],
                    ["logmag.out", "signals.in"],
                    ["signals.out", "ui:fft"],
                    ["chan.out", "demod.in"],
                    ["demod.out", "atee.in"],
                    ["atee.out0", "resamp.in"],
                    ["resamp.out", "packet.in"],
                    ["packet.events", "ui:aprs"],
                    ["atee.out1", "aresamp.in"],
                    ["aresamp.out", "audio.in"]
                ]
            }"#,
        )
    }

    fn split_profile(split: AudioSplit) -> Profile {
        Profile {
            audio: true,
            transcribe: false,
            audio_split: split,
        }
    }

    #[test]
    fn browser_cut_pushes_downstream_of_channelizer_into_the_tab() {
        let mut doc = packet_doc();
        apply_profile(&mut doc, &split_profile(AudioSplit::Browser), &Reg);

        // Source side + wideband display + channelizer stay node.
        for id in ["src", "tee", "fft", "logmag", "signals", "chan"] {
            assert_eq!(place(&doc, id), Some(Environment::Node), "{id} stays node");
        }
        // Everything downstream of the channelizer moves browser —
        // including the PacketDemod, which is `Either` (decode in tab).
        for id in ["demod", "atee", "resamp", "packet", "aresamp", "audio"] {
            assert_eq!(
                place(&doc, id),
                Some(Environment::Browser),
                "{id} moves browser"
            );
        }
    }

    #[test]
    fn browser_cut_keeps_nativeonly_subchain_node() {
        // Swap the decoder for a NativeOnly one (AIS): its branch must
        // stay node even under Browser, while the audio branch still
        // moves to the tab.
        let mut doc = packet_doc();
        doc.blocks.get_mut("packet").unwrap().type_name = "AisDecoder".to_string();
        apply_profile(&mut doc, &split_profile(AudioSplit::Browser), &Reg);

        // The NativeOnly decoder and everything feeding it past the
        // channelizer stay node...
        for id in ["chan", "demod", "atee", "resamp", "packet"] {
            assert_eq!(
                place(&doc, id),
                Some(Environment::Node),
                "{id} pinned node by the NativeOnly decoder downstream"
            );
        }
        // ...but the parallel audio branch is free to move browser.
        assert_eq!(place(&doc, "aresamp"), Some(Environment::Browser));
        assert_eq!(place(&doc, "audio"), Some(Environment::Browser));
    }

    #[test]
    fn browser_cut_never_creates_browser_to_node_crossing() {
        // For both the all-Either and NativeOnly-decoder shapes, no real
        // wire may go browser→node after the cut.
        for native in [false, true] {
            let mut doc = packet_doc();
            if native {
                doc.blocks.get_mut("packet").unwrap().type_name = "AisDecoder".to_string();
            }
            apply_profile(&mut doc, &split_profile(AudioSplit::Browser), &Reg);
            for w in &doc.wires {
                if w.ui_sink_name().is_some() {
                    continue;
                }
                let s = split_endpoint(&w.src).0;
                let d = split_endpoint(&w.dst).0;
                let (se, de) = (place(&doc, s), place(&doc, d));
                assert!(
                    !(se == Some(Environment::Browser) && de == Some(Environment::Node)),
                    "native={native}: {s}({se:?}) → {d}({de:?}) is a browser→node crossing"
                );
            }
        }
    }

    #[test]
    fn server_pulls_everything_node_except_wasm_sink() {
        let mut doc = packet_doc();
        apply_profile(&mut doc, &split_profile(AudioSplit::Server), &Reg);
        for id in [
            "src", "tee", "fft", "logmag", "signals", "chan", "demod", "atee", "resamp", "packet",
            "aresamp",
        ] {
            assert_eq!(place(&doc, id), Some(Environment::Node), "{id} node");
        }
        // The WasmOnly WebAudio sink can't move — stays in the tab.
        assert_eq!(place(&doc, "audio"), Some(Environment::Browser));
    }

    #[test]
    fn balanced_leaves_authored_placement() {
        let mut doc = packet_doc();
        apply_profile(&mut doc, &split_profile(AudioSplit::Balanced), &Reg);
        assert_eq!(place(&doc, "demod"), Some(Environment::Node));
        assert_eq!(place(&doc, "resamp"), Some(Environment::Node));
        assert_eq!(place(&doc, "packet"), Some(Environment::Node));
        assert_eq!(place(&doc, "aresamp"), Some(Environment::Browser));
        assert_eq!(place(&doc, "audio"), Some(Environment::Browser));
    }

    #[test]
    fn browser_no_channelizer_is_a_no_op_on_placement() {
        // A preset without a channelizer has no defined cut — placements
        // are left exactly as authored.
        let mut doc = doc_from(
            r#"{
                "name": "raw",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":   {"type": "Source", "placement": "node"},
                    "demod": {"type": "FmDemod", "placement": "node"},
                    "audio": {"type": "AudioSink", "placement": "browser"}
                },
                "wires": [["src.out", "demod.in"], ["demod.out", "audio.in"]]
            }"#,
        );
        apply_profile(&mut doc, &split_profile(AudioSplit::Browser), &Reg);
        assert_eq!(place(&doc, "demod"), Some(Environment::Node));
        assert_eq!(place(&doc, "audio"), Some(Environment::Browser));
    }

    #[test]
    fn idempotent_under_repeated_application() {
        let mut doc = packet_doc();
        let p = split_profile(AudioSplit::Browser);
        apply_profile(&mut doc, &p, &Reg);
        let once = serde_json::to_value(&doc).unwrap();
        apply_profile(&mut doc, &p, &Reg);
        let twice = serde_json::to_value(&doc).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn ui_sink_wire_survives_when_source_block_is_kept() {
        let mut doc = doc_from(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "src": {"type": "Source", "placement": "node"}
                },
                "wires": [["src.out", "ui:fft"]]
            }"#,
        );
        apply_profile(&mut doc, &Profile::default(), &Reg);
        assert_eq!(doc.wires.len(), 1);
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
        assert!(s.contains("\"audio_split\":\"server\""), "got {s}");
        let empty: Profile = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Profile::default());
        let _ = json!(0);
    }
}
