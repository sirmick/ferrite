//! Ferrite analysis toolkit.
//!
//! Measurements over a recorded IQ snapshot (interleaved little-endian
//! `cf32`, as the daemon's live tees write them). Everything returns
//! structured data — tables and arrays an agent can read and a UI can
//! draw — plus a compact text rendering ([`text`]) for terminals and for
//! agents that can't see the UI.
//!
//! - [`spectrum`]: long-FFT peaks over a range, Hz-accurate, with keying.
//! - [`slice`]: one frequency range mixed to baseband and decimated —
//!   the narrow working signal the other tools run on.
//! - [`spectrogram`]: time × frequency of a slice.
//! - [`envelope`]: on/off power timeline at one frequency.
//!
//! Pure Rust (rustfft); no I/O beyond the `Read` you hand in.

pub mod envelope;
pub mod iq;
pub mod slice;
pub mod spectrogram;
pub mod spectrum;
pub mod text;

pub use envelope::{envelope, Envelope, EnvelopeParams, Segment};
pub use iq::IqMeta;
pub use slice::{slice, Slice};
pub use spectrogram::{spectrogram, Spectrogram, SpectrogramParams};
pub use spectrum::{analyze as spectrum, Peak, SpectrumParams, SpectrumResult};

/// Power → dB with a floor so silence doesn't produce −∞.
#[must_use]
pub fn db(p: f32) -> f32 {
    10.0 * p.max(1e-30).log10()
}
