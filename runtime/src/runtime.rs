//! Tick pump — owns the instantiated blocks, allocates typed buffers per
//! output port, and drives `Block::process` in topological order.
//!
//! One [`Runtime`] owns one graph. `init()` allocates buffers and calls
//! every block's `init`; `tick()` runs one pass of `process` across the
//! topo order, feeding each consumer a view of its upstream producer's
//! buffer sliced to the last reported `Work.produced[i]`; `stop()`
//! drains in reverse order.
//!
//! **Back-pressure is coarse.** Every block sees its full-capacity output
//! buffer; unconsumed inputs are not re-presented next tick. Good enough
//! for the linear chains that ship today (e.g. `SineSource → Decimator →
//! FmDemod`); a credit-based scheme lands when a graph needs it.
//!
//! Lifecycle: `Created → Initialized → Running → Stopped`. `tick()` is
//! callable from `Initialized` or `Running` so tests can drive ticks
//! in lock-step without flipping to `Running`. `Stopped` is terminal
//! — no re-use. `Reconfigure` events come in M3 and don't add a new
//! state here; they're handled in place while `Running`.
//!
//! The TS counterpart used to live in `packages/flowgraph-runtime/src/runtime.ts`
//! and will be deleted at M4 once the browser loads this crate as WASM.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use num_complex::Complex;

use ferrite_blocks::{
    Block, BlockIo, BlockSpec, InBuf, InitCtx, InputPort, OutBuf, OutputPort, PortMeta, PortType,
    MAX_PORTS,
};

use crate::block_registry::{instantiate_blocks, BlockMap, InventorySpecRegistry};
use crate::doc::{Environment, FlowgraphDoc};
use crate::instantiate::{instantiate_flowgraph, SpecMap};
use crate::reconfigure::{diff_presets, ReconfigurePlan};
use crate::schedule::Schedule;
use crate::validate::validate_doc;

/// Default per-call frame budget. 1024 matches the browser `AudioWorklet`
/// batch (`128 · 8`) and is a reasonable default for native too.
pub const DEFAULT_FRAMES_HINT: usize = 1024;

/// The tick pump. Owns every block instance, every inter-block buffer,
/// and the topological run order. Single-threaded by design — not `Sync`.
pub struct Runtime {
    /// Parallel to the topological order: index `i` is the block that
    /// runs `i`-th each tick.
    entries: Vec<BlockEntry>,
    /// `input_bindings[i][j]` resolves block `i`'s `j`-th input port
    /// (declaration order) to its upstream producer. `None` = dangling.
    input_bindings: Vec<Vec<Option<InputBinding>>>,
    frames_hint: usize,
    state: RuntimeState,
    /// The doc the currently-instantiated graph was built from. Set by
    /// [`Runtime::load_doc`]; `None` for runtimes built through
    /// [`Runtime::from_parts`] (those can't be reconfigured because we
    /// don't have an old doc to diff against).
    applied_doc: Option<FlowgraphDoc>,
    /// Environment the doc is running in. Paired with [`Self::applied_doc`]
    /// so a reconfigure can rebuild through [`Runtime::load_doc`] with
    /// the same half of a cross-env split.
    environment: Option<Environment>,
}

/// Lifecycle state of a [`Runtime`]. Transitions are explicit and
/// one-way; see the method docs for which state each call requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    /// Just constructed. Only [`Runtime::init`] is legal.
    Created,
    /// [`Runtime::init`] has run. [`Runtime::tick`] is legal (useful
    /// for lock-step tests), as is [`Runtime::start`].
    Initialized,
    /// [`Runtime::start`] has been called. [`Runtime::tick`] remains
    /// legal; the caller owns the tick cadence — there is no internal
    /// timer today.
    Running,
    /// Terminal. No further transitions; dispose and rebuild.
    Stopped,
}

struct BlockEntry {
    id: String,
    block: Box<dyn Block>,
    spec: BlockSpec,
    /// One buffer per declared output port, in declaration order.
    outputs: Vec<TypedBuf>,
    /// `produced[j]` = count of elements written into `outputs[j]` on
    /// the most recent `process` call. Downstream reads honour this.
    produced: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
struct InputBinding {
    /// Index into [`Runtime::entries`] of the producer block.
    producer_index: usize,
    /// Index into the producer's `spec.outputs` (and its `outputs` /
    /// `produced` vectors).
    producer_port_index: usize,
}

/// Typed owned sample buffer. One variant per [`PortType`]; [`TypedBuf`]
/// hands out matching [`InBuf`] / [`OutBuf`] views so block code never
/// sees a mismatched port type at compile time.
enum TypedBuf {
    IqF32(Vec<Complex<f32>>),
    IqS16(Vec<Complex<i16>>),
    RealF32(Vec<f32>),
    RealI16(Vec<i16>),
    FftF32(Vec<f32>),
    FftU8(Vec<u8>),
    Bits(Vec<u8>),
    Frames(Vec<u8>),
    Events(Vec<u8>),
}

impl TypedBuf {
    fn for_port(pt: PortType, n: usize) -> Self {
        match pt {
            PortType::IqF32 => Self::IqF32(vec![Complex::new(0.0, 0.0); n]),
            PortType::IqS16 => Self::IqS16(vec![Complex::new(0, 0); n]),
            PortType::RealF32 => Self::RealF32(vec![0.0; n]),
            PortType::RealI16 => Self::RealI16(vec![0; n]),
            PortType::FftF32 => Self::FftF32(vec![0.0; n]),
            PortType::FftU8 => Self::FftU8(vec![0; n]),
            PortType::Bits => Self::Bits(vec![0; n]),
            PortType::Frames => Self::Frames(vec![0; n]),
            PortType::Events => Self::Events(vec![0; n]),
        }
    }

    /// Read-only view limited to the first `len` elements.
    fn as_in_buf(&self, len: usize) -> InBuf<'_> {
        match self {
            Self::IqF32(v) => InBuf::IqF32(&v[..len.min(v.len())]),
            Self::IqS16(v) => InBuf::IqS16(&v[..len.min(v.len())]),
            Self::RealF32(v) => InBuf::RealF32(&v[..len.min(v.len())]),
            Self::RealI16(v) => InBuf::RealI16(&v[..len.min(v.len())]),
            Self::FftF32(v) => InBuf::FftF32(&v[..len.min(v.len())]),
            Self::FftU8(v) => InBuf::FftU8(&v[..len.min(v.len())]),
            Self::Bits(v) => InBuf::Bits(&v[..len.min(v.len())]),
            Self::Frames(v) => InBuf::Frames(&v[..len.min(v.len())]),
            Self::Events(v) => InBuf::Events(&v[..len.min(v.len())]),
        }
    }

    /// Mutable view over the full capacity. The block writes here and
    /// reports how many elements were produced via [`Work`](ferrite_blocks::Work).
    fn as_out_buf_mut(&mut self) -> OutBuf<'_> {
        match self {
            Self::IqF32(v) => OutBuf::IqF32(v.as_mut_slice()),
            Self::IqS16(v) => OutBuf::IqS16(v.as_mut_slice()),
            Self::RealF32(v) => OutBuf::RealF32(v.as_mut_slice()),
            Self::RealI16(v) => OutBuf::RealI16(v.as_mut_slice()),
            Self::FftF32(v) => OutBuf::FftF32(v.as_mut_slice()),
            Self::FftU8(v) => OutBuf::FftU8(v.as_mut_slice()),
            Self::Bits(v) => OutBuf::Bits(v.as_mut_slice()),
            Self::Frames(v) => OutBuf::Frames(v.as_mut_slice()),
            Self::Events(v) => OutBuf::Events(v.as_mut_slice()),
        }
    }
}

fn state_error(op: &'static str, state: RuntimeState) -> anyhow::Error {
    anyhow!("runtime: {op} not allowed from state {state:?}")
}

/// Dangling-input view — a zero-length slice of the right type so the
/// block can downcast cleanly without special-casing "no producer".
fn empty_in_buf(pt: PortType) -> InBuf<'static> {
    match pt {
        PortType::IqF32 => InBuf::IqF32(&[]),
        PortType::IqS16 => InBuf::IqS16(&[]),
        PortType::RealF32 => InBuf::RealF32(&[]),
        PortType::RealI16 => InBuf::RealI16(&[]),
        PortType::FftF32 => InBuf::FftF32(&[]),
        PortType::FftU8 => InBuf::FftU8(&[]),
        PortType::Bits => InBuf::Bits(&[]),
        PortType::Frames => InBuf::Frames(&[]),
        PortType::Events => InBuf::Events(&[]),
    }
}

impl Runtime {
    /// Full load path: parse, validate, instantiate, assemble. Uses the
    /// real inventory registry — the ordinary entry point for the
    /// server and the browser WASM host.
    pub fn load_doc(doc: &FlowgraphDoc, env: Environment, frames_hint: usize) -> Result<Self> {
        let v = validate_doc(doc).map_err(|e| anyhow!("validate: {e}"))?;
        let (specs, schedule) = instantiate_flowgraph(&v, &InventorySpecRegistry, env)
            .map_err(|e| anyhow!("instantiate: {e}"))?;
        let blocks = instantiate_blocks(doc)?;
        let mut rt = Self::from_parts(blocks, &specs, &schedule, frames_hint)?;
        rt.applied_doc = Some(doc.clone());
        rt.environment = Some(env);
        Ok(rt)
    }

    /// Assemble a runtime from already-produced pieces. `blocks` is
    /// consumed — instances move into the runtime. `specs` and
    /// `schedule` are borrowed for the build and are not needed
    /// afterwards.
    pub fn from_parts(
        mut blocks: BlockMap,
        specs: &SpecMap,
        schedule: &Schedule,
        frames_hint: usize,
    ) -> Result<Self> {
        // 1. Map block id → its index in the topo order.
        let id_to_index: BTreeMap<String, usize> = schedule
            .order
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), i))
            .collect();

        // 2. Build entries in topo order, consuming from `blocks` and
        //    `specs` so we don't need to clone the block trait objects.
        let mut entries = Vec::with_capacity(schedule.order.len());
        for id in &schedule.order {
            let block = blocks
                .remove(id)
                .ok_or_else(|| anyhow!("runtime: block {id:?} missing from instantiated map"))?;
            let spec = specs
                .get(id)
                .copied()
                .ok_or_else(|| anyhow!("runtime: spec missing for {id:?}"))?;
            if spec.inputs.len() > MAX_PORTS || spec.outputs.len() > MAX_PORTS {
                return Err(anyhow!(
                    "runtime: block {id:?} declares > {MAX_PORTS} ports on one side"
                ));
            }
            let outputs = spec
                .outputs
                .iter()
                .map(|p| TypedBuf::for_port(p.port_type, frames_hint))
                .collect::<Vec<_>>();
            let produced = vec![0; spec.outputs.len()];
            entries.push(BlockEntry {
                id: id.clone(),
                block,
                spec,
                outputs,
                produced,
            });
        }

        // 3. Resolve wire plan into numeric (producer_index, port_index)
        //    pairs per consumer.
        let mut input_bindings: Vec<Vec<Option<InputBinding>>> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let port_sources = schedule.wire_plan.get(&entry.id);
            let mut row = Vec::with_capacity(entry.spec.inputs.len());
            for ip in entry.spec.inputs {
                let binding = port_sources
                    .and_then(|map| map.get(ip.name))
                    .map(|src| -> Result<InputBinding> {
                        let producer_index =
                            *id_to_index.get(&src.source_block).ok_or_else(|| {
                                anyhow!(
                                    "runtime: wire names unknown producer {:?}",
                                    src.source_block
                                )
                            })?;
                        let producer_spec = &entries[producer_index].spec;
                        let producer_port_index = producer_spec
                            .outputs
                            .iter()
                            .position(|p| p.name == src.source_port)
                            .ok_or_else(|| {
                                anyhow!(
                                    "runtime: producer {:?} has no output port {:?}",
                                    src.source_block,
                                    src.source_port,
                                )
                            })?;
                        Ok(InputBinding {
                            producer_index,
                            producer_port_index,
                        })
                    })
                    .transpose()?;
                row.push(binding);
            }
            input_bindings.push(row);
        }

        Ok(Self {
            entries,
            input_bindings,
            frames_hint,
            state: RuntimeState::Created,
            applied_doc: None,
            environment: None,
        })
    }

    #[must_use]
    pub fn frames_hint(&self) -> usize {
        self.frames_hint
    }

    #[must_use]
    pub fn state(&self) -> RuntimeState {
        self.state
    }

    /// Call `init` on every block in topological order. Required before
    /// any `tick`. Blocks are free to ignore `frames_hint`; rate
    /// metadata in `InitCtx` is empty today and gets populated when
    /// the negotiation phase lands.
    pub fn init(&mut self) -> Result<()> {
        self.require_state(RuntimeState::Created, "init")?;
        let frames_hint = self.frames_hint;
        for entry in &mut self.entries {
            let mut ctx = InitCtx {
                input_meta: &[],
                output_meta: &[],
                frames_hint,
            };
            entry
                .block
                .init(&mut ctx)
                .with_context(|| format!("init block {:?}", entry.id))?;
        }
        self.state = RuntimeState::Initialized;
        Ok(())
    }

    /// Transition `Initialized → Running`. A no-op beyond the state
    /// flip today — there is no internal timer. Tests that only need
    /// to drive `tick()` directly can skip this call.
    pub fn start(&mut self) -> Result<()> {
        self.require_state(RuntimeState::Initialized, "start")?;
        self.state = RuntimeState::Running;
        Ok(())
    }

    /// Run one tick: walk blocks in topo order, calling `process` on
    /// each. Returns on the first block that errors; previously-run
    /// blocks keep their mutated state. Legal from `Initialized` or
    /// `Running`.
    pub fn tick(&mut self) -> Result<()> {
        match self.state {
            RuntimeState::Initialized | RuntimeState::Running => {}
            other => return Err(state_error("tick", other)),
        }
        let Self {
            entries,
            input_bindings,
            ..
        } = self;

        // Index-based walk: each iteration splits `entries` at `i`, so a
        // range loop is the natural shape. `iter_mut().enumerate()`
        // would hold a live borrow that blocks `split_at_mut`.
        #[allow(clippy::needless_range_loop)]
        for i in 0..entries.len() {
            let (head, tail) = entries.split_at_mut(i);
            let (this, _) = tail.split_first_mut().expect("i is within entries bounds");
            let bindings = &input_bindings[i];

            // Inputs view: each consumer port resolves to either an
            // upstream slice trimmed to last-produced, or the empty
            // slice if the port is dangling.
            let mut inputs: Vec<InputPort<'_>> = Vec::with_capacity(this.spec.inputs.len());
            for (idx, ip) in this.spec.inputs.iter().enumerate() {
                let buf = match &bindings[idx] {
                    Some(b) => {
                        let producer = &head[b.producer_index];
                        let len = producer.produced[b.producer_port_index];
                        producer.outputs[b.producer_port_index].as_in_buf(len)
                    }
                    None => empty_in_buf(ip.port_type),
                };
                inputs.push(InputPort {
                    name: ip.name,
                    meta: PortMeta::default(),
                    buf,
                });
            }

            // Outputs view: full capacity, mutable. The block decides
            // how many elements are live via `Work.produced`.
            let mut outputs: Vec<OutputPort<'_>> = Vec::with_capacity(this.spec.outputs.len());
            for (op, buf) in this.spec.outputs.iter().zip(this.outputs.iter_mut()) {
                outputs.push(OutputPort {
                    name: op.name,
                    meta: PortMeta::default(),
                    buf: buf.as_out_buf_mut(),
                });
            }

            let work = {
                let mut io = BlockIo {
                    inputs: &mut inputs,
                    outputs: &mut outputs,
                };
                this.block
                    .process(&mut io)
                    .with_context(|| format!("process block {:?}", this.id))?
            };

            for (j, slot) in this.produced.iter_mut().enumerate() {
                *slot = work.produced[j];
            }
        }
        Ok(())
    }

    /// Call `stop` on every block in **reverse** topological order so
    /// consumers drain before producers disappear. Accumulates errors
    /// — every block is asked to stop even if an earlier one fails.
    /// Legal from `Initialized` or `Running`; no-op if already
    /// `Stopped`. Terminal: no re-use.
    pub fn stop(&mut self) -> Result<()> {
        match self.state {
            RuntimeState::Stopped => return Ok(()),
            RuntimeState::Created => return Err(state_error("stop", self.state)),
            RuntimeState::Initialized | RuntimeState::Running => {}
        }
        let mut failures = Vec::new();
        for entry in self.entries.iter_mut().rev() {
            if let Err(e) = entry.block.stop() {
                failures.push(format!("stop {:?}: {e}", entry.id));
            }
        }
        self.state = RuntimeState::Stopped;
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("runtime: stop errors — {}", failures.join("; ")))
        }
    }

    fn require_state(&self, expected: RuntimeState, op: &'static str) -> Result<()> {
        if self.state == expected {
            Ok(())
        } else {
            Err(state_error(op, self.state))
        }
    }

    /// Borrow an instantiated block by id. Used by tests and
    /// reconfigure-style callers (M3) that need to reach a specific
    /// block's `set_*` methods.
    pub fn block_mut(&mut self, id: &str) -> Option<&mut dyn Block> {
        for entry in &mut self.entries {
            if entry.id == id {
                return Some(&mut *entry.block);
            }
        }
        None
    }

    /// Borrow an instantiated block by id as its concrete type `T`.
    /// Returns `None` if the id is absent or the block is a different
    /// type. Callers use this to hand non-JSON handles to a specific
    /// block — e.g. attaching a `BridgeSink` to a `WsBridgeTx` after
    /// the graph has been built.
    pub fn block_typed<T: Block + 'static>(&mut self, id: &str) -> Option<&mut T> {
        self.block_mut(id)
            .and_then(|b| b.as_any_mut().downcast_mut::<T>())
    }

    /// The doc the runtime was built from. `None` when constructed via
    /// [`Self::from_parts`] — those paths opt out of the reconfigure
    /// machinery because they have no old doc to diff against.
    #[must_use]
    pub fn applied_doc(&self) -> Option<&FlowgraphDoc> {
        self.applied_doc.as_ref()
    }

    /// Apply `new_doc` to this runtime. Returns the [`ReconfigurePlan`]
    /// describing the change. A no-op plan is returned unchanged without
    /// touching the running graph.
    ///
    /// ### Rollback contract
    ///
    /// Build-time failures (bad JSON, validator reject, instantiate
    /// error, init error on the new graph) leave this runtime **exactly
    /// as it was** — callers can safely retry with a different doc.
    /// The swap happens only after the replacement graph has cleared
    /// every phase of construction, so a partial apply is impossible.
    ///
    /// ### What this slice does
    ///
    /// Every scope — `SelfBlock`, `Downstream`, `SourceRestart` — is
    /// handled by a full rebuild today: the runtime loads the new doc
    /// into a fresh graph, stops the old one, and swaps state in place.
    /// The declared scope is still surfaced in the returned plan so the
    /// wire protocol and future in-place fast paths have the information
    /// they need; the behavioural optimisation is a follow-up.
    pub fn reconfigure(&mut self, new_doc: &FlowgraphDoc) -> Result<ReconfigurePlan> {
        let old_doc = self
            .applied_doc
            .as_ref()
            .ok_or_else(|| anyhow!("runtime: reconfigure requires a load_doc-built runtime"))?;
        let env = self
            .environment
            .ok_or_else(|| anyhow!("runtime: reconfigure requires a load_doc-built runtime"))?;

        let plan = diff_presets(old_doc, new_doc, &InventorySpecRegistry)
            .map_err(|e| anyhow!("reconfigure: {e}"))?;
        if plan.is_noop() {
            return Ok(plan);
        }

        // Build the replacement graph fully (including init) before we
        // touch the current one. Any failure here returns Err without
        // mutating `self`.
        let mut replacement = Self::load_doc(new_doc, env, self.frames_hint)?;
        replacement.init()?;

        // Match the prior lifecycle so a Running graph stays Running.
        let prev_state = self.state;
        if matches!(
            prev_state,
            RuntimeState::Initialized | RuntimeState::Running
        ) {
            // Best-effort stop: the new graph is already live in our hand,
            // so we don't want a stop error to block the swap.
            let _ = self.stop();
        }

        let Self {
            entries,
            input_bindings,
            frames_hint,
            state,
            applied_doc,
            environment,
        } = replacement;
        self.entries = entries;
        self.input_bindings = input_bindings;
        self.frames_hint = frames_hint;
        self.state = state;
        self.applied_doc = applied_doc;
        self.environment = environment;

        if prev_state == RuntimeState::Running {
            self.start()?;
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_registry::instantiate_blocks;
    use crate::doc::FlowgraphDoc;
    use crate::instantiate::instantiate_flowgraph;

    fn load(json: &str, frames: usize) -> Runtime {
        let doc: FlowgraphDoc = serde_json::from_str(json).unwrap();
        Runtime::load_doc(&doc, Environment::Browser, frames).unwrap()
    }

    #[test]
    fn sine_source_fills_its_output_buffer_every_tick() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource","params":{"rate_hz":4,"tone_freq_abs_hz":1,"center_freq_hz":0,"amplitude":0.5}}},
                "wires":[]
            }"#,
            8,
        );
        rt.init().unwrap();
        rt.tick().unwrap();
        // SineSource produces frames_hint samples per tick.
        let produced = rt.entries[0].produced[0];
        assert_eq!(produced, 8);
        // Quarter-rate offset gives the 4-sample pattern (a,0),(0,a),(-a,0),(0,-a).
        let TypedBuf::IqF32(buf) = &rt.entries[0].outputs[0] else {
            panic!("expected IqF32");
        };
        assert!((buf[0].re - 0.5).abs() < 1e-6 && buf[0].im.abs() < 1e-6);
        assert!(buf[1].re.abs() < 1e-6 && (buf[1].im - 0.5).abs() < 1e-6);
    }

    #[test]
    fn chain_propagates_samples_src_to_decim() {
        // SineSource → Decimator(factor=2). After one tick the decimator
        // consumes `frames_hint` IQ samples and emits ~half as many.
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{
                    "src":{"type":"SineSource","params":{"rate_hz":1000000,"tone_freq_abs_hz":10,"center_freq_hz":0}},
                    "decim":{"type":"Decimator","params":{"factor":2,"num_taps":17,"cutoff_normalized":0.2}}
                },
                "wires":[["src.out","decim.in"]]
            }"#,
            64,
        );
        rt.init().unwrap();
        rt.tick().unwrap();
        let src_produced = rt.entries[0].produced[0];
        let decim_produced = rt.entries[1].produced[0];
        assert_eq!(src_produced, 64);
        // Factor-2 decimator producing 32-ish outputs; allow ±1 for
        // state-machine rounding across the first buffer.
        assert!(
            decim_produced == 32 || decim_produced == 31,
            "decim produced {decim_produced}, expected ~32"
        );
    }

    #[test]
    fn state_progresses_through_lifecycle() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        assert_eq!(rt.state(), RuntimeState::Created);
        rt.init().unwrap();
        assert_eq!(rt.state(), RuntimeState::Initialized);
        rt.tick().unwrap(); // legal from Initialized
        rt.start().unwrap();
        assert_eq!(rt.state(), RuntimeState::Running);
        rt.tick().unwrap(); // still legal from Running
        rt.stop().unwrap();
        assert_eq!(rt.state(), RuntimeState::Stopped);
    }

    #[test]
    fn tick_before_init_errors() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        let err = rt.tick().unwrap_err();
        assert!(format!("{err}").contains("Created"));
    }

    #[test]
    fn init_twice_errors() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        rt.init().unwrap();
        let err = rt.init().unwrap_err();
        assert!(format!("{err}").contains("Initialized"));
    }

    #[test]
    fn start_before_init_errors() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        let err = rt.start().unwrap_err();
        assert!(format!("{err}").contains("Created"));
    }

    #[test]
    fn stop_without_init_errors() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        let err = rt.stop().unwrap_err();
        assert!(format!("{err}").contains("Created"));
    }

    #[test]
    fn stop_is_idempotent_after_first_stop() {
        // Second stop() is a no-op, not an error — matches the TS
        // runtime's contract so a `try { stop() }` in a cleanup path
        // doesn't flag on a double-stop race.
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        rt.init().unwrap();
        rt.tick().unwrap();
        rt.stop().unwrap();
        rt.stop().unwrap();
    }

    #[test]
    fn tick_after_stop_errors() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        rt.init().unwrap();
        rt.stop().unwrap();
        let err = rt.tick().unwrap_err();
        assert!(format!("{err}").contains("Stopped"));
    }

    #[test]
    fn block_mut_resolves_by_id() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        assert!(rt.block_mut("src").is_some());
        assert!(rt.block_mut("nope").is_none());
    }

    #[test]
    fn block_typed_downcasts_to_concrete_type() {
        use ferrite_blocks::SineSource;
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        assert!(rt.block_typed::<SineSource>("src").is_some());
        // Wrong type → None, not a panic.
        assert!(rt.block_typed::<ferrite_blocks::FftBlock>("src").is_none());
        // Missing id → None.
        assert!(rt.block_typed::<SineSource>("nope").is_none());
    }

    #[test]
    fn from_parts_rejects_missing_producer() {
        // Hand-roll a schedule where a wire points at a producer that
        // isn't in the block map — caller misuse; must error, not panic.
        use crate::schedule::InputSource;
        let doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"},"decim":{"type":"Decimator"}},
                "wires":[["src.out","decim.in"]]
            }"#,
        )
        .unwrap();
        let v = validate_doc(&doc).unwrap();
        let (specs, mut schedule) =
            instantiate_flowgraph(&v, &InventorySpecRegistry, Environment::Browser).unwrap();
        // Corrupt the wire plan: claim decim reads from a non-existent
        // "ghost" block. Bypasses validate_doc deliberately.
        schedule.wire_plan.get_mut("decim").unwrap().insert(
            "in".to_string(),
            InputSource {
                source_block: "ghost".to_string(),
                source_port: "out".to_string(),
                port_type: Some(PortType::IqF32),
            },
        );
        let blocks = instantiate_blocks(&doc).unwrap();
        let Err(err) = Runtime::from_parts(blocks, &specs, &schedule, 16) else {
            panic!("expected unknown-producer error");
        };
        assert!(format!("{err}").contains("ghost"));
    }

    #[test]
    fn load_doc_stashes_applied_doc_for_reconfigure() {
        let rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            16,
        );
        let applied = rt.applied_doc().expect("load_doc populates applied_doc");
        assert_eq!(applied.name, "t");
    }

    #[test]
    fn from_parts_leaves_applied_doc_empty() {
        // A runtime assembled from parts (no doc) has nothing to diff
        // against — the reconfigure path should refuse rather than guess.
        let doc: FlowgraphDoc = serde_json::from_str(
            r#"{"name":"t","environments":["browser"],"blocks":{"src":{"type":"SineSource"}},"wires":[]}"#,
        )
        .unwrap();
        let v = validate_doc(&doc).unwrap();
        let (specs, schedule) =
            instantiate_flowgraph(&v, &InventorySpecRegistry, Environment::Browser).unwrap();
        let blocks = instantiate_blocks(&doc).unwrap();
        let rt = Runtime::from_parts(blocks, &specs, &schedule, 16).unwrap();
        assert!(rt.applied_doc().is_none());
    }

    #[test]
    fn reconfigure_noop_on_identical_doc() {
        let doc_json = r#"{
            "name":"t","environments":["browser"],
            "blocks":{"src":{"type":"SineSource","params":{"amplitude":0.5}}},
            "wires":[]
        }"#;
        let mut rt = load(doc_json, 16);
        rt.init().unwrap();
        let doc: FlowgraphDoc = serde_json::from_str(doc_json).unwrap();
        let plan = rt.reconfigure(&doc).unwrap();
        assert!(plan.is_noop());
        // Lifecycle untouched.
        assert_eq!(rt.state(), RuntimeState::Initialized);
    }

    #[test]
    fn reconfigure_applies_param_change_and_preserves_running_state() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource","params":{"rate_hz":4,"tone_freq_abs_hz":1,"center_freq_hz":0,"amplitude":0.5}}},
                "wires":[]
            }"#,
            8,
        );
        rt.init().unwrap();
        rt.start().unwrap();
        let new_doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource","params":{"rate_hz":4,"tone_freq_abs_hz":1,"center_freq_hz":0,"amplitude":0.25}}},
                "wires":[]
            }"#,
        )
        .unwrap();
        let plan = rt.reconfigure(&new_doc).unwrap();
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].param_key, "amplitude");
        // Running-before is Running-after.
        assert_eq!(rt.state(), RuntimeState::Running);
        rt.tick().unwrap();
        // Amplitude actually took effect.
        let TypedBuf::IqF32(buf) = &rt.entries[0].outputs[0] else {
            panic!("expected IqF32");
        };
        assert!((buf[0].re - 0.25).abs() < 1e-6 && buf[0].im.abs() < 1e-6);
        // applied_doc is updated.
        assert_eq!(
            rt.applied_doc().unwrap().blocks["src"]
                .params
                .as_ref()
                .unwrap()["amplitude"]
                .as_f64(),
            Some(0.25)
        );
    }

    #[test]
    fn reconfigure_rolls_back_on_build_failure() {
        let original_json = r#"{
            "name":"t","environments":["browser"],
            "blocks":{"src":{"type":"SineSource"}},
            "wires":[]
        }"#;
        let mut rt = load(original_json, 16);
        rt.init().unwrap();
        rt.start().unwrap();
        // New doc is invalid — references a block type the registry
        // doesn't know.
        let bad_doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"},"ghost":{"type":"NotAThing"}},
                "wires":[]
            }"#,
        )
        .unwrap();
        let err = rt.reconfigure(&bad_doc).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("NotAThing") || msg.contains("ghost") || msg.contains("validate"),
            "unexpected err: {msg}"
        );
        // Rollback: the runtime is untouched. State, applied_doc, block
        // count all unchanged.
        assert_eq!(rt.state(), RuntimeState::Running);
        rt.tick().unwrap();
        assert_eq!(
            rt.applied_doc().unwrap().blocks.len(),
            1,
            "rollback must preserve old doc"
        );
    }

    #[test]
    fn reconfigure_refuses_when_no_applied_doc() {
        // Hand-built runtimes (from_parts) can't reconfigure — there's
        // no old doc to diff against.
        let doc: FlowgraphDoc = serde_json::from_str(
            r#"{"name":"t","environments":["browser"],"blocks":{"src":{"type":"SineSource"}},"wires":[]}"#,
        )
        .unwrap();
        let v = validate_doc(&doc).unwrap();
        let (specs, schedule) =
            instantiate_flowgraph(&v, &InventorySpecRegistry, Environment::Browser).unwrap();
        let blocks = instantiate_blocks(&doc).unwrap();
        let mut rt = Runtime::from_parts(blocks, &specs, &schedule, 16).unwrap();
        let err = rt.reconfigure(&doc).unwrap_err();
        assert!(format!("{err}").contains("reconfigure"));
    }
}
