//! De-emphasis stage — inverse of FM broadcast pre-emphasis.
//!
//! One-pole IIR lowpass at `f_c = 1/(2π·τ)`. 75 µs for US broadcasting,
//! 50 µs for Europe. Pure pass-through when disabled. Shared by
//! `AudioNrMono` and `AudioNrStereo` (stereo runs a separate instance
//! per channel — deemph is linear, no cross-channel coupling needed).

#[derive(Debug, Clone, Copy)]
pub struct DeempStage {
    alpha: f32,
    prev: f32,
    input_rate_hz: f64,
    tau_us: f32,
}

impl DeempStage {
    #[must_use]
    pub fn new(tau_us: f32, input_rate_hz: f64) -> Self {
        let mut s = Self {
            alpha: 1.0,
            prev: 0.0,
            input_rate_hz: 0.0,
            tau_us,
        };
        s.recompute(tau_us, input_rate_hz);
        s
    }

    pub fn recompute(&mut self, tau_us: f32, input_rate_hz: f64) {
        self.tau_us = tau_us;
        self.input_rate_hz = input_rate_hz;
        if tau_us <= 0.0 || input_rate_hz <= 0.0 {
            // Degenerate config → pass-through.
            self.alpha = 1.0;
            return;
        }
        let dt = 1.0 / input_rate_hz;
        let tau = f64::from(tau_us) * 1e-6;
        #[allow(clippy::cast_possible_truncation)]
        let alpha = (dt / (tau + dt)) as f32;
        self.alpha = alpha;
    }

    pub fn reset(&mut self) {
        self.prev = 0.0;
    }

    /// Apply de-emphasis in place. Safe to call with any buffer size.
    pub fn run(&mut self, buf: &mut [f32]) {
        let a = self.alpha;
        let one_minus_a = 1.0 - a;
        let mut y = self.prev;
        for x in buf {
            y = a * *x + one_minus_a * y;
            *x = y;
        }
        self.prev = y;
    }

    #[must_use]
    pub const fn alpha(&self) -> f32 {
        self.alpha
    }
}

#[cfg(test)]
mod tests {
    use super::DeempStage;
    use core::f32::consts::TAU;

    #[test]
    fn dc_passes_through() {
        let mut s = DeempStage::new(75.0, 48_000.0);
        let mut buf = vec![0.4_f32; 512];
        s.run(&mut buf);
        // IIR converges to input DC.
        for (i, y) in buf.iter().enumerate().skip(64) {
            assert!((y - 0.4).abs() < 1e-3, "buf[{i}] = {y}");
        }
    }

    #[test]
    fn attenuates_above_corner() {
        let fs = 48_000.0_f32;
        let tau_us = 75.0_f32;
        let f_c = 1.0 / (TAU * (tau_us * 1e-6));
        let f_test = 10.0 * f_c;
        let mut buf: Vec<f32> = (0..4096)
            .map(|i| (TAU * f_test * i as f32 / fs).cos())
            .collect();
        let input = buf.clone();
        let mut s = DeempStage::new(tau_us, f64::from(fs));
        s.run(&mut buf);
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let ratio = rms(&buf[512..]) / rms(&input[512..]);
        assert!(ratio < 0.15, "10× corner attenuation, got ratio {ratio}");
    }
}
