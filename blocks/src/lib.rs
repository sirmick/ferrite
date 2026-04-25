//! Ferrite DSP blocks — dual-compile crate (native + WebAssembly).
//!
//! The [`block`] module defines the trait and static descriptors every
//! block implements; concrete blocks (`SineSource`, `FFT`, …) land in
//! sibling modules as Phase B progresses.
//!
//! Each block impl is annotated with [`ferrite_block`] so it
//! self-registers into [`registry`] at link time.

// Lets the `#[ferrite_block]` macro emit `::ferrite_blocks::…` paths
// that resolve both from downstream crates and from inside this crate.
extern crate self as ferrite_blocks;

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

pub mod am_demod;
pub mod am_modulator;
pub mod audio_nr;
pub mod audio_sink;
pub mod block;
pub mod channelizer;
pub mod decimator;
pub mod dtmf_audio_source;
pub mod dtmf_decoder;
pub mod events_sink;
pub mod fft;
pub mod file_audio_sink;
pub mod file_sink;
pub mod file_source;
pub mod fm_demod;
pub mod frame;
pub mod log_mag_u8;
#[cfg(feature = "multimon")]
pub mod pager;
pub mod rds_demod;
pub mod real_resamp;
pub mod registry;
pub mod render;
pub mod rssi_probe;
pub mod sine;
#[cfg(feature = "soapysdr")]
pub mod soapy_retry;
#[cfg(feature = "soapysdr")]
pub mod soapy_source;
pub mod spsc_ring;
pub mod squelch;
pub mod ssb_demod;
pub mod stereo_decoder;
pub mod tee_iq_f32;
pub mod tee_real_f32;
pub mod ws_bridge;

pub use am_demod::{AmDemod, AmDemodParams};
pub use am_modulator::{AmModulator, AmModulatorParams};
pub use audio_nr::{AudioNrMono, AudioNrParams, AudioNrStereo, SpectralMethod};
pub use audio_sink::{AudioSink, AudioSinkParams};
pub use block::{
    AsAny, Block, BlockFactory, BlockIo, BlockSpec, InBuf, InitCtx, InputPort, OutBuf, OutputPort,
    ParamKind, ParamSpec, Placement, PortMeta, PortSpec, PortType, ReconfigureScope, Work,
    MAX_PORTS,
};
pub use channelizer::{Channelizer, ChannelizerParams};
pub use decimator::{Decimator, DecimatorParams};
pub use dtmf_audio_source::{DtmfAudioSource, DtmfAudioSourceParams};
pub use dtmf_decoder::{DtmfDecoder, DtmfDecoderParams};
pub use events_sink::{EventsSink, EventsSinkParams};
pub use fft::{FftBlock, FftBlockParams, FftWindow};
pub use file_audio_sink::{FileAudioSink, FileAudioSinkParams};
pub use file_sink::{FileIqSink, FileIqSinkParams, IqSinkFormat, WriteSeek};
pub use file_source::{FileIqSource, FileIqSourceParams, IqFileFormat, ReadSeek};
pub use fm_demod::{FmDemod, FmDemodParams};
pub use frame::{Frame, CONTROL_STREAM, FFT_STREAM, VFO_STREAM_BASE};
pub use log_mag_u8::{LogMagU8, LogMagU8Params};
#[cfg(feature = "multimon")]
pub use pager::{PagerDemod, PagerDemodParams};
pub use rds_demod::{RdsDemod, RdsDemodParams};
pub use real_resamp::{RealF32Resamp, RealF32ResampParams};
pub use render::{collapse_row_to_columns, compute_spectrum_stats, update_max_hold, SpectrumStats};
pub use rssi_probe::{RssiProbe, RssiProbeParams};
pub use sine::{SineSource, SineSourceParams};
#[cfg(feature = "soapysdr")]
pub use soapy_source::{SoapyReadback, SoapySource, SoapySourceParams};
pub use spsc_ring::{AudioRing, IqRing, SpscRing};
pub use squelch::{Squelch, SquelchParams};
pub use ssb_demod::{Sideband, SsbDemod, SsbDemodParams};
pub use stereo_decoder::{StereoDecoder, StereoDecoderParams};
pub use tee_iq_f32::TeeIqF32;
pub use tee_real_f32::TeeRealF32;
pub use ws_bridge::{
    BridgeSink, WsBridgeFftU8Params, WsBridgeParams, WsBridgeRx, WsBridgeRxParams, WsBridgeTx,
    WsBridgeTxEvents, WsBridgeTxFftU8,
};

/// Marks an `impl Block for T` so `T` is added to [`registry`].
///
/// Re-exported from `ferrite-blocks-macros` for ergonomic use; the
/// macro's generated code references `::ferrite_blocks::…` paths.
pub use ferrite_blocks_macros::ferrite_block;

/// Re-exported so the generated code from [`ferrite_block`] can refer to
/// [`inventory`] without requiring callers to add a direct dep.
#[doc(hidden)]
pub use inventory;

#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Trivial placeholder — proves the Rust→WASM→Worker path. Replaced by
/// real DSP blocks as Phase B progresses.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[must_use]
pub fn demo_add(a: f32, b: f32) -> f32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::{demo_add, registry, version};
    use std::collections::HashSet;

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
    }

    #[test]
    fn demo_add_sums() {
        assert!((demo_add(1.5, 2.25) - 3.75).abs() < f32::EPSILON);
    }

    #[test]
    fn registry_contains_every_shipped_block() {
        let names: HashSet<&'static str> =
            registry::entries().map(|e| e.spec().type_name).collect();
        for expected in [
            "SineSource",
            "FFT",
            "FileAudioSink",
            "FileIqSink",
            "FileIqSource",
            "LogMagU8",
            "Decimator",
            "RealF32Resamp",
            "Channelizer",
            "FmDemod",
            "SsbDemod",
            "Squelch",
            "RssiProbe",
            "RdsDemod",
            "StereoDecoder",
            "AmDemod",
            "AmModulator",
            "AudioNrMono",
            "AudioNrStereo",
            "DtmfAudioSource",
            "DtmfDecoder",
            "EventsSink",
            "TeeIqF32",
            "TeeRealF32",
            "WsBridgeTx",
            "WsBridgeTxFftU8",
            "WsBridgeTxEvents",
            "WsBridgeRx",
            "AudioSink",
        ] {
            assert!(
                names.contains(expected),
                "{expected} missing from registry (found: {names:?})",
            );
        }
        #[cfg(feature = "soapysdr")]
        assert!(
            names.contains("SoapySource"),
            "SoapySource missing from registry under `soapysdr` feature (found: {names:?})",
        );
        #[cfg(feature = "multimon")]
        assert!(
            names.contains("PagerDemod"),
            "PagerDemod missing from registry under `multimon` feature (found: {names:?})",
        );
    }

    #[test]
    fn registry_find_returns_matching_entry() {
        let entry = registry::find("SineSource").expect("SineSource must be registered");
        assert_eq!(entry.spec().type_name, "SineSource");
    }

    #[test]
    fn registry_find_rejects_unknown_names() {
        assert!(registry::find("NoSuchBlock").is_none());
    }
}
