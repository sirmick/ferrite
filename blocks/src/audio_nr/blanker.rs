//! Impulse noise blanker — a time-domain stage that zeros out samples
//! whose instantaneous magnitude exceeds a slow-envelope reference by
//! more than the configured threshold. Cheap, order-of-magnitude-lower-
//! complexity than spectral NR for the class of noise it targets:
//!
//! - atmospheric static (QRN)
//! - power-line / SMPS clicks
//! - ignition noise
//! - abrupt ESD pops
//!
//! None of which spectral subtraction catches well — they're broadband
//! *transients*, not stationary hiss.
//!
//! ### Design
//!
//! The envelope follower is a two-time-constant tracker — fast attack,
//! slow release — running on `|x|`. Threshold is expressed in dB over
//! the envelope. When `|x[n]| / env[n]` crosses the threshold, a hold
//! counter is set to `hold_ms · fs / 1000` and subsequent output samples
//! are zeroed until the counter expires. A smooth fade rather than hard
//! zero avoids the blanker creating its own transients.

#[derive(Debug, Clone, Copy)]
pub struct BlankerStage {
    /// Envelope attack coefficient (fast). Roughly "how quickly the
    /// reference rises to a new signal level".
    alpha_attack: f32,
    /// Envelope release coefficient (slow). Holds the reference
    /// against the impulse so thresholding still fires.
    alpha_release: f32,
    /// Linear threshold ratio (not dB). `|x| > ratio · env` triggers.
    threshold_ratio: f32,
    /// Hold samples after a trigger.
    hold_samples: u32,

    env: f32,
    hold: u32,
    input_rate_hz: f64,
    /// Seeded-envelope flag. Before the first non-zero input sample,
    /// `env` is zero and *any* signal would trigger the threshold
    /// comparison — pathological on a cold start. We seed `env` from
    /// the first magnitude seen and only then engage the detector.
    primed: bool,
}

impl BlankerStage {
    #[must_use]
    pub fn new(threshold_db: f32, hold_ms: f32, input_rate_hz: f64) -> Self {
        let mut s = Self {
            alpha_attack: 0.1,
            alpha_release: 0.0001,
            threshold_ratio: 1.0,
            hold_samples: 0,
            env: 0.0,
            hold: 0,
            input_rate_hz: 0.0,
            primed: false,
        };
        s.recompute(threshold_db, hold_ms, input_rate_hz);
        s
    }

    pub fn recompute(&mut self, threshold_db: f32, hold_ms: f32, input_rate_hz: f64) {
        self.input_rate_hz = input_rate_hz;
        self.threshold_ratio = 10.0_f32.powf(threshold_db / 20.0);
        // Attack ~1 ms, release ~500 ms — standard broadcast NB values.
        // Kept fixed (not user-facing) to keep the surface small; the
        // threshold knob is the useful tuning lever.
        #[allow(clippy::cast_possible_truncation)]
        let alpha_from_tau_ms = |tau_ms: f32| -> f32 {
            if input_rate_hz <= 0.0 || tau_ms <= 0.0 {
                return 1.0;
            }
            let dt = 1.0 / input_rate_hz as f32;
            let tau_s = tau_ms * 1e-3;
            dt / (tau_s + dt)
        };
        self.alpha_attack = alpha_from_tau_ms(1.0);
        self.alpha_release = alpha_from_tau_ms(500.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.hold_samples = ((hold_ms as f64) * input_rate_hz / 1000.0).max(1.0) as u32;
        }
    }

    pub fn reset(&mut self) {
        self.env = 0.0;
        self.hold = 0;
        self.primed = false;
    }

    /// Zero-hole blanker. When a sample exceeds `threshold · env`, emit
    /// zero for the next `hold_samples` outputs. The envelope itself
    /// keeps updating on non-blanked samples only, so a long string of
    /// spikes doesn't drag the reference up and silence the blanker.
    pub fn run(&mut self, buf: &mut [f32]) {
        for x in buf {
            let mag = x.abs();
            // Seed the envelope from the first real sample so the
            // threshold detector has a sensible reference immediately.
            // Without this, a cold env=0 causes every sample to look
            // like an impulse and the blanker locks into a stuck-on
            // loop that silences the whole stream.
            if !self.primed && mag > 0.0 {
                self.env = mag;
                self.primed = true;
            }

            // Envelope: fast attack when mag > env, slow release
            // otherwise. Gate the update on the hold state so an
            // ongoing blanking event doesn't train the envelope with
            // zeros and lose the reference.
            if self.hold == 0 {
                let alpha = if mag > self.env {
                    self.alpha_attack
                } else {
                    self.alpha_release
                };
                self.env += alpha * (mag - self.env);
            }

            if self.hold == 0 && mag > self.env * self.threshold_ratio {
                self.hold = self.hold_samples;
            }

            if self.hold > 0 {
                *x = 0.0;
                self.hold -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BlankerStage;

    #[test]
    fn quiet_signal_passes_through() {
        let mut s = BlankerStage::new(20.0, 1.0, 48_000.0);
        // Steady-state sine at moderate amplitude — no spikes,
        // nothing to blank. The blanker is a high-pass threshold on
        // instantaneous magnitude vs envelope; a constant sine ratio
        // is ~1 (well below the 10× threshold).
        let mut buf: Vec<f32> = (0..4096)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                (2.0 * core::f32::consts::PI * 1_000.0 * t).cos() * 0.3
            })
            .collect();
        let input = buf.clone();
        s.run(&mut buf);
        // Skip envelope wind-up (first ~1000 samples).
        for i in 1024..buf.len() {
            assert!(
                (buf[i] - input[i]).abs() < 1e-6,
                "sample {i}: in={} out={}",
                input[i],
                buf[i]
            );
        }
    }

    #[test]
    fn impulse_is_blanked() {
        let mut s = BlankerStage::new(20.0, 0.5, 48_000.0);
        // 2000 samples of low-level noise then one big spike.
        let mut buf: Vec<f32> = (0..4096)
            .map(|i| if i == 2000 { 10.0 } else { 0.01 })
            .collect();
        s.run(&mut buf);
        // Spike and next ~24 samples (0.5 ms @ 48 k) should be zero.
        for i in 2000..2024 {
            assert!(
                buf[i].abs() < 1e-6,
                "sample {i} should be blanked, got {}",
                buf[i]
            );
        }
    }
}
