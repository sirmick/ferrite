//! DeepFilterNet3 neural noise reduction.
//!
//! Scaffold stub — the real DFN3 path lands in task #38 (via the
//! `deep_filter` crate + tract ONNX runtime + a 48 kHz resample
//! bridge). For now the stage is a no-op passthrough so the block
//! wiring and parameter plumbing can be verified end-to-end.

pub struct NeuralStage;

impl NeuralStage {
    #[must_use]
    pub fn new(_attenuation_db: f32, _input_rate_hz: f64) -> Self {
        Self
    }

    pub fn reset(&mut self) {}

    pub fn run(&mut self, _buf: &mut [f32]) {
        // No-op until task #38 lands the DFN3 inference path.
    }
}
