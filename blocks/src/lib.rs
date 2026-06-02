//! Ferrite DSP blocks — dual-compile crate (native + WebAssembly).
//!
//! The [`block`] module defines the trait and static descriptors every
//! block implements; concrete blocks (`SineSource`, `FFT`, …) live in
//! sibling modules.
//!
//! Each block impl is annotated with [`ferrite_block`] so it
//! self-registers into [`registry`] at link time.

// Lets the `#[ferrite_block]` macro emit `::ferrite_blocks::…` paths
// that resolve both from downstream crates and from inside this crate.
extern crate self as ferrite_blocks;

#[cfg(feature = "adsb")]
pub mod adsb;
pub mod aircraft_spot;
#[cfg(feature = "ais")]
pub mod ais;
pub mod am_demod;
pub mod am_modulator;
pub mod aprs;
pub mod audio_nr;
pub mod audio_shaper;
pub mod audio_sink;
pub mod auto_tune;
pub mod block;
pub mod channelizer;
pub mod dc_block;
pub mod decimator;
pub mod digital_spot;
pub mod dtmf_audio_source;
pub mod dtmf_decoder;
#[cfg(feature = "multimon")]
pub mod eas;
pub mod event_store;
pub mod events_sink;
pub mod fft;
pub mod file_audio_sink;
pub mod file_audio_source;
pub mod file_sink;
pub mod file_source;
#[cfg(feature = "fldigi")]
pub mod fldigi_modes;
pub mod fm_demod;
pub mod fm_modulator;
pub mod frame;
#[cfg(feature = "ft8")]
pub mod ft8;
pub mod iq_upmix;
pub mod log_mag_u8;
pub mod modulated_file_source;
#[cfg(feature = "multimon")]
pub mod morse;
pub mod morse_audio_source;
#[cfg(feature = "multimon")]
pub mod packet;
#[cfg(feature = "multimon")]
pub mod pager;
pub mod rds_demod;
pub mod real_resamp;
pub mod record;
pub mod registry;
pub mod render;
pub mod rssi_probe;
#[cfg(feature = "rtl_433")]
pub mod rtl_433;
pub mod signal_list;
pub mod sine;
#[cfg(feature = "soapysdr")]
pub mod soapy_retry;
#[cfg(feature = "soapysdr")]
pub mod soapy_source;
pub mod spsc_ring;
pub mod squelch;
pub mod ssb_demod;
pub mod ssb_modulator;
pub mod stereo_decoder;
pub mod tee_iq_f32;
pub mod tee_real_f32;
#[cfg(test)]
pub(crate) mod test_support;
/// Browser worker's handle to the shared Rust transcription core
/// (wasm-bindgen). Only built for the wasm surface.
#[cfg(feature = "wasm")]
pub mod transcribe_wasm;
pub mod voice_transcribe;
pub mod wav;
pub mod ws_bridge;
#[cfg(feature = "wspr")]
pub mod wspr;

#[cfg(feature = "adsb")]
pub use adsb::{AdsbDemod, AdsbDemodParams};
#[cfg(feature = "ais")]
pub use ais::{AisDemod, AisDemodParams};
pub use am_demod::{AmDemod, AmDemodParams};
pub use am_modulator::{AmModulator, AmModulatorParams};
pub use audio_nr::{AudioNrMono, AudioNrParams, AudioNrStereo, SpectralMethod};
pub use audio_shaper::{AudioShaper, AudioShaperParams};
pub use audio_sink::{AudioSink, AudioSinkParams};
pub use auto_tune::{afc_new_shift, estimate_center_hz, AutoTune, AutoTuneParams};
pub use block::{
    AsAny, Block, BlockFactory, BlockIo, BlockSpec, InBuf, InitCtx, InputPort, OutBuf, OutputPort,
    ParamKind, ParamSpec, Placement, PortMeta, PortSpec, PortType, ReconfigureScope, Work,
    MAX_PORTS,
};
pub use channelizer::{Channelizer, ChannelizerParams};
pub use dc_block::{DcBlock, DcBlockParams};
pub use decimator::{Decimator, DecimatorParams};
pub use dtmf_audio_source::{DtmfAudioSource, DtmfAudioSourceParams};
pub use dtmf_decoder::{DtmfDecoder, DtmfDecoderParams};
#[cfg(feature = "multimon")]
pub use eas::{EasDemod, EasDemodParams};
pub use event_store::{DecoderSink, EventStore, EventStoreParams};
pub use events_sink::{EventsSink, EventsSinkParams};
pub use fft::{FftBlock, FftBlockParams, FftWindow};
pub use file_audio_sink::{FileAudioSink, FileAudioSinkParams};
pub use file_audio_source::{AudioFileFormat, FileAudioSource, FileAudioSourceParams};
pub use file_sink::{FileIqSink, FileIqSinkParams, IqSinkFormat, WriteSeek};
pub use file_source::{FileIqSource, FileIqSourceParams, IqFileFormat, ReadSeek};
#[cfg(feature = "fldigi")]
pub use fldigi_modes::{
    ContestiaDemod, ContestiaDemodParams, CwDemod, CwDemodParams, DominoexDemod,
    DominoexDemodParams, FldigiAuto, FldigiAutoParams, Mt63Demod, Mt63DemodParams, NavtexDemod,
    NavtexDemodParams, OliviaDemod, OliviaDemodParams, Psk31Demod, Psk31DemodParams, RttyDemod,
    RttyDemodParams, ThrobDemod, ThrobDemodParams,
};
pub use fm_demod::{FmDemod, FmDemodParams};
pub use fm_modulator::{FmModulator, FmModulatorParams};
pub use frame::{Frame, CONTROL_STREAM, FFT_STREAM, VFO_STREAM_BASE};
#[cfg(feature = "ft8")]
pub use ft8::{Ft8Demod, Ft8DemodParams, Ft8Mode};
pub use iq_upmix::{IqUpmix, IqUpmixParams};
pub use log_mag_u8::{LogMagU8, LogMagU8Params};
pub use modulated_file_source::{ModulatedFileSource, ModulatedFileSourceParams};
#[cfg(feature = "multimon")]
pub use morse::{MorseDemod, MorseDemodParams};
pub use morse_audio_source::{MorseAudioSource, MorseAudioSourceParams};
#[cfg(feature = "multimon")]
pub use packet::{PacketDemod, PacketDemodParams};
#[cfg(feature = "multimon")]
pub use pager::{PagerDemod, PagerDemodParams};
pub use rds_demod::{RdsDemod, RdsDemodParams};
pub use real_resamp::{RealF32Resamp, RealF32ResampParams};
pub use render::{collapse_row_to_columns, compute_spectrum_stats, update_max_hold, SpectrumStats};
pub use rssi_probe::{RssiProbe, RssiProbeParams};
#[cfg(feature = "rtl_433")]
pub use rtl_433::{Rtl433Demod, Rtl433DemodParams};
pub use sine::{SineSource, SineSourceParams};
#[cfg(feature = "soapysdr")]
pub use soapy_source::{SoapyReadback, SoapySource, SoapySourceParams};
pub use spsc_ring::{AudioRing, IqRing, SpscRing};
pub use squelch::{Squelch, SquelchParams};
pub use ssb_demod::{Sideband, SsbDemod, SsbDemodParams};
pub use ssb_modulator::{SsbModulator, SsbModulatorParams};
pub use stereo_decoder::{StereoDecoder, StereoDecoderParams};
pub use tee_iq_f32::TeeIqF32;
pub use tee_real_f32::TeeRealF32;
pub use voice_transcribe::{VoiceTranscribe, VoiceTranscribeParams};
pub use ws_bridge::{
    BridgeSink, WsBridgeFftU8Params, WsBridgeParams, WsBridgeRx, WsBridgeRxF32, WsBridgeRxParams,
    WsBridgeTx, WsBridgeTxEvents, WsBridgeTxF32, WsBridgeTxFftU8,
};
#[cfg(feature = "wspr")]
pub use wspr::{WsprDemod, WsprDemodParams};

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

#[cfg(test)]
mod tests {
    use super::{registry, version};
    use std::collections::HashSet;

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
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
            "DcBlock",
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
            "AudioShaper",
            "AudioNrMono",
            "AudioNrStereo",
            "DtmfAudioSource",
            "DtmfDecoder",
            "MorseAudioSource",
            "EventStore",
            "EventsSink",
            "TeeIqF32",
            "TeeRealF32",
            "WsBridgeTx",
            "WsBridgeTxF32",
            "WsBridgeTxFftU8",
            "WsBridgeTxEvents",
            "WsBridgeRx",
            "WsBridgeRxF32",
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
        for n in ["PagerDemod", "PacketDemod", "EasDemod", "MorseDemod"] {
            assert!(
                names.contains(n),
                "{n} missing from registry under `multimon` feature (found: {names:?})",
            );
        }
        #[cfg(feature = "ft8")]
        assert!(
            names.contains("Ft8Demod"),
            "Ft8Demod missing from registry under `ft8` feature (found: {names:?})",
        );
        #[cfg(feature = "wspr")]
        assert!(
            names.contains("WsprDemod"),
            "WsprDemod missing from registry under `wspr` feature (found: {names:?})",
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
