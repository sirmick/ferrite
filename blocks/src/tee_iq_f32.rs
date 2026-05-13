//! Tee — broadcast one IQ stream to two consumers.
//!
//! The validator rejects presets where one output port is wired more
//! than once (`runtime/src/validate.rs`). This block is the sanctioned
//! fan-out: one input, two outputs, every input sample copied to both.
//! Use it when a single IQ source needs to feed parallel chains — e.g.
//! a server-side FFT tap alongside the main VFO channelizer.
//!
//! IqF32 only for now. Other port types would be sibling blocks
//! (`TeeRealF32`, `TeeFftU8`) until/unless the block framework gains
//! type-parameterized variants.

use anyhow::Result;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, Placement, PortSpec, PortType,
    Work,
};

/// Stateless 1→2 IQ broadcaster.
pub struct TeeIqF32;

impl TeeIqF32 {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for TeeIqF32 {
    fn default() -> Self {
        Self::new()
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for TeeIqF32 {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "TeeIqF32",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[
                PortSpec {
                    name: "out0",
                    port_type: PortType::IqF32,
                },
                PortSpec {
                    name: "out1",
                    port_type: PortType::IqF32,
                },
            ],
            params: &[],
            ai_notes: "Forks an IQ stream into two outputs. No params. Standard pattern: after the channelizer, tee to (a) the FFT for the waterfall and (b) the demod chain.",
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
            .and_then(InputPort::as_iq_f32)
        else {
            return Ok(Work::new());
        };
        // Borrow both outputs independently by splitting the slice —
        // `.iter_mut().find()` twice would double-borrow.
        let (out0, out1) = {
            let outs = &mut *io.outputs;
            let (head, tail) = outs.split_at_mut(1);
            (&mut head[0], &mut tail[0])
        };
        debug_assert_eq!(out0.name, "out0");
        debug_assert_eq!(out1.name, "out1");
        let Some(dst0) = out0.as_iq_f32_mut() else {
            return Ok(Work::new());
        };
        let Some(dst1) = out1.as_iq_f32_mut() else {
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

impl BlockFactory for TeeIqF32 {
    fn construct(_params: &serde_json::Value) -> Result<Box<dyn Block>> {
        Ok(Box::new(TeeIqF32::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::TeeIqF32;
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    #[test]
    fn copies_every_input_sample_to_both_outputs() {
        let input: Vec<Complex<f32>> = (0..32)
            .map(|i| Complex::new(i as f32, -(i as f32)))
            .collect();
        let mut out0 = vec![Complex::new(0.0_f32, 0.0); 32];
        let mut out1 = vec![Complex::new(0.0_f32, 0.0); 32];

        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&input),
        }];
        let mut outputs = [
            OutputPort {
                name: "out0",
                meta: PortMeta::default(),
                buf: OutBuf::IqF32(&mut out0),
            },
            OutputPort {
                name: "out1",
                meta: PortMeta::default(),
                buf: OutBuf::IqF32(&mut out1),
            },
        ];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };

        let mut tee = TeeIqF32::new();
        let w = tee.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], 32);
        assert_eq!(w.produced[0], 32);
        assert_eq!(w.produced[1], 32);
        assert_eq!(out0, input);
        assert_eq!(out1, input);
    }

    #[test]
    fn produces_only_as_many_as_the_smallest_buffer_allows() {
        // Tight output caps the whole op; the bigger output sees just
        // the common prefix written — the rest stays at its initial value.
        let input: Vec<Complex<f32>> = (0..20).map(|i| Complex::new(i as f32, 0.0)).collect();
        let mut out0 = vec![Complex::new(-1.0_f32, -1.0); 10]; // tight
        let mut out1 = vec![Complex::new(-1.0_f32, -1.0); 20]; // roomy

        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&input),
        }];
        let mut outputs = [
            OutputPort {
                name: "out0",
                meta: PortMeta::default(),
                buf: OutBuf::IqF32(&mut out0),
            },
            OutputPort {
                name: "out1",
                meta: PortMeta::default(),
                buf: OutBuf::IqF32(&mut out1),
            },
        ];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };

        let mut tee = TeeIqF32::new();
        let w = tee.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], 10);
        assert_eq!(w.produced[0], 10);
        assert_eq!(w.produced[1], 10);
        for i in 0..10 {
            assert_eq!(out0[i], input[i]);
            assert_eq!(out1[i], input[i]);
        }
        // The bigger output's tail stayed as the sentinel fill value.
        for tail in &out1[10..] {
            assert_eq!(*tail, Complex::new(-1.0, -1.0));
        }
    }
}
