//! Contract test: every shipped *audio* preset still composes, splits,
//! validates and instantiates for both environments after a source is
//! plugged in. No such test existed — which is how the `wbam`
//! `force_params` key typo and earlier wiring drift slipped through.
//! This exercises the exact build pipeline `AppState::start` runs.

use ferrite_runtime::{
    apply_profile, compose_source, inject_narrow_fft_taps, instantiate_flowgraph,
    split_for_environment, validate_doc, Environment, FlowgraphDoc, InventorySpecRegistry, Profile,
    SourceConfig,
};

fn load(name: &str) -> FlowgraphDoc {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("flowgraphs")
        .join(format!("{name}.json"));
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
    )
    .unwrap_or_else(|e| panic!("{name}.json parse: {e}"))
}

fn sine_source() -> SourceConfig {
    serde_json::from_value(serde_json::json!({
        "type": "SineSource",
        "params": {
            "rate_hz": 2_400_000.0,
            "center_freq_hz": 100_000_000.0,
            "tone_freq_abs_hz": 100_050_000.0,
            "amplitude": 0.5
        }
    }))
    .unwrap()
}

/// Run the full `AppState::start` build for both environments.
fn build_both_envs(name: &str) -> (FlowgraphDoc, FlowgraphDoc) {
    let preset = load(name);
    let mut composed =
        compose_source(&preset, &sine_source()).unwrap_or_else(|e| panic!("{name} compose: {e}"));
    inject_narrow_fft_taps(&mut composed);
    apply_profile(&mut composed, &Profile::default(), &InventorySpecRegistry);

    let mut halves = Vec::new();
    for env in [Environment::Node, Environment::Browser] {
        let split = split_for_environment(&composed, env, &InventorySpecRegistry)
            .unwrap_or_else(|e| panic!("{name} split {env:?}: {e}"));
        let validated =
            validate_doc(&split).unwrap_or_else(|e| panic!("{name} validate {env:?}: {e:?}"));
        instantiate_flowgraph(&validated, &InventorySpecRegistry, env)
            .unwrap_or_else(|e| panic!("{name} instantiate {env:?}: {e:?}"));
        halves.push(split);
    }
    let browser = halves.pop().unwrap();
    let node = halves.pop().unwrap();
    (node, browser)
}

#[test]
fn all_shipped_audio_presets_build() {
    // Core-block audio presets only. `nwr` / `*-audio-record` pull
    // feature-gated decoders (EAS=multimon, etc.) that aren't in the
    // runtime crate's default test registry; their build is covered by
    // the feature-enabled e2e suites.
    for name in ["wbfm", "wbfm_stereo", "nbfm", "wbam"] {
        build_both_envs(name);
    }
}

#[test]
fn wbfm_shaper_on_node_audio_branch_rds_stays_raw() {
    let (node, _browser) = build_both_envs("wbfm");
    // AudioShaper sits node-side on the audio branch.
    assert!(
        node.blocks.contains_key("shaper"),
        "wbfm: shaper missing from node half",
    );
    assert_eq!(node.blocks["shaper"].type_name, "AudioShaper");
    // RDS still consumes the RAW discriminator (not the shaper output).
    let rds_src_is_raw = node.wires.iter().any(|w| {
        let (s, _) = w.src.split_once('.').unwrap_or((w.src.as_str(), ""));
        let (d, _) = w.dst.split_once('.').unwrap_or((w.dst.as_str(), ""));
        s == "audio_tee" && d == "rds"
    });
    assert!(rds_src_is_raw, "wbfm: RDS no longer fed from raw audio_tee");
}

#[test]
fn nbfm_has_squelch_and_shaper() {
    let (node, browser) = build_both_envs("nbfm");
    assert_eq!(
        node.blocks.get("squelch").map(|b| b.type_name.as_str()),
        Some("Squelch"),
        "nbfm: Squelch missing on node half",
    );
    assert_eq!(
        browser.blocks.get("shaper").map(|b| b.type_name.as_str()),
        Some("AudioShaper"),
        "nbfm: AudioShaper missing on browser half",
    );
}

#[test]
fn wbam_shaper_present_and_agc_pinned_off() {
    let preset = load("wbam");
    // The earlier-fixed force_params: hardware AGC pinned off for AM.
    let composed = compose_source(&preset, &sine_source()).unwrap();
    let src = &composed.blocks["src"];
    let params = src.params.as_ref().expect("src params");
    assert_eq!(
        params.get("agc"),
        Some(&serde_json::Value::Bool(false)),
        "wbam: hardware AGC not pinned off (force_params regressed)",
    );
    let (_n, browser) = build_both_envs("wbam");
    assert_eq!(
        browser.blocks.get("shaper").map(|b| b.type_name.as_str()),
        Some("AudioShaper"),
        "wbam: AudioShaper missing",
    );
}

#[test]
fn wbfm_node_browser_bridge_ids_pair_up() {
    let (node, browser) = build_both_envs("wbfm");
    let txs: std::collections::BTreeSet<String> = node
        .blocks
        .keys()
        .filter(|k| k.contains("bridge_tx"))
        .map(|k| k.rsplit('_').next().unwrap().to_string())
        .collect();
    let rxs: std::collections::BTreeSet<String> = browser
        .blocks
        .keys()
        .filter(|k| k.contains("bridge_rx"))
        .map(|k| k.rsplit('_').next().unwrap().to_string())
        .collect();
    println!("node tx ids: {txs:?}");
    println!("browser rx ids: {rxs:?}");
    assert!(
        rxs.is_subset(&txs),
        "browser RX ids {rxs:?} not all in node TX ids {txs:?}"
    );
}

/// Drift guard: the channelizer integer-decimates, so the source rate
/// must be an exact multiple of the channel rate. A non-integer ratio
/// (e.g. the old 2.4 MS/s → 250 kHz = 9.6) makes the actual output
/// rate differ from what downstream assumes — a slow producer/consumer
/// drift that surfaces as a periodic underrun blip in the audio.
#[test]
fn audio_preset_rates_are_drift_free() {
    fn f64_param(p: Option<&serde_json::Value>, key: &str) -> Option<f64> {
        p.and_then(|v| v.get(key))
            .and_then(serde_json::Value::as_f64)
    }

    for name in ["wbfm", "wbfm_stereo", "nbfm", "wbam"] {
        let doc = load(name);
        let src = doc.blocks.get("src").expect("src block");
        let src_rate = f64_param(src.params.as_ref(), "sample_rate_hz")
            .unwrap_or_else(|| panic!("{name}: src has no sample_rate_hz"));

        let chan = doc
            .blocks
            .values()
            .find(|b| b.type_name == "Channelizer")
            .unwrap_or_else(|| panic!("{name}: no Channelizer"));
        let chan_in = f64_param(chan.params.as_ref(), "input_rate_hz")
            .unwrap_or_else(|| panic!("{name}: chan has no input_rate_hz"));
        let chan_out = f64_param(chan.params.as_ref(), "output_rate_hz")
            .unwrap_or_else(|| panic!("{name}: chan has no output_rate_hz"));

        assert_eq!(
            chan_in, src_rate,
            "{name}: channelizer input_rate_hz ({chan_in}) must match the source \
             sample_rate_hz hint ({src_rate}) — a mismatch hides the real ratio",
        );

        let factor = (src_rate / chan_out).round();
        let actual_out = src_rate / factor;
        assert!(
            (actual_out - chan_out).abs() < 1e-6,
            "{name}: {src_rate} / {chan_out} is non-integer (factor {factor} -> actual \
             {actual_out} Hz). Pick a source rate that integer-divides to the channel \
             rate, else the chain drifts and blips periodically.",
        );
    }
}
