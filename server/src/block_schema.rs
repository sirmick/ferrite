//! Wire representation of [`ferrite_blocks::BlockSpec`] used by the
//! `GET /api/blocks` endpoint.
//!
//! Rust-side types live in `ferrite-blocks` with `&'static str` fields
//! and bespoke enums; none of them implement `Serialize`. This module
//! defines owned DTO mirrors that do, plus a conversion pass that walks
//! the inventory registry and surfaces every registered block to the
//! client as one flat array. The shape matches what the flowgraph
//! options dialog expects — one section per block, one control per
//! param, sized and labelled by the schema.

use ferrite_blocks::{
    registry, BlockSpec, ParamKind, ParamSpec, Placement, PortSpec, PortType, ReconfigureScope,
};
use serde::Serialize;

/// One port on a block — matches [`PortSpec`] with an owned name.
#[derive(Serialize, Debug, Clone)]
pub struct PortSchemaDto {
    pub name: String,
    pub port_type: &'static str,
}

/// Wire form of [`ParamKind`]. Tagged by `kind` so the client can
/// pattern-match without re-deriving the variant from field presence.
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamKindDto {
    Range {
        min: f64,
        max: f64,
        step: f64,
        default: f64,
        unit: String,
    },
    EnumNumeric {
        values: Vec<f64>,
        default: f64,
        unit: String,
    },
    EnumString {
        values: Vec<String>,
        default: String,
    },
    Toggle {
        default: bool,
    },
    Text {
        default: String,
    },
}

/// One param on a block — matches [`ParamSpec`] with owned strings and
/// the wire-format scope tag.
#[derive(Serialize, Debug, Clone)]
pub struct ParamSchemaDto {
    pub key: String,
    pub label: String,
    #[serde(flatten)]
    pub kind: ParamKindDto,
    pub reconfig_scope: &'static str,
    /// Plain-prose tooltip / AI-prompt note. See [`ParamSpec::ai_notes`].
    /// Empty during the schema rollout.
    pub ai_notes: String,
}

/// Full schema for one block type. Top-level response is `Vec<Self>`
/// sorted by `type_name` so clients can display without re-sorting.
#[derive(Serialize, Debug, Clone)]
pub struct BlockSchemaDto {
    pub type_name: String,
    pub placement: &'static str,
    pub inputs: Vec<PortSchemaDto>,
    pub outputs: Vec<PortSchemaDto>,
    pub params: Vec<ParamSchemaDto>,
    /// Plain-prose block-level note. See [`BlockSpec::ai_notes`].
    /// Empty during the schema rollout.
    pub ai_notes: String,
}

impl From<&PortSpec> for PortSchemaDto {
    fn from(p: &PortSpec) -> Self {
        Self {
            name: p.name.to_string(),
            port_type: port_type_wire(p.port_type),
        }
    }
}

fn port_type_wire(t: PortType) -> &'static str {
    match t {
        PortType::IqF32 => "iq_f32",
        PortType::IqS16 => "iq_s16",
        PortType::RealF32 => "real_f32",
        PortType::RealI16 => "real_i16",
        PortType::FftF32 => "fft_f32",
        PortType::FftU8 => "fft_u8",
        PortType::Bits => "bits",
        PortType::Frames => "frames",
        PortType::Events => "events",
    }
}

fn placement_wire(p: Placement) -> &'static str {
    match p {
        Placement::NativeOnly => "native",
        Placement::WasmOnly => "browser",
        Placement::Either => "either",
    }
}

impl From<&ParamKind> for ParamKindDto {
    fn from(k: &ParamKind) -> Self {
        match *k {
            ParamKind::Range {
                min,
                max,
                step,
                default,
                unit,
            } => Self::Range {
                min,
                max,
                step,
                default,
                unit: unit.to_string(),
            },
            ParamKind::EnumNumeric {
                values,
                default,
                unit,
            } => Self::EnumNumeric {
                values: values.to_vec(),
                default,
                unit: unit.to_string(),
            },
            ParamKind::EnumString { values, default } => Self::EnumString {
                values: values.iter().map(|s| (*s).to_string()).collect(),
                default: default.to_string(),
            },
            ParamKind::Toggle { default } => Self::Toggle { default },
            ParamKind::Text { default } => Self::Text {
                default: default.to_string(),
            },
        }
    }
}

impl From<&ParamSpec> for ParamSchemaDto {
    fn from(p: &ParamSpec) -> Self {
        Self {
            key: p.key.to_string(),
            label: p.label.to_string(),
            kind: (&p.kind).into(),
            reconfig_scope: reconfig_scope_wire(p.reconfig_scope),
            ai_notes: p.ai_notes.to_string(),
        }
    }
}

fn reconfig_scope_wire(s: ReconfigureScope) -> &'static str {
    s.as_wire_str()
}

impl From<BlockSpec> for BlockSchemaDto {
    fn from(spec: BlockSpec) -> Self {
        Self {
            type_name: spec.type_name.to_string(),
            placement: placement_wire(spec.placement),
            inputs: spec.inputs.iter().map(PortSchemaDto::from).collect(),
            outputs: spec.outputs.iter().map(PortSchemaDto::from).collect(),
            params: spec.params.iter().map(ParamSchemaDto::from).collect(),
            ai_notes: spec.ai_notes.to_string(),
        }
    }
}

/// Snapshot every registered block's spec as a sorted list of DTOs.
pub fn all_block_schemas() -> Vec<BlockSchemaDto> {
    let mut out: Vec<BlockSchemaDto> = registry::entries().map(|e| e.spec().into()).collect();
    out.sort_by(|a, b| a.type_name.cmp(&b.type_name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_block_appears_once() {
        let schemas = all_block_schemas();
        assert!(!schemas.is_empty(), "registry must produce at least one");
        // All names unique.
        let mut names: Vec<&str> = schemas.iter().map(|s| s.type_name.as_str()).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate block name in schemas");
        // Alphabetical ordering — the route contract is a sorted list.
        let pairs: Vec<_> = schemas
            .iter()
            .zip(schemas.iter().skip(1))
            .map(|(a, b)| (a.type_name.as_str(), b.type_name.as_str()))
            .collect();
        for (a, b) in pairs {
            assert!(a < b, "schemas out of order: {a} then {b}");
        }
    }

    #[test]
    fn fm_demod_schema_surfaces_both_params_with_correct_scopes() {
        let schemas = all_block_schemas();
        let fm = schemas
            .iter()
            .find(|s| s.type_name == "FmDemod")
            .expect("FmDemod registered");
        assert_eq!(fm.params.len(), 2);
        let rate = fm
            .params
            .iter()
            .find(|p| p.key == "sample_rate_hz")
            .unwrap();
        assert_eq!(rate.reconfig_scope, "sourceRestart");
        let dev = fm
            .params
            .iter()
            .find(|p| p.key == "max_deviation_hz")
            .unwrap();
        assert_eq!(dev.reconfig_scope, "downstream");
    }

    #[test]
    fn am_demod_schema_surfaces_params_with_correct_scopes() {
        let schemas = all_block_schemas();
        let am = schemas
            .iter()
            .find(|s| s.type_name == "AmDemod")
            .expect("AmDemod registered");
        let rate = am
            .params
            .iter()
            .find(|p| p.key == "sample_rate_hz")
            .unwrap();
        assert_eq!(rate.reconfig_scope, "sourceRestart");
        let gain = am.params.iter().find(|p| p.key == "audio_gain").unwrap();
        assert_eq!(gain.reconfig_scope, "downstream");
    }

    #[test]
    fn decimator_schema_params_are_downstream() {
        let schemas = all_block_schemas();
        let d = schemas
            .iter()
            .find(|s| s.type_name == "Decimator")
            .expect("Decimator registered");
        for key in ["factor", "num_taps", "cutoff_normalized"] {
            let p = d
                .params
                .iter()
                .find(|p| p.key == key)
                .unwrap_or_else(|| panic!("param {key} missing on Decimator"));
            assert_eq!(p.reconfig_scope, "downstream", "param {key}");
        }
    }

    #[test]
    fn soapy_source_surfaces_sdr_knobs_with_notes() {
        // Regression: the source block used to advertise only
        // args/sample_rate/center_freq/gain/channel, hiding the most
        // SDR-specific knobs (agc, antenna, dc_offset, bandwidth) from
        // the AI's `list_block_types` introspection. Every settable
        // param must appear with non-empty ai_notes.
        let schemas = all_block_schemas();
        let src = schemas
            .iter()
            .find(|s| s.type_name == "SoapySource")
            .expect("SoapySource registered");
        for key in [
            "args",
            "sample_rate_hz",
            "center_freq_hz",
            "bandwidth_hz",
            "gain_db",
            "agc",
            "antenna",
            "dc_offset_correction",
            "channel",
        ] {
            let p = src
                .params
                .iter()
                .find(|p| p.key == key)
                .unwrap_or_else(|| panic!("SoapySource param {key} missing from schema"));
            assert!(
                !p.ai_notes.trim().is_empty(),
                "SoapySource param {key} has empty ai_notes"
            );
        }
        // The live-tunable knobs hot-apply (SelfBlock); bandwidth is a
        // hardware-filter change that restarts the source.
        let scope = |k: &str| {
            src.params
                .iter()
                .find(|p| p.key == k)
                .unwrap()
                .reconfig_scope
        };
        assert_eq!(scope("agc"), "self");
        assert_eq!(scope("antenna"), "self");
        assert_eq!(scope("dc_offset_correction"), "self");
        assert_eq!(scope("bandwidth_hz"), "sourceRestart");
    }

    #[test]
    fn range_param_serializes_with_kind_tag() {
        let schemas = all_block_schemas();
        let fm = schemas.iter().find(|s| s.type_name == "FmDemod").unwrap();
        let rate = fm
            .params
            .iter()
            .find(|p| p.key == "sample_rate_hz")
            .unwrap();
        let v = serde_json::to_value(rate).unwrap();
        assert_eq!(v["kind"].as_str(), Some("range"));
        assert_eq!(v["key"].as_str(), Some("sample_rate_hz"));
        assert_eq!(v["unit"].as_str(), Some("Hz"));
        assert!(v["min"].as_f64().is_some());
        assert!(v["max"].as_f64().is_some());
    }
}
