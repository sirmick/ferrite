//! Source composition — fills in a preset's source placeholder.
//!
//! A preset is distributable and hardware-agnostic: the `src` block in
//! a preset doc is a [`SOURCE_SENTINEL_TYPE`] placeholder whose params
//! carry only hints (center freq, sample rate, bandwidth). The concrete
//! source type and its device-specific params come from a separate
//! [`SourceConfig`] supplied at runtime — typically built on the fly
//! from the available hardware.
//!
//! [`compose_source`] takes a preset + source config and returns a
//! runnable [`FlowgraphDoc`]: the placeholder block at [`SOURCE_ID`] is
//! replaced with `source.type_name` and params formed by overlaying
//! `source.params` on top of the placeholder's hints (hints lose on
//! overlap). Everything else in the preset passes through verbatim.
//!
//! The output is a plain `FlowgraphDoc` — all downstream layers
//! (`validate_doc`, `split_for_environment`, `Runtime::load_doc`) work
//! without changes.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::doc::{BlockInstanceDecl, FlowgraphDoc};

/// Reserved block id for the source placeholder. Preset authors wire
/// their graph against `src.<port>` and compose fills a real block in
/// at this id.
pub const SOURCE_ID: &str = "src";

/// Reserved `type` on a preset's source placeholder. Not registered —
/// attempting to load a preset without composing first fails on
/// registry-dependent validation with a clear "not registered" error.
pub const SOURCE_SENTINEL_TYPE: &str = "Source";

/// The source picked for a running preset. `type_name` names a real
/// registered block type (`"SoapySource"`, `"SineSource"`,
/// `"FileSource"`, …); `params` carries device-specific fields that
/// override preset hints on overlap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceConfig {
    #[serde(rename = "type")]
    pub type_name: String,
    /// Device-specific params. Any JSON object shape the target block's
    /// `BlockFactory::construct` accepts. Omitted or `null` = the
    /// preset's hints alone drive the source.
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ComposeError {
    #[error("preset is missing a {SOURCE_ID:?} placeholder block")]
    MissingSource,
    #[error(
        "preset {SOURCE_ID:?} block has type {type_name:?}, expected {SOURCE_SENTINEL_TYPE:?}"
    )]
    WrongType { type_name: String },
}

/// Compose a runnable flowgraph by replacing the preset's source
/// placeholder with the picked source.
///
/// Merge rule for the composed block's `params` (three layers, low →
/// high precedence):
///
/// 1. Placeholder `params` — **hints**. Carried through for keys not
///    set elsewhere (e.g. `sample_rate_hz`, `bandwidth_hz` defaults).
/// 2. Live `source.params` — **user intent**. Overlays the hints.
/// 3. Placeholder `force_params` — **preset-imposed overrides**. Wins
///    over both above so a preset can pin a setting it genuinely
///    requires (e.g. `wbam` setting `agc=false` to avoid the
///    AM-AGC pumping pathology). Use sparingly.
///
/// The placeholder's `placement` pins the composed block to the same
/// environment; the caller doesn't have to thread that through.
pub fn compose_source(
    preset: &FlowgraphDoc,
    source: &SourceConfig,
) -> Result<FlowgraphDoc, ComposeError> {
    let placeholder = preset
        .blocks
        .get(SOURCE_ID)
        .ok_or(ComposeError::MissingSource)?;
    if placeholder.type_name != SOURCE_SENTINEL_TYPE {
        return Err(ComposeError::WrongType {
            type_name: placeholder.type_name.clone(),
        });
    }
    let hinted = merge_params(placeholder.params.as_ref(), &source.params);
    let merged_params = apply_force_params(hinted, placeholder.force_params.as_ref());
    let mut composed = preset.clone();
    composed.blocks.insert(
        SOURCE_ID.to_string(),
        BlockInstanceDecl {
            type_name: source.type_name.clone(),
            params: Some(merged_params),
            placement: placeholder.placement,
            ..Default::default()
        },
    );
    Ok(composed)
}

/// Lay `force_params` on top of an already-merged params object. Used
/// by `compose_source` so a preset can pin source-side settings even
/// against the live `SourceConfig`. No-op when `force` is `None` or a
/// non-object.
fn apply_force_params(mut params: Value, force: Option<&Value>) -> Value {
    let Some(Value::Object(forced)) = force else {
        return params;
    };
    if let Value::Object(p) = &mut params {
        for (k, v) in forced {
            p.insert(k.clone(), v.clone());
        }
    }
    params
}

/// Overlay `source` on top of `hints`. Both are expected to be JSON
/// objects (or null); non-object shapes on either side collapse to
/// whichever side is an object. If both are non-objects, returns an
/// empty object — the target block will default its fields from
/// `Default`.
fn merge_params(hints: Option<&Value>, source: &Value) -> Value {
    let mut out: Map<String, Value> = Map::new();
    if let Some(Value::Object(h)) = hints {
        for (k, v) in h {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Value::Object(s) = source {
        for (k, v) in s {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn preset(src_type: &str, src_params: Value) -> FlowgraphDoc {
        serde_json::from_value(json!({
            "name": "t",
            "environments": ["node"],
            "blocks": {
                "src": { "type": src_type, "placement": "node", "params": src_params },
                "sink": { "type": "Sink", "placement": "node" }
            },
            "wires": [["src.out", "sink.in"]]
        }))
        .unwrap()
    }

    #[test]
    fn fills_type_and_merges_params() {
        let p = preset(
            SOURCE_SENTINEL_TYPE,
            json!({
                "center_freq_hz": 100_100_000.0,
                "sample_rate_hz": 2_400_000.0,
                "bandwidth_hz": 2_000_000.0
            }),
        );
        let source = SourceConfig {
            type_name: "SoapySource".into(),
            params: json!({ "args": "driver=rtlsdr", "gain_db": 30 }),
        };
        let composed = compose_source(&p, &source).unwrap();
        let src = composed.blocks.get(SOURCE_ID).unwrap();
        assert_eq!(src.type_name, "SoapySource");
        let params = src.params.as_ref().unwrap();
        assert_eq!(params["center_freq_hz"].as_f64(), Some(100_100_000.0));
        assert_eq!(params["sample_rate_hz"].as_f64(), Some(2_400_000.0));
        assert_eq!(params["bandwidth_hz"].as_f64(), Some(2_000_000.0));
        assert_eq!(params["args"].as_str(), Some("driver=rtlsdr"));
        assert_eq!(params["gain_db"].as_i64(), Some(30));
    }

    #[test]
    fn source_wins_on_overlap() {
        let p = preset(
            SOURCE_SENTINEL_TYPE,
            json!({ "center_freq_hz": 100e6, "sample_rate_hz": 2.4e6 }),
        );
        let source = SourceConfig {
            type_name: "SineSource".into(),
            params: json!({ "center_freq_hz": 200e6 }),
        };
        let params = compose_source(&p, &source)
            .unwrap()
            .blocks
            .get(SOURCE_ID)
            .unwrap()
            .params
            .clone()
            .unwrap();
        assert_eq!(params["center_freq_hz"].as_f64(), Some(200e6));
        // Non-overlapping hint still carries through.
        assert_eq!(params["sample_rate_hz"].as_f64(), Some(2.4e6));
    }

    #[test]
    fn placeholder_placement_is_preserved() {
        let p = preset(SOURCE_SENTINEL_TYPE, json!({}));
        let source = SourceConfig {
            type_name: "SineSource".into(),
            params: json!({}),
        };
        let src = compose_source(&p, &source)
            .unwrap()
            .blocks
            .remove(SOURCE_ID)
            .unwrap();
        assert_eq!(src.placement, Some(crate::doc::Environment::Node));
    }

    #[test]
    fn hints_alone_when_source_params_empty() {
        let p = preset(
            SOURCE_SENTINEL_TYPE,
            json!({ "center_freq_hz": 100e6, "sample_rate_hz": 2.4e6 }),
        );
        let source = SourceConfig {
            type_name: "SineSource".into(),
            params: Value::Null,
        };
        let params = compose_source(&p, &source)
            .unwrap()
            .blocks
            .get(SOURCE_ID)
            .unwrap()
            .params
            .clone()
            .unwrap();
        assert_eq!(params["center_freq_hz"].as_f64(), Some(100e6));
        assert_eq!(params["sample_rate_hz"].as_f64(), Some(2.4e6));
    }

    #[test]
    fn rejects_missing_src_block() {
        let p: FlowgraphDoc = serde_json::from_value(json!({
            "name": "no-src",
            "environments": ["node"],
            "blocks": { "a": { "type": "Noop" } },
            "wires": []
        }))
        .unwrap();
        let source = SourceConfig {
            type_name: "SineSource".into(),
            params: Value::Null,
        };
        assert_eq!(
            compose_source(&p, &source).unwrap_err(),
            ComposeError::MissingSource
        );
    }

    #[test]
    fn rejects_non_placeholder_type() {
        let p = preset("SoapySource", json!({ "args": "driver=rtlsdr" }));
        let source = SourceConfig {
            type_name: "SoapySource".into(),
            params: Value::Null,
        };
        let err = compose_source(&p, &source).unwrap_err();
        assert_eq!(
            err,
            ComposeError::WrongType {
                type_name: "SoapySource".into()
            }
        );
    }

    #[test]
    fn composed_doc_passes_registry_independent_validation() {
        let p = preset(SOURCE_SENTINEL_TYPE, json!({ "center_freq_hz": 100e6 }));
        let source = SourceConfig {
            type_name: "SineSource".into(),
            params: json!({ "amplitude": 0.5 }),
        };
        let composed = compose_source(&p, &source).unwrap();
        crate::validate::validate_doc(&composed).expect("composed doc validates structurally");
    }

    #[test]
    fn source_config_round_trips_through_serde() {
        let s = SourceConfig {
            type_name: "SoapySource".into(),
            params: json!({ "args": "driver=rtlsdr", "gain_db": 30 }),
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: SourceConfig = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }
}
