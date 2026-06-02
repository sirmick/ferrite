//! Ferrite flowgraph runtime.
//!
//! Parses a `FlowgraphDoc` (JSON), instantiates blocks from a registry,
//! runs the topological scheduler, drives the lifecycle state machine.
//! Dual-compile: the same crate ships as native (`ferrited`) and as
//! WebAssembly (browser). The server-half and browser-half of a preset
//! are two instances of this runtime, joined by `WsBridge` blocks on
//! wires that cross the environment boundary.
//!
//! This crate is the single, authoritative flowgraph runtime: the same
//! `FlowgraphDoc` JSON parses identically in the native and browser
//! builds, so one preset file drives both halves of a split graph.

pub mod apply_profile;
pub mod block_registry;
pub mod compose;
pub mod corpus;
pub mod doc;
pub mod env_split;
pub mod inject_dc_block;
pub mod inject_narrow_fft;
pub mod inject_signal_list;
pub mod inject_voice_transcribe;
pub mod instantiate;
pub mod reconfigure;
pub mod runtime;
pub mod schedule;
pub mod typed_ring;
pub mod validate;

#[cfg(feature = "wasm")]
pub mod wasm;

pub use apply_profile::{apply_profile, AudioSplit, Profile};
pub use block_registry::{instantiate_blocks, BlockMap, InventorySpecRegistry};
pub use compose::{compose_source, ComposeError, SourceConfig, SOURCE_ID, SOURCE_SENTINEL_TYPE};
pub use corpus::{validate_corpus, CorpusFinding};
pub use doc::{BlockInstanceDecl, Environment, FlowgraphDoc, VariantDecl, Wire};
pub use env_split::{split_for_environment, SplitError, CROSS_ENV_STREAM_BASE};
pub use inject_dc_block::inject_dc_block_taps;
pub use inject_narrow_fft::inject_narrow_fft_taps;
pub use inject_signal_list::inject_signal_list_taps;
pub use inject_voice_transcribe::inject_voice_transcribe;
pub use instantiate::{instantiate_flowgraph, SpecMap, SpecRegistry};
pub use reconfigure::{
    diff_presets, ParamChange, ReconfigureError, ReconfigurePlan, StructuralChange,
};
pub use runtime::{
    DiagBlock, DiagPort, DiagSnapshot, Runtime, RuntimeState, DEFAULT_FRAMES_HINT,
    DEFAULT_TICK_PERIOD,
};
pub use schedule::{build_wire_plan, topological_order, InputSource, Schedule, WirePlan};
pub use typed_ring::TypedRing;
pub use validate::{validate_doc, FlowgraphValidationError, Phase, ValidatedDoc, ValidationError};
