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
            // Scrub non-finite samples to 0 first: a NaN would poison the
            // envelope EMA (`env += α·(mag−env)` stays NaN), and the
            // threshold test `mag > env·ratio` goes false forever, so the
            // blanker would silently stop blanking. Treat it as silence.
            if !x.is_finite() {
                *x = 0.0;
            }
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
            // otherwise. `mag` is the input magnitude so it's always
            // safe to update — the output zeroing below doesn't feed
            // back into env. Updating every sample (not only when hold
            // is clear) is load-bearing: a real silent-to-loud step
            // would otherwise trigger, then freeze env, then trigger
            // again as soon as hold expired, chopping the first ~100 ms
            // of every burst into 0.5 ms chunks of zeros.
            let alpha = if mag > self.env {
                self.alpha_attack
            } else {
                self.alpha_release
            };
            self.env += alpha * (mag - self.env);

            // Threshold check stays gated on hold so an ongoing blank
            // doesn't keep re-arming. The env-during-hold growth means
            // by the time hold expires the ratio has fallen below the
            // threshold for a sustained step; for a true impulse, env
            // barely moves (mag dropped back to noise → slow release)
            // so the next impulse is still caught.
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
        for (i, sample) in buf.iter().enumerate().take(2024).skip(2000) {
            assert!(
                sample.abs() < 1e-6,
                "sample {i} should be blanked, got {sample}"
            );
        }
    }

    #[test]
    fn sustained_step_does_not_chop_audio_after_first_blank() {
        // Regression: a silent gap followed by sustained signal must
        // recover within one hold cycle. With env-frozen-during-hold,
        // the post-hold sample would re-trigger because env never
        // caught up, chopping ~100 ms of audio into 0.5 ms zeros.
        let mut s = BlankerStage::new(20.0, 0.5, 48_000.0);
        // 1000 samples near-zero, then a 1.0-amplitude sine sustained
        // for the rest of the buffer.
        let mut buf: Vec<f32> = (0..8192)
            .map(|i| {
                if i < 1000 {
                    0.001
                } else {
                    let t = i as f32 / 48_000.0;
                    (2.0 * core::f32::consts::PI * 1_000.0 * t).cos()
                }
            })
            .collect();
        s.run(&mut buf);
        // Step onset triggers a 24-sample hold; after that, audio must
        // be passing through. Check that beyond a generous 200-sample
        // recovery window the output isn't dominated by zeros.
        let nonzero: usize = buf[1200..].iter().filter(|x| x.abs() > 0.01).count();
        let total = buf.len() - 1200;
        assert!(
            nonzero > total / 2,
            "blanker still chopping past recovery window: {nonzero}/{total} samples non-zero",
        );
    }

    #[test]
    fn non_finite_sample_does_not_latch() {
        let fs = 48_000.0_f32;
        let mut s = BlankerStage::new(20.0, 1.0, 48_000.0);
        let tone = |n: usize| -> Vec<f32> {
            (0..n)
                .map(|i| 0.3 * (core::f32::consts::TAU * 1_000.0 * i as f32 / fs).sin())
                .collect()
        };
        let mut warm = tone(2048);
        s.run(&mut warm);
        let mut glitch = tone(64);
        glitch[10] = f32::NAN;
        glitch[20] = f32::INFINITY;
        s.run(&mut glitch);
        assert!(glitch.iter().all(|v| v.is_finite()));
        // After the glitch the envelope must recover so the tone passes
        // through again — not a poisoned env that either NaNs the output
        // or stops detecting impulses.
        let mut after = tone(2048);
        s.run(&mut after);
        assert!(
            after.iter().all(|v| v.is_finite()),
            "blanker envelope latched non-finite after a NaN"
        );
        let energy: f32 = after[1024..].iter().map(|v| v * v).sum();
        assert!(energy > 0.0, "blanker stuck silent after the glitch");
    }
}
