//! `WsBridge` — placeholder pair for wires that cross the
//! environment boundary.
//!
//! A flowgraph is one doc with per-block `placement`. Wires whose
//! endpoints straddle that boundary (e.g. server-side `Channelizer →`
//! browser-side `FmDemod`) are auto-split by the scheduler into a
//! `WsBridgeTx` on the producing side and a `WsBridgeRx` on the
//! consuming side, joined by a WebSocket stream that carries the
//! sample frames. Both ends share a `stream_id` so the transport
//! knows which WS channel belongs to which bridge.
//!
//! ### Scope of this commit
//!
//! This is the **block-type placeholder only** — the schedules and the
//! validator need to know these block types exist before auto-insertion
//! lands in M2. `process()` is a no-op: [`WsBridgeTx`] drops its input,
//! [`WsBridgeRx`] produces nothing. Real encode/send/recv/decode logic
//! arrives with the server preset-load commit in M2.
//!
//! Only `IqF32` is wired up today — it's the port type every preset in
//! the repo currently uses at the env boundary (`Channelizer.out`,
//! `FmDemod.in`). `RealF32`, `FftU8`, etc. land as siblings when a
//! preset needs them.

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, ParamKind, ParamSpec, Placement, PortSpec,
    PortType, Work,
};

/// Stream identifier carried on both ends of a bridge pair. Unique
/// within a graph; the server allocates it at preset-load time.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct WsBridgeParams {
    pub stream_id: u32,
}

/// Stream-id param schema — inlined into both bridge specs below. The
/// transport id is fixed once a bridge pair is wired; a reconfigure
/// that changes it is really a re-plumb, not a knob.
const STREAM_ID_PARAM: ParamSpec = ParamSpec {
    key: "stream_id",
    label: "Stream ID",
    kind: ParamKind::Range {
        min: 0.0,
        max: 4_294_967_295.0,
        step: 1.0,
        default: 0.0,
        unit: "",
    },
    mutable_while_streaming: false,
};

// ---------------------------------------------------------------------------
// Tx — server-side egress. Accepts samples, will marshal to the WS
// stream. Native-only: a WASM block never sends over a WS to itself.
// ---------------------------------------------------------------------------

pub struct WsBridgeTx {
    params: WsBridgeParams,
}

impl WsBridgeTx {
    #[must_use]
    pub const fn new(params: WsBridgeParams) -> Self {
        Self { params }
    }

    #[must_use]
    pub const fn stream_id(&self) -> u32 {
        self.params.stream_id
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for WsBridgeTx {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "WsBridgeTx",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[],
            params: &[STREAM_ID_PARAM],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        // Placeholder: consume whatever arrived so upstream sees back-
        // pressure relief, then drop it. Real transport lands in M2.
        let mut w = Work::new();
        if let Some(port) = io.inputs.iter().find(|p| p.name == "in") {
            if let Some(slice) = port.as_iq_f32() {
                w.consumed[0] = slice.len();
            }
        }
        Ok(w)
    }
}

impl BlockFactory for WsBridgeTx {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsBridgeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsBridgeTx::new(p)))
    }
}

// ---------------------------------------------------------------------------
// Rx — browser-side ingress. Emits samples it received from the WS
// stream. WASM-only.
// ---------------------------------------------------------------------------

pub struct WsBridgeRx {
    params: WsBridgeParams,
}

impl WsBridgeRx {
    #[must_use]
    pub const fn new(params: WsBridgeParams) -> Self {
        Self { params }
    }

    #[must_use]
    pub const fn stream_id(&self) -> u32 {
        self.params.stream_id
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for WsBridgeRx {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "WsBridgeRx",
            placement: Placement::WasmOnly,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[STREAM_ID_PARAM],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, _io: &mut BlockIo<'_>) -> Result<Work> {
        // Placeholder: no transport yet, so produce nothing. Once M2
        // lands, this fills the output buffer from decoded WS frames.
        Ok(Work::new())
    }
}

impl BlockFactory for WsBridgeRx {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsBridgeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsBridgeRx::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{WsBridgeParams, WsBridgeRx, WsBridgeTx};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta, Work};
    use num_complex::Complex;

    #[test]
    fn tx_spec_is_native_only_iq_in() {
        let s = WsBridgeTx::spec();
        assert_eq!(s.type_name, "WsBridgeTx");
        assert!(matches!(s.placement, crate::block::Placement::NativeOnly));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 0);
    }

    #[test]
    fn rx_spec_is_wasm_only_iq_out() {
        let s = WsBridgeRx::spec();
        assert_eq!(s.type_name, "WsBridgeRx");
        assert!(matches!(s.placement, crate::block::Placement::WasmOnly));
        assert_eq!(s.inputs.len(), 0);
        assert_eq!(s.outputs.len(), 1);
    }

    #[test]
    fn tx_consumes_all_input_samples() {
        let mut tx = WsBridgeTx::new(WsBridgeParams { stream_id: 7 });
        let input = vec![Complex::new(1.0_f32, 0.0); 32];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&input),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        let w: Work = tx.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], 32);
        assert_eq!(tx.stream_id(), 7);
    }

    #[test]
    fn rx_produces_nothing_until_transport_lands() {
        let mut rx = WsBridgeRx::new(WsBridgeParams { stream_id: 3 });
        let mut out = vec![Complex::new(0.0_f32, 0.0); 16];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        let w: Work = rx.process(&mut io).unwrap();
        assert_eq!(w.produced[0], 0);
    }
}
