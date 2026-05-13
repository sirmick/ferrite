//! Tee — broadcast one real-f32 stream to two consumers.
//!
//! Real-valued sibling of [`TeeIqF32`](crate::TeeIqF32). Same contract:
//! one input, two outputs, every input sample mirrored to both. Use it
//! when a single audio / MPX stream needs to feed parallel chains —
//! e.g. the FM demodulator output fed into both the audio path and an
//! RDS decoder.

use anyhow::Result;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, Placement, PortSpec, PortType,
    Work,
};

/// Stateless 1→2 real-f32 broadcaster.
pub struct TeeRealF32;

impl TeeRealF32 {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for TeeRealF32 {
    fn default() -> Self {
        Self::new()
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for TeeRealF32 {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "TeeRealF32",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[
                PortSpec {
                    name: "out0",
                    port_type: PortType::RealF32,
                },
                PortSpec {
                    name: "out1",
                    port_type: PortType::RealF32,
                },
            ],
            params: &[],
            ai_notes: "Forks a real-valued audio stream into two outputs. Mirror of `TeeIqF32` for post-demod chains (e.g. audio + decoder branch sharing the same demodulated signal).",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        else {
            return Ok(Work::new());
        };
        let (out0, out1) = {
            let outs = &mut *io.outputs;
            let (head, tail) = outs.split_at_mut(1);
            (&mut head[0], &mut tail[0])
        };
        debug_assert_eq!(out0.name, "out0");
        debug_assert_eq!(out1.name, "out1");
        let Some(dst0) = out0.as_real_f32_mut() else {
            return Ok(Work::new());
        };
        let Some(dst1) = out1.as_real_f32_mut() else {
            return Ok(Work::new());
        };
        let n = src.len().min(dst0.len()).min(dst1.len());
        dst0[..n].copy_from_slice(&src[..n]);
        dst1[..n].copy_from_slice(&src[..n]);

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        w.produced[1] = n;
        Ok(w)
    }
}

impl BlockFactory for TeeRealF32 {
    fn construct(_params: &serde_json::Value) -> Result<Box<dyn Block>> {
        Ok(Box::new(TeeRealF32::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::TeeRealF32;
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};

    #[test]
    fn copies_every_input_sample_to_both_outputs() {
        let input: Vec<f32> = (0..32).map(|i| i as f32 * 0.25).collect();
        let mut out0 = vec![0.0_f32; 32];
        let mut out1 = vec![0.0_f32; 32];

        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(&input),
        }];
        let mut outputs = [
            OutputPort {
                name: "out0",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut out0),
            },
            OutputPort {
                name: "out1",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut out1),
            },
        ];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };

        let mut tee = TeeRealF32::new();
        let w = tee.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], 32);
        assert_eq!(w.produced[0], 32);
        assert_eq!(w.produced[1], 32);
        assert_eq!(out0, input);
        assert_eq!(out1, input);
    }
}
