//! Audio AGC — final stage of the NR chain. Peak-detects the input
//! envelope and applies a smoothly-adapted gain so the output peaks
//! sit near a target level regardless of input amplitude.
//!
//! ### Why this lives here
//!
//! AM station strengths span 30–40 dB between a local 50 kW blowtorch
//! and a 0.5 µV DX catch. A static `AmDemod.audio_gain` can be tuned
//! for one regime but not both — strong stations clip the AudioWorklet
//! at ±1.0, weak ones are inaudible. The AGC adapts continuously so
//! the user doesn't reach for the volume slider per-station.
//!
//! ### Algorithm
//!
//! Classic broadcast peak-AGC: **instant attack, exponential release**
//! on the envelope, plus a brick-wall ±1.0 clamp as a safety net.
//!
//! ```text
//!   for each sample x:
//!     mag = |x|
//!     env ← max(mag, env + α_release · (mag − env))
//!     gain = min(target / max(env, ε), max_gain)
//!     y  = x · gain
//!     x  ← clamp(y, −1, +1)
//! ```
//!
//! - **Instant attack** (`env = max(env, mag)`) is the only way to
//!   actually track a sine peak — an EMA can only follow the
//!   smoothed mean, which leaves audible 6 dB of head-room error on
//!   continuous tones.
//! - **Exponential release** with a slow τ (≥ a few hundred ms) keeps
//!   the AGC from pumping on speech pauses.
//! - **Brick-wall clamp** at ±1.0 catches the one sample where mag
//!   exceeds env (the instant before the peak update fires) so the
//!   downstream AudioWorklet never sees an above-1.0 sample. Rare in
//!   practice but cheap insurance.
//!
//! ### Defaults for AM broadcast
//!
//! - target  = −6 dBFS (= 0.501)   peak headroom of 6 dB
//! - release = 500 ms              slow enough not to pump on speech
//! - max boost = +20 dB            covers DX without amplifying static

#[derive(Debug, Clone, Copy)]
pub struct AgcStage {
    /// Target peak amplitude (linear, derived from `target_dbfs`).
    target: f32,
    /// EMA coefficient for the exponential release (slow).
    release_alpha: f32,
    /// Linear gain ceiling, derived from `max_gain_db`.
    max_gain: f32,
    /// Tracked peak envelope.
    envelope: f32,
    /// Seeded-envelope flag — set to the first non-zero `|x|` so a
    /// cold start doesn't divide by near-zero and pin the first
    /// samples at +max_gain.
    primed: bool,
}

impl AgcStage {
    #[must_use]
    pub fn new(target_dbfs: f32, release_ms: f32, max_gain_db: f32, sample_rate_hz: f64) -> Self {
        let mut s = Self {
            target: 0.5,
            release_alpha: 1.0,
            max_gain: 10.0,
            envelope: 0.0,
            primed: false,
        };
        s.recompute(target_dbfs, release_ms, max_gain_db, sample_rate_hz);
        s
    }

    pub fn recompute(
        &mut self,
        target_dbfs: f32,
        release_ms: f32,
        max_gain_db: f32,
        sample_rate_hz: f64,
    ) {
        // Clamp to a sane range so misconfigured presets can't blow
        // up the gain stage. Target above 0 dBFS would invite
        // clipping; below −60 dBFS the signal would be inaudible.
        self.target = 10.0_f32.powf(target_dbfs / 20.0).clamp(1e-3, 1.0);
        // 1.0× floor — negative max_gain_db would require attenuating
        // everything, which is the limiter's job not ours.
        self.max_gain = 10.0_f32.powf(max_gain_db / 20.0).max(1.0);
        #[allow(clippy::cast_possible_truncation)]
        let alpha_from_tau_ms = |tau_ms: f32| -> f32 {
            if sample_rate_hz <= 0.0 || tau_ms <= 0.0 {
                return 1.0;
            }
            let dt = 1.0 / sample_rate_hz as f32;
            let tau_s = tau_ms * 1e-3;
            dt / (tau_s + dt)
        };
        self.release_alpha = alpha_from_tau_ms(release_ms);
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.primed = false;
    }

    pub fn run(&mut self, buf: &mut [f32]) {
        // ε floor on env: prevents 0-division and keeps gain finite.
        // With `max_gain` capping the boost, the exact ε value barely
        // matters as long as it's >0.
        const EPS: f32 = 1e-6;
        for x in buf {
            let mag = x.abs();
            // Seed from first non-zero sample. Without this, env
            // starts at 0, gain = target/ε = huge, clamped to
            // max_gain, and weak silence bursts get pinned at
            // +max_gain before the tracker can settle.
            if !self.primed && mag > EPS {
                self.envelope = mag;
                self.primed = true;
            }

            // Peak detector with instant attack + exponential release.
            self.envelope = if mag > self.envelope {
                mag
            } else {
                self.envelope + self.release_alpha * (mag - self.envelope)
            };
            let env = self.envelope.max(EPS);
            let gain = (self.target / env).min(self.max_gain);
            let y = *x * gain;
            // Brick-wall safety net. The peak detector keeps env at
            // the running max, so gain×mag = target — except for the
            // one sample where mag steps above env before the update
            // fires. Clamp that sample so the AudioWorklet never
            // sees > ±1.0.
            *x = y.clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AgcStage;

    fn rms(buf: &[f32]) -> f32 {
        (buf.iter().map(|v| v * v).sum::<f32>() / buf.len() as f32).sqrt()
    }

    #[test]
    fn loud_sine_converges_to_target_peak() {
        // 1 kHz sine at amplitude 0.9. Peak-detector should track
        // 0.9 exactly; gain = target/0.9 = 0.557; output peak = target.
        // For target = -6 dBFS (= 0.501), RMS of the output sine is
        // 0.501/√2 ≈ 0.354.
        let fs = 48_000.0;
        let mut s = AgcStage::new(-6.0, 500.0, 20.0, fs);
        let mut buf: Vec<f32> = (0..fs as usize) // 1 second
            .map(|i| (core::f32::consts::TAU * 1_000.0 * i as f32 / fs as f32).sin() * 0.9)
            .collect();
        s.run(&mut buf);
        let tail = &buf[fs as usize * 3 / 4..];
        let r = rms(tail);
        let target = 10.0_f32.powf(-6.0 / 20.0);
        let expected = target / core::f32::consts::SQRT_2;
        assert!(
            (r - expected).abs() < 0.02,
            "loud sine RMS should converge near {expected:.3}, got {r:.3}",
        );
    }

    #[test]
    fn quiet_sine_is_boosted_up_to_max_gain() {
        // Quiet sine at amplitude 0.01 with max_gain = +20 dB (10×).
        // Peak gain hits the ceiling, output peak ≈ 0.1, RMS ≈ 0.071.
        let fs = 48_000.0;
        let mut s = AgcStage::new(-6.0, 500.0, 20.0, fs);
        let mut buf: Vec<f32> = (0..fs as usize)
            .map(|i| (core::f32::consts::TAU * 1_000.0 * i as f32 / fs as f32).sin() * 0.01)
            .collect();
        s.run(&mut buf);
        let tail = &buf[fs as usize * 3 / 4..];
        let r = rms(tail);
        let expected = 0.1_f32 / core::f32::consts::SQRT_2;
        assert!(
            (r - expected).abs() < 0.02,
            "quiet sine should be boosted to RMS ≈ {expected:.3}, got {r:.3}",
        );
    }

    #[test]
    fn output_never_exceeds_unity() {
        // Pathological input: steady tone plus periodic transients
        // larger than the AGC envelope. The instant-attack peak
        // detector + ±1.0 clamp must ensure nothing escapes
        // upstream — the AudioWorklet hard-clips at ±1.0 with harsh
        // artifacts.
        let fs = 48_000.0;
        let mut s = AgcStage::new(-6.0, 500.0, 20.0, fs);
        let mut buf: Vec<f32> = (0..fs as usize)
            .map(|i| {
                let t = i as f32 / fs as f32;
                let base = (core::f32::consts::TAU * 440.0 * t).sin() * 0.5;
                if i % 4_800 == 0 {
                    base + 1.5 // big transient — would clip without clamp
                } else {
                    base
                }
            })
            .collect();
        s.run(&mut buf);
        let peak = buf.iter().map(|v| v.abs()).fold(0.0_f32, f32::max);
        assert!(peak <= 1.0, "AGC output peaked at {peak:.3}");
    }

    #[test]
    fn step_up_converges_without_pumping_through_silence() {
        // Quiet sustained tone for 0.5 s, then steps to loud. After
        // a brief attack-time transient, output peak should track
        // target. (A static gain would let the loud half clip.)
        let fs = 48_000.0;
        let mut s = AgcStage::new(-6.0, 500.0, 20.0, fs);
        let n = fs as usize;
        let mut buf: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / fs as f32;
                let amp = if i < n / 2 { 0.1 } else { 0.9 };
                (core::f32::consts::TAU * 1_000.0 * t).sin() * amp
            })
            .collect();
        s.run(&mut buf);
        // Skip the post-step window so the peak detector is converged.
        let post_step = &buf[n / 2 + (fs as usize / 5)..];
        let r = rms(post_step);
        // With instant attack, the peak is captured immediately, so
        // RMS should be at the target — comfortably below the
        // unprotected 0.9/√2 ≈ 0.636.
        assert!(
            r < 0.45,
            "post-step RMS {r:.3} suggests the AGC didn't normalize the loud half",
        );
    }

    #[test]
    fn primed_state_passes_target_amplitude_through_unchanged() {
        // A signal already at the target peak amplitude should pass
        // through with gain ≈ 1.0 — proves the maths is internally
        // consistent (target/env where env=target gives unity gain).
        let fs = 48_000.0;
        let target = 10.0_f32.powf(-6.0 / 20.0);
        let mut s = AgcStage::new(-6.0, 500.0, 20.0, fs);
        let mut buf = [target];
        s.run(&mut buf);
        assert!(
            (buf[0] - target).abs() < 1e-5,
            "input at target amplitude should pass through, got {} vs target {target}",
            buf[0],
        );
    }
}
