//! STFT-based spectral noise reduction.
//!
//! Scaffold stub — the real `Boll` and `MmseLsa` implementations land
//! in task #37. For now the stage is a no-op passthrough so the block
//! wiring and parameter plumbing can be exercised end-to-end without
//! pulling in the full STFT/noise-tracker machinery.

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SpectralMethod {
    /// 1979 Boll spectral subtraction.
    #[serde(rename = "boll")]
    Boll,
    /// Log-spectral-amplitude MMSE (Ephraim–Malah).
    #[serde(rename = "mmse_lsa")]
    MmseLsa,
}

impl Default for SpectralMethod {
    fn default() -> Self {
        Self::Boll
    }
}

/// Stub spectral NR stage — passthrough placeholder.
pub struct SpectralStage;

impl SpectralStage {
    #[must_use]
    pub fn new(
        _method: SpectralMethod,
        _block_size: usize,
        _oversub: f32,
        _floor: f32,
        _noise_alpha: f32,
    ) -> Self {
        Self
    }

    pub fn reset(&mut self) {}

    pub fn run(&mut self, _buf: &mut [f32]) {
        // No-op until task #37 lands the STFT path.
    }
}
