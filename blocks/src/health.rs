//! Shared pieces for [`crate::block::Block::health`]: what a decoder
//! reports so "0 decodes" can be told apart — no signal reaching it, a
//! signal it can't sync to, or syncs that never decode.

use num_complex::Complex;

/// Smoothed input power, dBFS (full scale = 1.0 amplitude). One-pole
/// average with a time constant counted in samples, so it works at any
/// rate without knowing it exactly.
#[derive(Debug, Clone)]
pub struct LevelMeter {
    tau_samples: f64,
    power: f64,
    seen: u64,
}

impl LevelMeter {
    /// `tau_samples` ≈ rate × seconds to average over.
    #[must_use]
    pub fn new(tau_samples: f64) -> Self {
        Self {
            tau_samples: tau_samples.max(1.0),
            power: 0.0,
            seen: 0,
        }
    }

    fn fold(&mut self, sum_sq: f64, n: usize) {
        if n == 0 {
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        let nf = n as f64;
        let mean = sum_sq / nf;
        let a = if self.seen == 0 {
            1.0
        } else {
            1.0 - (-nf / self.tau_samples).exp()
        };
        self.power += (mean - self.power) * a;
        self.seen = self.seen.saturating_add(n as u64);
    }

    pub fn push_real(&mut self, x: &[f32]) {
        let s: f64 = x.iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
        self.fold(s, x.len());
    }

    pub fn push_iq(&mut self, x: &[Complex<f32>]) {
        let s: f64 = x.iter().map(|v| f64::from(v.norm_sqr())).sum();
        self.fold(s, x.len());
    }

    /// Smoothed level, dBFS, rounded to 0.1 dB; `None` before any input.
    #[must_use]
    pub fn dbfs(&self) -> Option<f64> {
        (self.seen > 0).then(|| (10.0 * self.power.max(1e-20).log10() * 10.0).round() / 10.0)
    }

    /// Samples seen since construction.
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scale_sine_reads_minus_3_dbfs() {
        let mut m = LevelMeter::new(1000.0);
        assert_eq!(m.dbfs(), None);
        let x: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin()).collect();
        m.push_real(&x);
        let db = m.dbfs().unwrap();
        assert!((db + 3.0).abs() < 0.2, "{db}");
    }

    #[test]
    fn follows_a_drop_within_a_few_time_constants() {
        let mut m = LevelMeter::new(100.0);
        m.push_iq(&vec![Complex::new(1.0, 0.0); 1000]);
        assert!(m.dbfs().unwrap().abs() < 0.1);
        for _ in 0..10 {
            m.push_iq(&vec![Complex::new(0.01, 0.0); 100]);
        }
        assert!(m.dbfs().unwrap() < -35.0, "{:?}", m.dbfs());
    }
}
