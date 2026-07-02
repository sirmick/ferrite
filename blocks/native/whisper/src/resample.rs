//! Linear resampler — source audio rate → whisper's 16 kHz mono.
//!
//! A direct port of `web/src/lib/transcribe/resample.ts`. Whisper wants
//! 16 kHz f32 mono; the demod chain hands us whatever the preset
//! negotiated (12 k SSB, 48 k FM, …). A streaming linear resampler is
//! plenty for speech intelligibility — whisper is robust to the mild
//! aliasing, and accuracy here is dominated by the model, not the
//! interpolation kernel. Stateful so it can be fed arbitrary
//! ring-drained chunks without clicks at the seams.

/// Whisper's required sample rate.
pub const WHISPER_RATE: f64 = 16_000.0;

/// Streaming linear resampler from `src_rate` to 16 kHz. Carries its
/// fractional read position and left-neighbour sample across `feed`
/// calls so chunk boundaries don't glitch.
#[derive(Debug, Clone)]
pub struct LinearResampler {
    src_rate: f64,
    /// Fractional read position into the *input* stream, carried across
    /// `feed` calls so chunk boundaries don't glitch.
    pos: f64,
    /// Last sample of the previous chunk — the left neighbour for the
    /// first interpolation of the next chunk.
    prev: f32,
    primed: bool,
}

impl LinearResampler {
    #[must_use]
    pub fn new(src_rate: f64) -> Self {
        Self {
            src_rate,
            pos: 0.0,
            prev: 0.0,
            primed: false,
        }
    }

    /// True when the source already is 16 kHz (passthrough).
    #[must_use]
    pub fn is_identity(&self) -> bool {
        (self.src_rate - WHISPER_RATE).abs() < f64::EPSILON
    }

    /// Resample one chunk into a freshly-allocated 16 kHz buffer.
    #[must_use]
    pub fn feed(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        if self.is_identity() {
            return input.to_vec();
        }

        let step = self.src_rate / WHISPER_RATE; // input samples per output sample
        let mut out = Vec::new();
        if !self.primed {
            self.prev = input[0];
            self.primed = true;
        }
        // `pos` is an absolute index into a virtual stream whose sample
        // -1 is `self.prev` and 0..n-1 is `input`. Emit until we'd need
        // a sample past the end of this chunk.
        let n = input.len();
        while self.pos < (n - 1) as f64 {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let idx = i as isize;
            let a = if idx < 0 {
                self.prev
            } else {
                input[idx as usize]
            };
            let b = input[(idx + 1) as usize];
            out.push(a + (b - a) * frac);
            self.pos += step;
        }
        // Carry the fractional remainder into the next chunk's
        // coordinate space and stash the trailing sample as the next
        // left-neighbour.
        self.pos -= n as f64;
        self.prev = input[n - 1];
        out
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.prev = 0.0;
        self.primed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_passes_through_when_already_16k() {
        let mut r = LinearResampler::new(16_000.0);
        assert!(r.is_identity());
        let out = r.feed(&[0.1, 0.2, 0.3]);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn empty_input_is_empty() {
        let mut r = LinearResampler::new(48_000.0);
        assert!(r.feed(&[]).is_empty());
    }

    #[test]
    fn downsample_48k_halves_roughly_threefold() {
        // 48k → 16k is a 3:1 decimation; a long ramp should yield ~1/3
        // the samples, monotonic in [first, last].
        let mut r = LinearResampler::new(48_000.0);
        let input: Vec<f32> = (0..300).map(|i| i as f32).collect();
        let out = r.feed(&input);
        // ~100 output samples (300/3), allow a couple either side.
        assert!((out.len() as i64 - 100).abs() <= 2, "got {}", out.len());
        // Monotonic non-decreasing for a ramp.
        for w in out.windows(2) {
            assert!(w[1] >= w[0]);
        }
    }

    #[test]
    fn streaming_matches_one_shot() {
        // Feeding in two chunks must equal feeding the whole buffer at
        // once — the cross-chunk state is the whole point.
        let input: Vec<f32> = (0..240).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut whole = LinearResampler::new(48_000.0);
        let a = whole.feed(&input);

        let mut split = LinearResampler::new(48_000.0);
        let mut b = split.feed(&input[..120]);
        b.extend(split.feed(&input[120..]));

        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert!((x - y).abs() < 1e-6, "{x} vs {y}");
        }
    }

    #[test]
    fn reset_clears_position() {
        let mut r = LinearResampler::new(48_000.0);
        let _ = r.feed(&[1.0, 2.0, 3.0, 4.0]);
        r.reset();
        // After reset, identical input yields identical output to a
        // fresh resampler.
        let mut fresh = LinearResampler::new(48_000.0);
        assert_eq!(
            r.feed(&[5.0, 6.0, 7.0, 8.0]),
            fresh.feed(&[5.0, 6.0, 7.0, 8.0])
        );
    }
}
