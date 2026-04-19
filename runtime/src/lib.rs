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

pub mod doc;
pub mod schedule;
pub mod validate;

pub use doc::{BlockInstanceDecl, Environment, FlowgraphDoc, Wire};
pub use schedule::{build_wire_plan, topological_order, InputSource, Schedule, WirePlan};
pub use validate::{validate_doc, FlowgraphValidationError, Phase, ValidatedDoc, ValidationError};
