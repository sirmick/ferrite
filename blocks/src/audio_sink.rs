//! `AudioSink` — browser-side terminal for demodulated audio.
//!
//! Receives `RealF32` samples and, in the browser, hands them to a
//! `SharedArrayBuffer` ring that an `AudioWorklet` drains on its own
//! 48 kHz clock. Mirrors the TS `AudioSink` in
//! `web/src/lib/audio/audioSinkBlock.ts` byte-for-byte at the params /
//! port level so a preset authored against the Rust registry parses
//! today and runs natively once the M4 browser WASM host lands.
//!
//! ### Scope of this commit
//!
//! This is the **block-type placeholder only**. `Placement::WasmOnly`
//! means `env_split` carves it out of the node-side doc entirely, so
//! `ferrited` never instantiates it. The `process` impl is a no-op
//! guard that consumes the input without producing anything. The real
//! Web Audio glue lands at M4 when the browser loads the Rust runtime
//! as WASM and we wire the SAB ring through `wasm-bindgen`.

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, ParamKind, ParamSpec, Placement, PortSpec,
    PortType, Work,
};

/// Ring capacity default — ~170 ms at 48 kHz. Matches the TS
/// `AudioSink` default so presets carry the same numbers across the
/// TS→Rust cut-over.
const DEFAULT_BUFFER_SAMPLES: usize = 8192;

/// `f64` mirror of [`DEFAULT_BUFFER_SAMPLES`] for the param schema.
/// Kept as its own constant so the cast is explicit and exact rather
/// than recomputed at schema-construction time.
const DEFAULT_BUFFER_SAMPLES_F64: f64 = 8192.0;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AudioSinkParams {
    /// Ring capacity in samples. Must be a power of two when the real
    /// SAB ring lands; the placeholder does not enforce it today.
    pub buffer_samples: usize,
}

impl Default for AudioSinkParams {
    fn default() -> Self {
        Self {
            buffer_samples: DEFAULT_BUFFER_SAMPLES,
        }
    }
}

const BUFFER_SAMPLES_PARAM: ParamSpec = ParamSpec {
    key: "buffer_samples",
    label: "Ring capacity",
    kind: ParamKind::Range {
        min: 256.0,
        max: 1_048_576.0,
        step: 1.0,
        default: DEFAULT_BUFFER_SAMPLES_F64,
        unit: "samples",
    },
    mutable_while_streaming: false,
};

pub struct AudioSink {
    _params: AudioSinkParams,
}

impl AudioSink {
    #[must_use]
    pub const fn new(params: AudioSinkParams) -> Self {
        Self { _params: params }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AudioSink {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AudioSink",
            placement: Placement::WasmOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[],
            params: &[BUFFER_SAMPLES_PARAM],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        // Placeholder: no Web Audio in the native build, so just drain
        // the input so upstream doesn't back-pressure in lock-step
        // tests. Real SAB write lands in M4.
        let mut w = Work::new();
        if let Some(port) = io.inputs.iter().find(|p| p.name == "in") {
            if let crate::block::InBuf::RealF32(slice) = &port.buf {
                w.consumed[0] = slice.len();
            }
        }
        Ok(w)
    }
}

impl BlockFactory for AudioSink {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AudioSinkParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AudioSink::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioSink, AudioSinkParams, DEFAULT_BUFFER_SAMPLES};
    use crate::block::{Block, Placement};

    #[test]
    fn spec_is_wasm_only_real_in() {
        let s = AudioSink::spec();
        assert_eq!(s.type_name, "AudioSink");
        assert!(matches!(s.placement, Placement::WasmOnly));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 0);
        assert_eq!(s.inputs[0].port_type, crate::block::PortType::RealF32);
    }

    #[test]
    fn defaults_match_ts() {
        let p = AudioSinkParams::default();
        assert_eq!(p.buffer_samples, DEFAULT_BUFFER_SAMPLES);
    }

    #[test]
    fn params_round_trip_through_json() {
        let value = serde_json::json!({ "buffer_samples": 4096 });
        let p: AudioSinkParams = serde_json::from_value(value).unwrap();
        assert_eq!(p.buffer_samples, 4096);
    }
}
