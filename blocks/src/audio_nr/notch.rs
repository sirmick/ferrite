//! Adaptive LMS auto-notch — suppresses narrowband tones (heterodynes,
//! CW carriers, power-line harmonics) inside an otherwise broadband
//! audio channel.
//!
//! ### Prediction-error structure
//!
//! The classic "ANC" trick: take `x[n]`, push a *delayed* copy into an
//! LMS filter whose desired output is `x[n]`, subtract the filter's
//! prediction. The delay decorrelates the broadband part (white noise
//! at sample `n` has no linear relationship with the white noise at
//! `n-D`), so the filter can't predict it — but a pure tone at
//! frequency `f` always has a deterministic relationship with its
//! delayed self (`cos(ωn) = linear combination of cos(ω(n-D))`), so
//! the filter converges onto the tone, and the subtraction notches it.
//!
//! No frequency knob, no estimator, no explicit detector — the LMS
//! gradient finds whatever narrowband content is in the signal.
//!
//! ### Why not a biquad
//!
//! A static biquad notch works if you know the tone frequency. For
//! heterodynes on HF SSB you usually don't, and they drift. An
//! adaptive LMS notch tracks.

use ferrite_liquid_dsp::EqLms;

pub struct NotchStage {
    inner: EqLms,
    delay_line: Vec<f32>,
    delay_write: usize,
}

impl NotchStage {
    pub fn new(taps: u32, mu: f32, delay_samples: u32) -> Result<Self, &'static str> {
        let mut inner = EqLms::new(taps)?;
        inner.set_bw(mu);
        let d = delay_samples.max(1) as usize;
        Ok(Self {
            inner,
            delay_line: vec![0.0; d],
            delay_write: 0,
        })
    }

    pub fn set_mu(&mut self, mu: f32) {
        self.inner.set_bw(mu);
    }

    pub fn reset(&mut self) {
        self.inner.reset();
        for v in &mut self.delay_line {
            *v = 0.0;
        }
        self.delay_write = 0;
    }

    /// Run the notch in place. Per-sample: buffer the current input,
    /// feed the *oldest* delayed sample into the LMS predictor, ask
    /// for its prediction of `x[n]`, subtract to get the notched
    /// residual, step the LMS tap-update with the error.
    pub fn run(&mut self, buf: &mut [f32]) {
        let d = self.delay_line.len();
        for x in buf {
            let current = *x;
            // Read out the oldest slot, overwrite with the newest.
            let oldest = self.delay_line[self.delay_write];
            self.delay_line[self.delay_write] = current;
            self.delay_write = (self.delay_write + 1) % d;

            self.inner.push(oldest);
            let pred = self.inner.execute();
            let residual = current - pred;
            // LMS step: desired = current sample, actual = filter out.
            self.inner.step(current, pred);
            *x = residual;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NotchStage;

    #[test]
    fn suppresses_tone_in_noise() {
        use core::f32::consts::TAU;
        let fs = 48_000.0_f32;
        let f = 3_000.0_f32;
        let n = 40_000usize;
        let mut rng: u32 = 0xDEAD_BEEF;
        let mut rand = || -> f32 {
            rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((rng >> 8) & 0x00FF_FFFF) as f32 / 8_388_608.0 - 1.0
        };

        let mut buf: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                (TAU * f * t).cos() * 0.5 + rand() * 0.1
            })
            .collect();

        let mut stage = NotchStage::new(64, 0.01, 1).expect("create");
        stage.run(&mut buf);

        let goertzel = |xs: &[f32]| -> f32 {
            let k = TAU * f / fs;
            let c = 2.0 * k.cos();
            let (mut s1, mut s2) = (0.0_f32, 0.0_f32);
            for &x in xs {
                let s0 = x + c * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            (s1 * s1 + s2 * s2 - c * s1 * s2).sqrt() / xs.len() as f32
        };
        let q = n / 4;
        let start = goertzel(&buf[0..q]);
        let end = goertzel(&buf[3 * q..]);
        let db = 20.0 * (start / end.max(1e-9)).log10();
        assert!(db > 15.0, "expected >15 dB suppression, got {db:.1}");
    }
}
