//! Ferrite flowgraph runtime.
//!
//! Parses a `FlowgraphDoc` (JSON), instantiates blocks from a registry,
//! runs the topological scheduler, drives the lifecycle state machine.
//! Dual-compile: the same crate ships as native (`ferrited`) and as
//! WebAssembly (browser). The server-half and browser-half of a preset
//! are two instances of this runtime, joined by `WsBridge` blocks on
//! wires that cross the environment boundary.
//!
//! This is the Rust counterpart of the TS `packages/flowgraph-runtime`;
//! the JSON doc shape is identical by design so one preset file parses
//! on either side. The TS package will be deleted at milestone M4.

pub mod block_registry;
pub mod compose;
pub mod doc;
pub mod env_split;
pub mod instantiate;
pub mod reconfigure;
pub mod runtime;
pub mod schedule;
pub mod typed_ring;
pub mod validate;

#[cfg(feature = "wasm")]
pub mod wasm;

pub use block_registry::{instantiate_blocks, BlockMap, InventorySpecRegistry};
pub use compose::{compose_source, ComposeError, SourceConfig, SOURCE_ID, SOURCE_SENTINEL_TYPE};
pub use doc::{BlockInstanceDecl, Environment, FlowgraphDoc, Wire};
pub use env_split::{split_for_environment, SplitError, CROSS_ENV_STREAM_BASE};
pub use instantiate::{instantiate_flowgraph, SpecMap, SpecRegistry};
pub use reconfigure::{
    diff_presets, ParamChange, ReconfigureError, ReconfigurePlan, StructuralChange,
};
pub use runtime::{Runtime, RuntimeState, DEFAULT_FRAMES_HINT};
pub use schedule::{build_wire_plan, topological_order, InputSource, Schedule, WirePlan};
pub use typed_ring::TypedRing;
pub use validate::{validate_doc, FlowgraphValidationError, Phase, ValidatedDoc, ValidationError};
