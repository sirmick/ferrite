//! Tick pump — owns the instantiated blocks, the per-wire accumulating
//! rings, and drives `Block::process` in a demand-driven loop across the
//! topological order.
//!
//! One [`Runtime`] owns one graph. `init()` allocates per-wire rings and
//! calls every block's `init`; `tick()` drives a work loop: walk blocks
//! in topo order, call `process` on any block whose inputs are ready
//! and whose outputs have free space, advance the rings by the reported
//! `Work.consumed` / `Work.produced`, and re-run the pass until no
//! block made progress. Sources (no inputs) run at most once per tick
//! so a producer can't starve the loop by filling a rate-expansion
//! chain indefinitely. `stop()` drains in reverse order.
//!
//! **Accumulating semantics.** Each connected wire is a power-of-two
//! [`TypedRing`] with one reader (Ferrite is one-wire-per-port, so
//! fan-out is a `Tee` block, not ring-level broadcast). Samples a
//! consumer doesn't touch this tick stay in the ring for the next
//! tick — the old overwrite-per-tick behaviour silently dropped them
//! and broke any chain whose consumer was slower than its producer.
//!
//! Lifecycle: `Created → Initialized → Running → Stopped`. `tick()` is
//! callable from `Initialized` or `Running` so tests can drive ticks
//! in lock-step without flipping to `Running`. `Stopped` is terminal
//! — no re-use. `Reconfigure` is handled by rebuilding a replacement
//! runtime and swapping state in place.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use num_complex::Complex;
use serde::{Deserialize, Serialize};

use ferrite_blocks::{
    Block, BlockIo, BlockSpec, InBuf, InitCtx, InputPort, OutBuf, OutputPort, PortMeta, PortType,
    MAX_PORTS,
};

use crate::block_registry::{BlockMap, InventorySpecRegistry};
use crate::doc::{Environment, FlowgraphDoc};
use crate::instantiate::{instantiate_flowgraph, SpecMap};
use crate::reconfigure::{diff_presets, ParamChange, ReconfigurePlan};
use crate::schedule::Schedule;
use crate::typed_ring::TypedRing;
use crate::validate::validate_doc;

/// Default per-call frame budget. 1024 matches the browser `AudioWorklet`
/// batch (`128 · 8`) and is a reasonable default for native too.
pub const DEFAULT_FRAMES_HINT: usize = 1024;

/// Default scheduler tick period assumed by callers that can't supply one
/// (unit tests, short-lived harnesses). 400µs matches the server's
/// `--tick-period-us` default. Runtime sizes source-output rings from
/// `rate × tick_period × SOURCE_RING_TICKS`, so the default keeps
/// rate-aware sizing active even when the caller is indifferent to the
/// real cadence.
pub const DEFAULT_TICK_PERIOD: Duration = Duration::from_micros(400);

/// Source output rings hold this many ticks' worth of samples before
/// back-pressure kicks in. 4× absorbs tick-cadence jitter and brief
/// downstream stalls without stalling the source reader, which for
/// hardware backends (SoapySource) would mean USB overflow.
const SOURCE_RING_TICKS: f64 = 4.0;

/// Safety cap on work-loop passes inside a single `tick()` call. Every
/// pass must make at least one block produce or consume; if none does,
/// the loop exits. The cap only matters for pathological blocks that
/// claim progress without moving any samples — real graphs settle in
/// O(depth) passes.
const MAX_TICK_PASSES: usize = 1024;

/// One port's cumulative counters. `samples_cum` is consumed (input) or
/// produced (output). `buffered` is the ring depth at snapshot time
/// (inputs only; 0 for outputs — a wire belongs to its reader).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagPort {
    pub name: String,
    pub samples_cum: u64,
    pub buffered: usize,
}

/// Per-block entry in a [`DiagSnapshot`]. Cumulative counters — the
/// consumer subtracts against the previous snapshot to get a rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagBlock {
    pub id: String,
    pub type_name: String,
    pub process_calls_cum: u64,
    /// Wall-clock nanoseconds spent inside `process()` cumulatively.
    /// Sourced from `web_time::Instant`, which transparently uses
    /// `std::time::Instant` on native and `performance.now()` on wasm.
    pub process_ns_cum: u64,
    pub inputs: Vec<DiagPort>,
    pub outputs: Vec<DiagPort>,
}

/// What `Runtime::diag_snapshot` returns. One entry per block, in
/// topological order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagSnapshot {
    pub blocks: Vec<DiagBlock>,
}

/// The tick pump. Owns every block instance, every inter-block ring
/// buffer, and the topological run order. Single-threaded by design —
/// not `Sync`.
pub struct Runtime {
    /// Parallel to the topological order: index `i` is the block that
    /// runs `i`-th each pass.
    entries: Vec<BlockEntry>,
    /// One ring per connected output port. Index into this vector is
    /// referenced from [`BlockEntry::input_wires`] and
    /// [`OutputSlot::Wire`]. `RefCell` so a block's input and output
    /// rings can be peeked concurrently from different slots within one
    /// `process` call.
    wires: Vec<RefCell<TypedRing>>,
    frames_hint: usize,
    tick_period: Duration,
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
    /// One slot per declared output port. `Wire(i)` means the port's
    /// samples flow into `Runtime::wires[i]`; `Scratch(buf)` means the
    /// port is unconnected and gets a stand-alone buffer so the block
    /// can still write without failing.
    outputs: Vec<OutputSlot>,
    /// One entry per declared input port. `Some(wire_idx)` resolves to
    /// the producer's ring; `None` is a dangling input (empty slice).
    input_wires: Vec<Option<usize>>,
    /// `produced[j]` = last-reported count for output port `j`, for
    /// introspection (tests, dashboards). Does not feed scheduling.
    produced: Vec<usize>,
    /// Monotonic per-port counters for the 1 Hz flow-table probe. Reset
    /// never — the JS/UI consumer takes deltas between snapshots so
    /// counters survive reconfigure reuse and never race a reader.
    in_samples_cum: Vec<u64>,
    out_samples_cum: Vec<u64>,
    /// Number of `process()` calls. Multiple per tick is normal (work
    /// loop re-runs until the block stops making progress).
    process_calls_cum: u64,
    /// Total wall-clock time spent inside `process()` in nanoseconds.
    /// Sourced via `web_time::Instant`, which works on both native
    /// (std clock) and wasm (`performance.now()`); both sides report
    /// real numbers in `flowdiag` snapshots.
    process_ns_cum: u64,
    /// `true` when this block instance was moved over from a previous
    /// runtime during reconfigure (its `init()` has already been called
    /// and, for stateful blocks like `SoapySource`, its reader thread
    /// is already running). [`Runtime::init`] skips `block.init()` on
    /// these so the reused block isn't asked to double-initialise. No
    /// effect on [`Runtime::stop`] — a carried-over block still needs
    /// a graceful shutdown when its new runtime winds down.
    carried_over: bool,
}

/// What backs one output port's buffer for a `process` call.
enum OutputSlot {
    /// Port is wired to a downstream consumer; writes go into this
    /// ring (peeked contiguously; may be shorter than the whole free
    /// region on wrap — the work loop re-runs the block for the tail).
    Wire(usize),
    /// Port has no consumer. Block still needs a mutable slice to write
    /// into — we give it this scratch buffer, sized to at least the
    /// block's `output_capacity_hints` or `frames_hint`, whichever is
    /// larger. Contents are discarded.
    Scratch(TypedBuf),
}

/// Scratch storage for unconnected output ports. Unlike wires, there's
/// no read pointer — the block overwrites the whole slice each call.
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
}

fn state_error(op: &'static str, state: RuntimeState) -> anyhow::Error {
    anyhow!("runtime: {op} not allowed from state {state:?}")
}

/// Heuristic: does this construction error look like a transient
/// exclusive-hardware conflict against the *previous* graph (i.e.
/// will it likely succeed if we drop the old graph and retry)?
///
/// Signatures we recognise:
///   - "RX stream already opened" — SoapyHackRF when the prior
///     Device handle still has an active RxStream on the cached pointer.
///   - "device deletion in-progress" — SoapyRTLSDR / SoapySDRplay
///     when a handle is mid-tear-down at the C/C++ layer.
fn is_exclusive_resource_conflict(err: &anyhow::Error) -> bool {
    let chain = err.chain().map(|e| e.to_string()).collect::<Vec<_>>();
    let blob = chain.join(" :: ");
    blob.contains("RX stream already opened") || blob.contains("device deletion in-progress")
}

/// Pull a readable message out of a `catch_unwind` payload. `panic!` with
/// a string literal or formatted string comes through as `&'static str`
/// or `String`; anything else falls back to a generic tag.
fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
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
    pub fn load_doc(
        doc: &FlowgraphDoc,
        env: Environment,
        frames_hint: usize,
        tick_period: Duration,
    ) -> Result<Self> {
        let v = validate_doc(doc).map_err(|e| anyhow!("validate: {e}"))?;
        let (specs, schedule) = instantiate_flowgraph(&v, &InventorySpecRegistry, env)
            .map_err(|e| anyhow!("instantiate: {e}"))?;
        let blocks = crate::block_registry::instantiate_blocks(doc)?;
        let mut rt = Self::from_parts(blocks, &specs, &schedule, frames_hint, tick_period)?;
        rt.applied_doc = Some(doc.clone());
        rt.environment = Some(env);
        Ok(rt)
    }

    /// Compute the set of block ids whose `type_name` + `params` +
    /// `placement` are byte-identical between two docs. Pure — does
    /// not touch any runtime state.
    fn reusable_ids(
        old_doc: &FlowgraphDoc,
        new_doc: &FlowgraphDoc,
    ) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for (id, old_decl) in &old_doc.blocks {
            if let Some(new_decl) = new_doc.blocks.get(id) {
                if old_decl.type_name == new_decl.type_name
                    && old_decl.params == new_decl.params
                    && old_decl.placement == new_decl.placement
                {
                    out.insert(id.clone());
                }
            }
        }
        out
    }

    /// Factory-construct every block in `doc` whose id is *not* in
    /// `skip` — used by [`Self::reconfigure`] to build replacement
    /// blocks speculatively before touching the live runtime. A factory
    /// failure here is safely rollback-able because nothing in `self`
    /// has been touched yet.
    fn construct_blocks_excluding(
        doc: &FlowgraphDoc,
        skip: &std::collections::BTreeSet<String>,
    ) -> Result<BlockMap> {
        let mut out = BlockMap::new();
        for (id, decl) in &doc.blocks {
            if skip.contains(id) {
                continue;
            }
            let entry = ferrite_blocks::registry::find(&decl.type_name).ok_or_else(|| {
                anyhow!("block {id:?}: type {:?} is not registered", decl.type_name)
            })?;
            let params = decl.params.clone().unwrap_or(serde_json::Value::Null);
            let block = entry
                .construct(&params)
                .with_context(|| format!("constructing block {id:?} ({})", decl.type_name))?;
            out.insert(id.clone(), block);
        }
        Ok(out)
    }

    /// Remove block instances by id from `self.entries`. Called by
    /// [`Self::reconfigure`] *only after* the rest of the new runtime
    /// has been speculatively constructed — at that point `self` is
    /// about to be replaced anyway, so the swap_remove side-effect
    /// (entries reordering) is fine.
    fn drain_blocks_by_id(&mut self, ids: &std::collections::BTreeSet<String>) -> BlockMap {
        let mut out = BlockMap::new();
        let mut i = 0;
        while i < self.entries.len() {
            if ids.contains(&self.entries[i].id) {
                let entry = self.entries.swap_remove(i);
                out.insert(entry.id, entry.block);
            } else {
                i += 1;
            }
        }
        out
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
        tick_period: Duration,
    ) -> Result<Self> {
        // 1. Map block id → its index in the topo order.
        let id_to_index: BTreeMap<String, usize> = schedule
            .order
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), i))
            .collect();

        // 2. Collect per-block specs and (consumed) block instances, in
        //    topo order.
        #[derive(Clone, Copy)]
        struct Skeleton {
            spec: BlockSpec,
        }
        let mut skeletons: Vec<Skeleton> = Vec::with_capacity(schedule.order.len());
        let mut instances: Vec<Box<dyn Block>> = Vec::with_capacity(schedule.order.len());
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
            skeletons.push(Skeleton { spec });
            instances.push(block);
        }

        // 3. Walk the wire plan and resolve (producer_idx, producer_port_idx)
        //    per consumer input. Build a per-producer-port → wire_idx map
        //    the same pass so we can cross-reference it when filling in
        //    producer-side output slots.
        let mut input_wires_per_block: Vec<Vec<Option<usize>>> = skeletons
            .iter()
            .map(|s| vec![None; s.spec.inputs.len()])
            .collect();
        // (producer_idx, producer_port_idx) → wire_idx.
        let mut producer_to_wire: BTreeMap<(usize, usize), usize> = BTreeMap::new();

        struct WireSpec {
            port_type: PortType,
            capacity: usize,
        }
        let mut wire_specs: Vec<WireSpec> = Vec::new();

        for (consumer_idx, id) in schedule.order.iter().enumerate() {
            let Some(port_sources) = schedule.wire_plan.get(id) else {
                continue;
            };
            let consumer_spec = skeletons[consumer_idx].spec;
            for (cp_idx, ip) in consumer_spec.inputs.iter().enumerate() {
                let Some(src) = port_sources.get(ip.name) else {
                    continue;
                };
                let producer_idx = *id_to_index.get(&src.source_block).ok_or_else(|| {
                    anyhow!(
                        "runtime: wire names unknown producer {:?}",
                        src.source_block
                    )
                })?;
                let producer_spec = skeletons[producer_idx].spec;
                let producer_port_idx = producer_spec
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
                let producer_port = producer_spec.outputs[producer_port_idx];

                let wire_idx = if let Some(&existing) =
                    producer_to_wire.get(&(producer_idx, producer_port_idx))
                {
                    existing
                } else {
                    // Wire capacity: respect the producer's declared
                    // output_capacity_hints (e.g. FFT=4096), floor at
                    // frames_hint. Pure sources also floor at
                    // `rate × tick_period × SOURCE_RING_TICKS` so a
                    // single tick can absorb the full burst — without
                    // this, `frames_hint=1024` caps pipeline throughput
                    // at ~2.56 MSPS regardless of the source's actual
                    // rate. Migrates to relative_rate-based sizing in
                    // a follow-up commit (B4).
                    let hints = instances[producer_idx].output_capacity_hints();
                    let mut capacity = frames_hint.max(hints[producer_port_idx]);
                    let producer_is_source = skeletons[producer_idx].spec.inputs.is_empty();
                    if producer_is_source && !tick_period.is_zero() {
                        if let Some(rate) =
                            instances[producer_idx].output_rate_hz(producer_port_idx)
                        {
                            if rate.is_finite() && rate > 0.0 {
                                let per_burst =
                                    (rate * tick_period.as_secs_f64() * SOURCE_RING_TICKS).ceil();
                                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                                let per_burst = per_burst as usize;
                                capacity = capacity.max(per_burst);
                            }
                        }
                    }
                    let idx = wire_specs.len();
                    wire_specs.push(WireSpec {
                        port_type: producer_port.port_type,
                        capacity,
                    });
                    producer_to_wire.insert((producer_idx, producer_port_idx), idx);
                    idx
                };
                input_wires_per_block[consumer_idx][cp_idx] = Some(wire_idx);
            }
        }

        // 4. Allocate rings.
        let wires: Vec<RefCell<TypedRing>> = wire_specs
            .into_iter()
            .map(|w| RefCell::new(TypedRing::new(w.port_type, w.capacity, 1)))
            .collect();

        // 5. Build entries. An output port is `Wire(idx)` if
        //    `producer_to_wire` has an entry for it; otherwise `Scratch`.
        let mut entries: Vec<BlockEntry> = Vec::with_capacity(schedule.order.len());
        for (idx, (id, block)) in schedule.order.iter().zip(instances).enumerate() {
            let Skeleton { spec } = skeletons[idx];
            let hints = block.output_capacity_hints();
            let outputs: Vec<OutputSlot> = spec
                .outputs
                .iter()
                .enumerate()
                .map(|(pi, p)| {
                    if let Some(&w) = producer_to_wire.get(&(idx, pi)) {
                        OutputSlot::Wire(w)
                    } else {
                        let cap = frames_hint.max(hints[pi]);
                        OutputSlot::Scratch(TypedBuf::for_port(p.port_type, cap))
                    }
                })
                .collect();
            let input_wires = std::mem::take(&mut input_wires_per_block[idx]);
            entries.push(BlockEntry {
                id: id.clone(),
                block,
                spec,
                outputs,
                input_wires,
                produced: vec![0; spec.outputs.len()],
                in_samples_cum: vec![0; spec.inputs.len()],
                out_samples_cum: vec![0; spec.outputs.len()],
                process_calls_cum: 0,
                process_ns_cum: 0,
                carried_over: false,
            });
        }

        Ok(Self {
            entries,
            wires,
            frames_hint,
            tick_period,
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
    pub fn tick_period(&self) -> Duration {
        self.tick_period
    }

    #[must_use]
    pub fn state(&self) -> RuntimeState {
        self.state
    }

    /// Snapshot of per-block counters + per-wire ring depth for the 1 Hz
    /// flow-table probe. Cumulative since block construction — the
    /// consumer (JS-side diag loop or server `drive`) takes deltas
    /// between successive snapshots to get per-second rates. Cheap: walks
    /// the entries + wires arrays, no locking beyond the caller's
    /// outer mutex.
    #[must_use]
    pub fn diag_snapshot(&self) -> DiagSnapshot {
        let mut blocks = Vec::with_capacity(self.entries.len());
        for (idx, e) in self.entries.iter().enumerate() {
            // Input buffered depth: look up the wire each input is
            // connected to. Each wire has one reader, so `readable_len`
            // is what this block sees queued.
            let inputs = e
                .spec
                .inputs
                .iter()
                .zip(e.in_samples_cum.iter())
                .enumerate()
                .map(|(j, (port, samples))| DiagPort {
                    name: port.name.to_string(),
                    samples_cum: *samples,
                    buffered: e.input_wires[j]
                        .map(|widx| self.wires[widx].borrow().available_read(0))
                        .unwrap_or(0),
                })
                .collect();
            let outputs = e
                .spec
                .outputs
                .iter()
                .zip(e.out_samples_cum.iter())
                .map(|(port, samples)| DiagPort {
                    name: port.name.to_string(),
                    samples_cum: *samples,
                    buffered: 0,
                })
                .collect();
            let _ = idx;
            blocks.push(DiagBlock {
                id: e.id.clone(),
                type_name: e.spec.type_name.to_string(),
                process_calls_cum: e.process_calls_cum,
                process_ns_cum: e.process_ns_cum,
                inputs,
                outputs,
            });
        }
        DiagSnapshot { blocks }
    }

    /// Call `init` on every block in topological order. Required before
    /// any `tick`. The runtime propagates per-port sample rates through
    /// `InitCtx.input_meta` — for each block, it collects each input's
    /// rate from the producing block's `output_rate_hz(port)` (already
    /// initialised on an earlier pass, since topo order). Rate-changing
    /// blocks like `Channelizer` use this to snap their factor / FIR
    /// sizing to the source's actual rate, not whatever the preset
    /// JSON declared. Rings are reset so a re-initialised graph starts
    /// from a clean state.
    pub fn init(&mut self) -> Result<()> {
        self.require_state(RuntimeState::Created, "init")?;
        let frames_hint = self.frames_hint;
        for ring in &self.wires {
            ring.borrow_mut().reset();
        }

        // Reverse-index: wire index → (producer block index, out port
        // index). Every connected wire has exactly one producer.
        let mut wire_producer: Vec<Option<(usize, usize)>> = vec![None; self.wires.len()];
        for (bi, entry) in self.entries.iter().enumerate() {
            for (pi, slot) in entry.outputs.iter().enumerate() {
                if let OutputSlot::Wire(widx) = slot {
                    wire_producer[*widx] = Some((bi, pi));
                }
            }
        }

        // Cache each block's resolved output rate AND centre frequency
        // as we iterate topo. Accounts for blocks whose
        // `output_rate_hz()` / `output_center_freq_hz()` return None
        // (default impl) by falling back to a 1:1 pass-through: rate
        // and centre equal input[0]'s. This lets pipelines like `src →
        // chan → demod → resamp` propagate both across intermediate
        // blocks (e.g. FmDemod) without every one needing to override.
        let mut block_out_rate: Vec<f64> = vec![0.0; self.entries.len()];
        let mut block_out_center: Vec<f64> = vec![0.0; self.entries.len()];
        for i in 0..self.entries.len() {
            // Build per-input-port (name, PortMeta) with rate + centre
            // resolved from the producing block. Producer ran earlier
            // in topo order (entries are topo-sorted at construction),
            // so `block_out_rate[producer]` / `block_out_center[producer]`
            // are valid by now.
            let input_meta: Vec<(&str, PortMeta)> = self.entries[i]
                .spec
                .inputs
                .iter()
                .enumerate()
                .map(|(port_idx, port_spec)| {
                    let producer =
                        self.entries[i].input_wires[port_idx].and_then(|widx| wire_producer[widx]);
                    let rate = producer.map(|(pb, _)| block_out_rate[pb]).unwrap_or(0.0);
                    let centre = producer.map(|(pb, _)| block_out_center[pb]).unwrap_or(0.0);
                    (
                        port_spec.name,
                        PortMeta {
                            sample_rate_hz: rate,
                            center_freq_hz: centre,
                        },
                    )
                })
                .collect();
            let in0_rate = input_meta
                .first()
                .map(|(_, m)| m.sample_rate_hz)
                .unwrap_or(0.0);
            let in0_center = input_meta
                .first()
                .map(|(_, m)| m.center_freq_hz)
                .unwrap_or(0.0);
            let mut ctx = InitCtx {
                input_meta: &input_meta,
                output_meta: &[],
                frames_hint,
            };
            let id = self.entries[i].id.clone();
            if self.entries[i].carried_over {
                // Reused from a previous runtime — its init() has
                // already run (and, for stateful blocks, set up external
                // state like reader threads that must not be
                // duplicated). If an upstream rate changed (source
                // retuned while the channelizer was preserved), rate-
                // aware blocks rebuild their FIR / decim / resampler
                // via `update_rates` without re-running the one-time
                // init path.
                self.entries[i]
                    .block
                    .update_rates(&ctx)
                    .with_context(|| format!("update_rates block {id:?}"))?;
            } else {
                self.entries[i]
                    .block
                    .init(&mut ctx)
                    .with_context(|| format!("init block {id:?}"))?;
            }
            // Cache this block's output rate + centre for downstream
            // InitCtx. Prefer the block's own overrides (sources +
            // rate-changers + frequency translators); fall back to
            // input[0]'s value as a 1:1 pass-through for sync blocks
            // (FmDemod, Tee, RssiProbe, Squelch, …) that don't override.
            block_out_rate[i] = self.entries[i].block.output_rate_hz(0).unwrap_or(in0_rate);
            block_out_center[i] = self.entries[i]
                .block
                .output_center_freq_hz(0)
                .unwrap_or(in0_center);
        }
        self.state = RuntimeState::Initialized;
        Ok(())
    }

    /// Re-run the input-rate propagation pass and call `update_rates`
    /// on every block whose upstream rate changed since the last init.
    /// Called from the driver loop (server-side `drive`, browser-side
    /// runner tick) every second or so. Blocks whose rates are stable
    /// skip the rebuild — `update_rates` checks and returns quickly.
    ///
    /// This is how dynamic rate changes cross the WS boundary:
    /// `WsBridgeRx::set_advertised_rate` updates its stored rate when a
    /// frame with a non-zero `sample_rate_hz` lands; the next
    /// `refresh_rates` walk sees that Rx advertises a different rate
    /// and propagates it to downstream `RealF32Resamp` / demod blocks
    /// so their ratios stay in sync with the live stream.
    ///
    /// Cheap: O(blocks × input_ports) reads, plus whatever rate-
    /// sensitive blocks do in their `update_rates`. No-op for graphs
    /// that haven't gone through `init` yet.
    pub fn refresh_rates(&mut self) -> Result<()> {
        if !matches!(
            self.state,
            RuntimeState::Initialized | RuntimeState::Running
        ) {
            return Ok(());
        }
        let frames_hint = self.frames_hint;
        let mut wire_producer: Vec<Option<(usize, usize)>> = vec![None; self.wires.len()];
        for (bi, entry) in self.entries.iter().enumerate() {
            for (pi, slot) in entry.outputs.iter().enumerate() {
                if let OutputSlot::Wire(widx) = slot {
                    wire_producer[*widx] = Some((bi, pi));
                }
            }
        }
        let mut block_out_rate: Vec<f64> = vec![0.0; self.entries.len()];
        let mut block_out_center: Vec<f64> = vec![0.0; self.entries.len()];
        for i in 0..self.entries.len() {
            let input_meta: Vec<(&str, PortMeta)> = self.entries[i]
                .spec
                .inputs
                .iter()
                .enumerate()
                .map(|(port_idx, port_spec)| {
                    let producer =
                        self.entries[i].input_wires[port_idx].and_then(|widx| wire_producer[widx]);
                    let rate = producer.map(|(pb, _)| block_out_rate[pb]).unwrap_or(0.0);
                    let centre = producer.map(|(pb, _)| block_out_center[pb]).unwrap_or(0.0);
                    (
                        port_spec.name,
                        PortMeta {
                            sample_rate_hz: rate,
                            center_freq_hz: centre,
                        },
                    )
                })
                .collect();
            let in0_rate = input_meta
                .first()
                .map(|(_, m)| m.sample_rate_hz)
                .unwrap_or(0.0);
            let in0_center = input_meta
                .first()
                .map(|(_, m)| m.center_freq_hz)
                .unwrap_or(0.0);
            let ctx = InitCtx {
                input_meta: &input_meta,
                output_meta: &[],
                frames_hint,
            };
            let id = self.entries[i].id.clone();
            self.entries[i]
                .block
                .update_rates(&ctx)
                .with_context(|| format!("refresh_rates block {id:?}"))?;
            block_out_rate[i] = self.entries[i].block.output_rate_hz(0).unwrap_or(in0_rate);
            block_out_center[i] = self.entries[i]
                .block
                .output_center_freq_hz(0)
                .unwrap_or(in0_center);
        }
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

    /// Run one tick: walk blocks in topo order calling `process` on any
    /// that are ready; re-walk until no block made progress (or the
    /// safety cap is hit). Sources (no inputs) run at most once per
    /// tick so an unconstrained producer can't dominate the loop.
    /// Legal from `Initialized` or `Running`.
    pub fn tick(&mut self) -> Result<()> {
        match self.state {
            RuntimeState::Initialized | RuntimeState::Running => {}
            other => return Err(state_error("tick", other)),
        }

        for entry in &mut self.entries {
            for slot in entry.produced.iter_mut() {
                *slot = 0;
            }
        }

        let mut source_ran = vec![false; self.entries.len()];

        for _ in 0..MAX_TICK_PASSES {
            let mut progress = false;
            // Indexed loop — body needs `&mut self` and indexes both
            // `self.entries` and `source_ran`; iterator rewrite would
            // need a split-borrow dance.
            #[allow(clippy::needless_range_loop)]
            for i in 0..self.entries.len() {
                let is_source = self.entries[i].spec.inputs.is_empty();
                if is_source && source_ran[i] {
                    continue;
                }
                let ran = self.try_run_block(i)?;
                if ran {
                    progress = true;
                    if is_source {
                        source_ran[i] = true;
                    }
                }
            }
            if !progress {
                return Ok(());
            }
        }
        Err(anyhow!(
            "runtime: tick work loop exceeded {MAX_TICK_PASSES} passes without settling"
        ))
    }

    /// Run block `i` once if its inputs and outputs permit. Returns
    /// `true` if the block produced or consumed at least one sample.
    fn try_run_block(&mut self, i: usize) -> Result<bool> {
        let spec = self.entries[i].spec;

        // 1. Output budget — min available_write across connected outs;
        //    unconnected scratches contribute their full length. No
        //    outputs → pure sink; use frames_hint as the nominal work
        //    budget so forecast math has a meaningful target.
        //
        //    Per-port cap is `max(frames_hint, output_capacity_hints[j])`
        //    so a block like FFT that needs a full `size=4096` dst to
        //    emit isn't starved by a default `frames_hint=1024`. The
        //    actual write_peek still clamps to ring free space.
        //
        //    Pure sources skip the `frames_hint` cap: their own reader
        //    thread paces them, and `frames_hint=1024` per tick would
        //    throttle any source above ~2.56 MSPS (at 400µs ticks). The
        //    downstream ring capacity, sized in `from_parts` from the
        //    source's declared rate × tick_period, is what actually
        //    bounds the burst.
        let hints = self.entries[i].block.output_capacity_hints();
        let is_source = spec.inputs.is_empty();
        let mut output_budget = usize::MAX;
        let saw_output = !spec.outputs.is_empty();
        #[allow(clippy::needless_range_loop)]
        for j in 0..spec.outputs.len() {
            let per_port_cap = if is_source {
                usize::MAX
            } else {
                self.frames_hint.max(hints[j])
            };
            match &self.entries[i].outputs[j] {
                OutputSlot::Wire(widx) => {
                    let avail = self.wires[*widx].borrow().available_write();
                    output_budget = output_budget.min(avail.min(per_port_cap));
                }
                OutputSlot::Scratch(buf) => {
                    output_budget = output_budget.min(scratch_capacity(buf).min(per_port_cap));
                }
            }
        }
        if !saw_output {
            output_budget = self.frames_hint;
        } else if output_budget == 0 {
            return Ok(false);
        }

        // 2. Forecast — how many inputs does the block need before the
        //    scheduler calls it? Custom override wins; otherwise each
        //    input needs at least one sample. The conservative "any
        //    input" default ensures event-driven consumers (DtmfDecoder,
        //    EventsSink) aren't starved when they're happy to work on
        //    whatever arrives. Rate-changing blocks that need a
        //    specific chunk size (FFT needs `size`, DtmfDecoder needs
        //    `block_size`) override `forecast` to declare it.
        let mut input_needs = [0usize; MAX_PORTS];
        {
            let block = &self.entries[i].block;
            if let Some(custom) = block.forecast(output_budget) {
                input_needs = custom;
            } else {
                for slot in input_needs.iter_mut().take(spec.inputs.len()) {
                    *slot = 1;
                }
            }
        }

        // 3. Input availability — each input must cover its need. A
        //    dangling input satisfies only a zero need.
        let mut input_avail = [0usize; MAX_PORTS];
        for ip in 0..spec.inputs.len() {
            let need = input_needs[ip];
            match self.entries[i].input_wires[ip] {
                Some(widx) => {
                    let avail = self.wires[widx].borrow().available_read(0);
                    if need > 0 && avail < need {
                        return Ok(false);
                    }
                    input_avail[ip] = avail;
                }
                None => {
                    if need > 0 {
                        return Ok(false);
                    }
                }
            }
        }

        // 4. Run the block inside an inner scope so every ring Ref /
        //    RefMut and every borrow of `self.entries[i]` is released
        //    before we advance the rings.
        let work = {
            let entry_mut = &mut self.entries[i];
            // Output RefMuts, one per output port (None for scratch).
            let mut output_borrows: Vec<Option<std::cell::RefMut<'_, TypedRing>>> = entry_mut
                .outputs
                .iter()
                .map(|o| match o {
                    OutputSlot::Wire(widx) => Some(self.wires[*widx].borrow_mut()),
                    OutputSlot::Scratch(_) => None,
                })
                .collect();
            // Input Refs (None for dangling).
            let input_borrows: Vec<Option<std::cell::Ref<'_, TypedRing>>> = entry_mut
                .input_wires
                .iter()
                .map(|w| w.map(|idx| self.wires[idx].borrow()))
                .collect();

            let mut inputs: Vec<InputPort<'_>> = Vec::with_capacity(spec.inputs.len());
            for ip in 0..spec.inputs.len() {
                let port = spec.inputs[ip];
                let max = input_avail[ip].max(input_needs[ip]);
                let buf = match &input_borrows[ip] {
                    Some(ring) => ring.read_peek(0, max),
                    None => empty_in_buf(port.port_type),
                };
                inputs.push(InputPort {
                    name: port.name,
                    meta: PortMeta::default(),
                    buf,
                });
            }

            // Outputs: iterate the output slots and the RefMut vec in
            // lock-step so both borrows are disjoint.
            let mut outputs: Vec<OutputPort<'_>> = Vec::with_capacity(spec.outputs.len());
            for (j, (slot, ob)) in entry_mut
                .outputs
                .iter_mut()
                .zip(output_borrows.iter_mut())
                .enumerate()
            {
                let port = spec.outputs[j];
                let buf = match slot {
                    OutputSlot::Wire(_) => ob
                        .as_mut()
                        .expect("Wire slot has a RefMut")
                        .write_peek(output_budget),
                    OutputSlot::Scratch(buf) => limited_out_buf_mut(buf, output_budget),
                };
                outputs.push(OutputPort {
                    name: port.name,
                    meta: PortMeta::default(),
                    buf,
                });
            }

            let work = {
                let mut io = BlockIo {
                    inputs: &mut inputs,
                    outputs: &mut outputs,
                };
                // Catch panics so one buggy block can't kill the whole
                // runtime — we surface them as a normal error with the
                // block id in context. `AssertUnwindSafe` is sound here
                // because the runtime halts on error; the block's
                // internal state may be corrupt but we won't reuse it.
                let t0 = web_time::Instant::now();
                let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    entry_mut.block.process(&mut io)
                }));
                #[allow(clippy::cast_possible_truncation)]
                {
                    entry_mut.process_ns_cum = entry_mut
                        .process_ns_cum
                        .saturating_add(t0.elapsed().as_nanos() as u64);
                }
                entry_mut.process_calls_cum = entry_mut.process_calls_cum.saturating_add(1);
                match panic_result {
                    Ok(res) => res.with_context(|| format!("process block {:?}", entry_mut.id))?,
                    Err(payload) => {
                        let msg = panic_payload_message(&payload);
                        return Err(anyhow!("block {:?} panicked: {msg}", entry_mut.id));
                    }
                }
            };
            // inputs/outputs/input_borrows/output_borrows all drop at
            // scope end, releasing the ring borrows before step 6.
            work
        };

        // 5. Accumulate per-tick produced for introspection, sum progress.
        //    A block may run multiple times per tick (work loop), so we
        //    sum across invocations rather than overwriting — otherwise
        //    a cumulative-output assertion would only see the last call.
        let mut any_progress = false;
        for j in 0..spec.outputs.len() {
            let produced = work.produced[j];
            self.entries[i].produced[j] += produced;
            self.entries[i].out_samples_cum[j] =
                self.entries[i].out_samples_cum[j].saturating_add(produced as u64);
            if produced > 0 {
                any_progress = true;
            }
        }
        for j in 0..spec.inputs.len() {
            let consumed = work.consumed[j];
            self.entries[i].in_samples_cum[j] =
                self.entries[i].in_samples_cum[j].saturating_add(consumed as u64);
        }

        // 6. Advance writers (connected outputs) and readers
        //    (connected inputs). Reborrow the wires fresh — the earlier
        //    scope has released every Ref/RefMut.
        for j in 0..spec.outputs.len() {
            let produced = work.produced[j];
            if produced == 0 {
                continue;
            }
            if let OutputSlot::Wire(widx) = self.entries[i].outputs[j] {
                self.wires[widx].borrow_mut().advance_writer(produced);
            }
        }
        for j in 0..spec.inputs.len() {
            let consumed = work.consumed[j];
            if consumed == 0 {
                continue;
            }
            any_progress = true;
            if let Some(widx) = self.entries[i].input_wires[j] {
                self.wires[widx].borrow_mut().advance_reader(0, consumed);
            }
        }

        Ok(any_progress)
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

        // Capture the pre-reconfigure run state once, up-front: the
        // recovery path below may stop the runtime, and we still want
        // to resume Running after a successful swap.
        let prev_state = self.state;

        // Reconfigure in three commit-safe phases:
        //
        //   1. Validate + speculatively construct every NEW block
        //      (anything whose type/params/placement differ from the
        //      old doc). All failures here roll back trivially — `self`
        //      is untouched.
        //   2. Drain reusable block instances out of `self.entries`.
        //      After this point we're committed: the old graph's
        //      structure is broken, but each surviving block is either
        //      in the new `blocks` map or queued for stop().
        //   3. Assemble the replacement runtime via `from_parts` — a
        //      deterministic wiring/indexing pass that only fails on
        //      programmer error.
        //
        // This is what makes "swapping preset wbfm → wbam keeps the
        // SoapySource running" cheap instead of doing a 5+ second SDR
        // close+reopen cycle.
        let v = validate_doc(new_doc).map_err(|e| anyhow!("validate: {e}"))?;
        let (specs, schedule) = instantiate_flowgraph(&v, &InventorySpecRegistry, env)
            .map_err(|e| anyhow!("instantiate: {e}"))?;

        let reusable = Self::reusable_ids(old_doc, new_doc);

        // Phase 1: build the brand-new blocks first. Some blocks own
        // exclusive hardware (notably SoapySource on HackRF, which
        // refuses a second `rx_stream()` until our previous one drops).
        // When that conflict surfaces we fall back to stop-old-then-
        // build; the retry rebuilds from scratch, deliberately giving
        // up the reuse optimisation for that one transition.
        let mut blocks = match Self::construct_blocks_excluding(new_doc, &reusable) {
            Ok(blocks) => blocks,
            Err(err) if is_exclusive_resource_conflict(&err) => {
                // Runtime crate doesn't depend on tracing (kept lean for the
                // wasm bundle); this rare recovery path stays on stderr.
                eprintln!(
                    "reconfigure: replacement build hit exclusive-resource conflict ({err}); \
                     stopping old graph and retrying"
                );
                if matches!(
                    prev_state,
                    RuntimeState::Initialized | RuntimeState::Running
                ) {
                    let _ = self.stop();
                }
                // Force-rebuild: skip the reuse path entirely so the old
                // hardware handles release before the retry asks the
                // driver for fresh ones.
                let empty = std::collections::BTreeSet::new();
                Self::construct_blocks_excluding(new_doc, &empty)?
            }
            Err(err) => return Err(err),
        };

        // Phase 2: drain reusable blocks from the old runtime and merge.
        for (id, block) in self.drain_blocks_by_id(&reusable) {
            blocks.insert(id, block);
        }

        // Phase 3: assemble. Any failure here is a programmer bug
        // because `validate_doc` + `instantiate_flowgraph` already
        // succeeded; surface it as-is.
        let mut replacement = Self::from_parts(
            blocks,
            &specs,
            &schedule,
            self.frames_hint,
            self.tick_period,
        )?;
        for entry in &mut replacement.entries {
            if reusable.contains(&entry.id) {
                entry.carried_over = true;
            }
        }
        replacement.applied_doc = Some(new_doc.clone());
        replacement.environment = Some(env);
        replacement.init()?;

        if matches!(
            self.state,
            RuntimeState::Initialized | RuntimeState::Running
        ) {
            let _ = self.stop();
        }

        let Self {
            entries,
            wires,
            frames_hint,
            tick_period,
            state,
            applied_doc,
            environment,
        } = replacement;
        self.entries = entries;
        self.wires = wires;
        self.frames_hint = frames_hint;
        self.tick_period = tick_period;
        self.state = state;
        self.applied_doc = applied_doc;
        self.environment = environment;

        if prev_state == RuntimeState::Running {
            self.start()?;
        }
        Ok(plan)
    }

    /// Apply a params delta to one block by id, then reconfigure. Merges
    /// `delta` into the applied doc's `blocks[id].params` (keys present
    /// in `delta` replace; other keys stay) and delegates to
    /// [`Self::reconfigure`].
    ///
    /// `delta` must be a JSON object. Errors when the runtime has no
    /// applied doc, the id is missing, or `delta` is a non-object JSON
    /// value. The merged doc is passed through `reconfigure`, so the
    /// same rollback guarantees apply — a build-time failure leaves the
    /// runtime unchanged.
    pub fn reconfigure_block(
        &mut self,
        id: &str,
        delta: serde_json::Value,
    ) -> Result<ReconfigurePlan> {
        let delta_obj = delta
            .as_object()
            .ok_or_else(|| anyhow!("params delta must be a JSON object"))?
            .clone();
        let mut new_doc = self
            .applied_doc
            .as_ref()
            .ok_or_else(|| anyhow!("runtime: reconfigure_block requires a load_doc-built runtime"))?
            .clone();
        let block = new_doc
            .blocks
            .get_mut(id)
            .ok_or_else(|| anyhow!("no block {id:?} in applied doc"))?;
        let mut merged = match block.params.take() {
            Some(serde_json::Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        for (k, v) in delta_obj {
            merged.insert(k, v);
        }
        block.params = Some(serde_json::Value::Object(merged));
        self.reconfigure(&new_doc)
    }

    /// Try the block's live-update path first; fall back to the full
    /// rebuild on `Ok(false)`. Returns the plan that was applied either
    /// way — the live branch fabricates a `SelfBlock`-scoped plan from
    /// the diff so callers see a uniform shape.
    ///
    /// On success the live branch also merges `delta` into the applied
    /// doc so subsequent diffs stay consistent with the live device
    /// state. A live-path error propagates as-is (doesn't fall back),
    /// because partial application would leave the doc and device out
    /// of sync.
    pub fn live_reconfigure_block(
        &mut self,
        id: &str,
        delta: serde_json::Value,
    ) -> Result<ReconfigurePlan> {
        let delta_obj = delta
            .as_object()
            .ok_or_else(|| anyhow!("params delta must be a JSON object"))?
            .clone();
        let block = self
            .block_mut(id)
            .ok_or_else(|| anyhow!("no block {id:?} in runtime"))?;
        if !block.apply_live_params(&delta)? {
            return self.reconfigure_block(id, delta);
        }

        // Live succeeded — patch applied_doc + synthesize a SelfBlock plan.
        let doc = self.applied_doc.as_mut().ok_or_else(|| {
            anyhow!("runtime: live_reconfigure_block requires a load_doc-built runtime")
        })?;
        let decl = doc
            .blocks
            .get_mut(id)
            .ok_or_else(|| anyhow!("no block {id:?} in applied doc"))?;
        let spec = ferrite_blocks::registry::find(decl.type_name.as_str())
            .map(ferrite_blocks::registry::BlockEntry::spec)
            .ok_or_else(|| anyhow!("registry: no spec for block type {:?}", decl.type_name))?;
        let mut merged = match decl.params.take() {
            Some(serde_json::Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        let mut changes = Vec::with_capacity(delta_obj.len());
        for (k, v) in delta_obj {
            let old_value = merged.get(&k).cloned().unwrap_or(serde_json::Value::Null);
            let scope = spec
                .params
                .iter()
                .find(|p| p.key == k)
                .map(|p| p.reconfig_scope)
                .unwrap_or(ferrite_blocks::ReconfigureScope::SelfBlock);
            changes.push(ParamChange {
                block_id: id.to_string(),
                param_key: k.clone(),
                old_value,
                new_value: v.clone(),
                scope,
            });
            merged.insert(k, v);
        }
        decl.params = Some(serde_json::Value::Object(merged));
        Ok(ReconfigurePlan {
            changes,
            structural: Vec::new(),
            overall: ferrite_blocks::ReconfigureScope::SelfBlock,
        })
    }
}

fn scratch_capacity(buf: &TypedBuf) -> usize {
    match buf {
        TypedBuf::IqF32(v) => v.len(),
        TypedBuf::IqS16(v) => v.len(),
        TypedBuf::RealF32(v) => v.len(),
        TypedBuf::RealI16(v) => v.len(),
        TypedBuf::FftF32(v) => v.len(),
        TypedBuf::FftU8(v) => v.len(),
        TypedBuf::Bits(v) => v.len(),
        TypedBuf::Frames(v) => v.len(),
        TypedBuf::Events(v) => v.len(),
    }
}

/// Build an OutBuf view over at most `max` leading elements of a scratch
/// buffer. Preserves the element type.
fn limited_out_buf_mut(buf: &mut TypedBuf, max: usize) -> OutBuf<'_> {
    match buf {
        TypedBuf::IqF32(v) => {
            let n = max.min(v.len());
            OutBuf::IqF32(&mut v[..n])
        }
        TypedBuf::IqS16(v) => {
            let n = max.min(v.len());
            OutBuf::IqS16(&mut v[..n])
        }
        TypedBuf::RealF32(v) => {
            let n = max.min(v.len());
            OutBuf::RealF32(&mut v[..n])
        }
        TypedBuf::RealI16(v) => {
            let n = max.min(v.len());
            OutBuf::RealI16(&mut v[..n])
        }
        TypedBuf::FftF32(v) => {
            let n = max.min(v.len());
            OutBuf::FftF32(&mut v[..n])
        }
        TypedBuf::FftU8(v) => {
            let n = max.min(v.len());
            OutBuf::FftU8(&mut v[..n])
        }
        TypedBuf::Bits(v) => {
            let n = max.min(v.len());
            OutBuf::Bits(&mut v[..n])
        }
        TypedBuf::Frames(v) => {
            let n = max.min(v.len());
            OutBuf::Frames(&mut v[..n])
        }
        TypedBuf::Events(v) => {
            let n = max.min(v.len());
            OutBuf::Events(&mut v[..n])
        }
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
        Runtime::load_doc(&doc, Environment::Browser, frames, DEFAULT_TICK_PERIOD).unwrap()
    }

    /// Copy the first `n` IqF32 elements of the named block's output
    /// scratch (unconnected only). Used by tests that inspect a
    /// source's samples directly.
    fn scratch_iq_f32(rt: &Runtime, block_idx: usize, port_idx: usize) -> Vec<Complex<f32>> {
        let slot = &rt.entries[block_idx].outputs[port_idx];
        let OutputSlot::Scratch(TypedBuf::IqF32(v)) = slot else {
            panic!("expected scratch IqF32 on block {block_idx}, port {port_idx}");
        };
        v.clone()
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
        let buf = scratch_iq_f32(&rt, 0, 0);
        assert!((buf[0].re - 0.5).abs() < 1e-6 && buf[0].im.abs() < 1e-6);
        assert!(buf[1].re.abs() < 1e-6 && (buf[1].im - 0.5).abs() < 1e-6);
    }

    #[test]
    fn chain_propagates_samples_src_to_decim() {
        // SineSource → Decimator(factor=2). After one tick the source
        // has run once, filling the ring; the decimator drains that
        // and emits ~half as many before stalling on empty input
        // (sources run at most once per tick). The low `rate_hz`
        // keeps rate-aware ring sizing below `frames_hint` so the
        // assertion doesn't drift with tick-period defaults.
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{
                    "src":{"type":"SineSource","params":{"rate_hz":1000,"tone_freq_abs_hz":10,"center_freq_hz":0}},
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
        let Err(err) = Runtime::from_parts(blocks, &specs, &schedule, 16, DEFAULT_TICK_PERIOD)
        else {
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
        let rt = Runtime::from_parts(blocks, &specs, &schedule, 16, DEFAULT_TICK_PERIOD).unwrap();
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
        let buf = scratch_iq_f32(&rt, 0, 0);
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
    fn reconfigure_preserves_byte_identical_block_instance() {
        // When a block's type_name + params + placement are unchanged
        // between the old and new docs, the reconfigure path must keep
        // the original Block instance instead of constructing a fresh
        // one. We verify that by checking the concrete `SineSource`
        // instance identity — its internal sample counter advances
        // after a tick, and a reused instance keeps that state; a
        // freshly-constructed one would start from zero again.
        use ferrite_blocks::SineSource;
        let doc_json = r#"{
            "name":"wbfm_like","environments":["browser"],
            "blocks":{
                "src":{"type":"SineSource","params":{"rate_hz":4,"tone_freq_abs_hz":1,"amplitude":0.5}},
                "demod":{"type":"FmDemod","params":{"sample_rate_hz":4,"max_deviation_hz":1}}
            },
            "wires":[["src.out","demod.in"]]
        }"#;
        let mut rt = load(doc_json, 8);
        rt.init().unwrap();
        rt.start().unwrap();
        rt.tick().unwrap();
        rt.tick().unwrap();
        let phase_before = rt.block_typed::<SineSource>("src").unwrap().phase();

        // New doc swaps the demod only. The source block is byte-
        // identical → must carry over.
        let new_doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name":"wbam_like","environments":["browser"],
                "blocks":{
                    "src":{"type":"SineSource","params":{"rate_hz":4,"tone_freq_abs_hz":1,"amplitude":0.5}},
                    "demod":{"type":"AmDemod","params":{"sample_rate_hz":4,"bias_tau_ms":100}}
                },
                "wires":[["src.out","demod.in"]]
            }"#,
        )
        .unwrap();
        rt.reconfigure(&new_doc).unwrap();

        // If the instance was preserved, the phase hasn't been reset
        // to zero. A fresh SineSource starts at phase 0.0.
        let phase_after = rt.block_typed::<SineSource>("src").unwrap().phase();
        assert_eq!(
            phase_before, phase_after,
            "SineSource phase should carry over unchanged across reconfigure; \
             before={phase_before} after={phase_after}"
        );
    }

    #[test]
    fn reconfigure_rebuilds_block_when_params_change() {
        // Mirror of the test above: when params differ the block must
        // be reconstructed, and a fresh SineSource starts back at
        // phase 0. Rate=5, tone=1 → phase_inc = 858_993_459, which
        // doesn't wrap evenly over the tick's sample count, so phase
        // settles at a non-zero value before reconfigure.
        use ferrite_blocks::SineSource;
        let doc_json = r#"{
            "name":"t","environments":["browser"],
            "blocks":{
                "src":{"type":"SineSource","params":{"rate_hz":5,"tone_freq_abs_hz":1,"amplitude":0.5}},
                "demod":{"type":"FmDemod","params":{"sample_rate_hz":5,"max_deviation_hz":1}}
            },
            "wires":[["src.out","demod.in"]]
        }"#;
        let mut rt = load(doc_json, 8);
        rt.init().unwrap();
        rt.start().unwrap();
        rt.tick().unwrap();
        rt.tick().unwrap();
        let phase_before = rt.block_typed::<SineSource>("src").unwrap().phase();
        assert!(phase_before > 0);

        let new_doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{
                    "src":{"type":"SineSource","params":{"rate_hz":5,"tone_freq_abs_hz":1,"amplitude":0.75}},
                    "demod":{"type":"FmDemod","params":{"sample_rate_hz":5,"max_deviation_hz":1}}
                },
                "wires":[["src.out","demod.in"]]
            }"#,
        )
        .unwrap();
        rt.reconfigure(&new_doc).unwrap();
        let phase_after = rt.block_typed::<SineSource>("src").unwrap().phase();
        assert_eq!(phase_after, 0, "fresh SineSource must start at phase 0");
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
        let mut rt =
            Runtime::from_parts(blocks, &specs, &schedule, 16, DEFAULT_TICK_PERIOD).unwrap();
        let err = rt.reconfigure(&doc).unwrap_err();
        assert!(format!("{err}").contains("reconfigure"));
    }

    #[test]
    fn reconfigure_block_merges_delta_preserving_other_params() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource","params":{"rate_hz":4,"tone_freq_abs_hz":1,"center_freq_hz":0,"amplitude":0.5}}},
                "wires":[]
            }"#,
            8,
        );
        rt.init().unwrap();
        let plan = rt
            .reconfigure_block("src", serde_json::json!({ "amplitude": 0.25 }))
            .unwrap();
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].param_key, "amplitude");
        // Untouched keys survived the merge.
        let params = rt.applied_doc().unwrap().blocks["src"]
            .params
            .as_ref()
            .unwrap();
        assert_eq!(params["amplitude"].as_f64(), Some(0.25));
        assert_eq!(params["tone_freq_abs_hz"].as_f64(), Some(1.0));
        assert_eq!(params["rate_hz"].as_f64(), Some(4.0));
    }

    #[test]
    fn reconfigure_block_noop_when_delta_matches_current() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource","params":{"amplitude":0.5}}},
                "wires":[]
            }"#,
            8,
        );
        rt.init().unwrap();
        let plan = rt
            .reconfigure_block("src", serde_json::json!({ "amplitude": 0.5 }))
            .unwrap();
        assert!(plan.is_noop());
    }

    #[test]
    fn reconfigure_block_rejects_unknown_block_id() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            8,
        );
        rt.init().unwrap();
        let err = rt
            .reconfigure_block("ghost", serde_json::json!({ "x": 1 }))
            .unwrap_err();
        assert!(format!("{err}").contains("ghost"));
    }

    #[test]
    fn reconfigure_block_rejects_non_object_delta() {
        let mut rt = load(
            r#"{
                "name":"t","environments":["browser"],
                "blocks":{"src":{"type":"SineSource"}},
                "wires":[]
            }"#,
            8,
        );
        rt.init().unwrap();
        let err = rt
            .reconfigure_block("src", serde_json::json!(42))
            .unwrap_err();
        assert!(format!("{err}").contains("JSON object"));
    }

    // --- B5: rate-expansion correctness ---------------------------------
    //
    // The old overwrite-per-tick runtime couldn't cope with a producer
    // whose per-call output volume exceeded `frames_hint` — an
    // AmModulator upsampling 8 kS/s → 48 kS/s would silently lose 5 out
    // of every 6 output samples because the next tick's peek clobbered
    // what wasn't drained. These tests pin the accumulating-ring
    // behaviour: totals across ticks match the declared rate ratio.

    fn producer_rate_dtmf_amod_decim(factor_up: u32, factor_down: u32) -> String {
        // DtmfAudioSource at 8 kS/s generates audio; AmModulator upsamples
        // to `8000 * factor_up`; Decimator brings it back down by
        // `factor_down`. Net rate is `factor_up / factor_down`.
        format!(
            r#"{{
                "name":"rate-expansion","environments":["browser"],
                "blocks":{{
                    "src": {{
                        "type":"DtmfAudioSource","placement":"browser",
                        "params":{{
                            "rate_hz":8000.0,"digits":"1","hold_ms":500,
                            "gap_ms":0,"amplitude":0.9,"loop_sequence":false
                        }}
                    }},
                    "ammod": {{
                        "type":"AmModulator","placement":"browser",
                        "params":{{
                            "input_rate_hz":8000.0,
                            "output_rate_hz":{out_rate}.0,
                            "offset_hz":0.0,"mod_depth":0.5
                        }}
                    }},
                    "decim": {{
                        "type":"Decimator","placement":"browser",
                        "params":{{"factor":{factor_down},"num_taps":9,"cutoff_normalized":0.1}}
                    }}
                }},
                "wires":[
                    ["src.out","ammod.in"],
                    ["ammod.out","decim.in"]
                ]
            }}"#,
            out_rate = 8000 * factor_up,
            factor_down = factor_down,
        )
    }

    /// Sum `produced[port]` across all ticks for a block by id — lets
    /// tests check cumulative flow, since `BlockEntry::produced` only
    /// holds the last tick's numbers.
    fn tick_and_sum_produced(
        rt: &mut Runtime,
        ticks: usize,
        ids: &[&str],
        port: usize,
    ) -> std::collections::HashMap<String, usize> {
        let mut totals: std::collections::HashMap<String, usize> =
            ids.iter().map(|id| ((*id).to_string(), 0_usize)).collect();
        for _ in 0..ticks {
            rt.tick().expect("tick");
            for id in ids {
                let entry = rt
                    .entries
                    .iter()
                    .find(|e| e.id == *id)
                    .unwrap_or_else(|| panic!("no block {id}"));
                *totals.get_mut(*id).unwrap() += entry.produced[port];
            }
        }
        totals
    }

    #[test]
    fn am_modulator_300x_upsample_does_not_drop_samples() {
        // AmModulator at 300× (8 kS/s → 2.4 MS/s) stresses the work
        // loop: one `frames_hint=1024` chunk of input should drive ~300
        // × 1024 output samples across the tick, which only fits
        // because the scheduler re-runs AmModulator until its input
        // ring is drained.
        let mut rt = load(&producer_rate_dtmf_amod_decim(300, 1), 1024);
        rt.init().unwrap();
        rt.start().unwrap();

        let totals = tick_and_sum_produced(&mut rt, 8, &["src", "ammod"], 0);
        let src_total = totals["src"];
        let ammod_total = totals["ammod"];
        assert!(src_total > 0, "source produced nothing");
        // Allow a small ramp-up window — first tick's input only
        // partly drains because the output ring fills before the loop
        // can finish. After the first tick the loop re-runs until
        // empty. The ratio should be ≥ ~290× on average over 8 ticks.
        let expected = src_total.saturating_mul(300);
        assert!(
            ammod_total >= expected.saturating_sub(300),
            "ammod produced {ammod_total}, expected ~{expected} (src={src_total})",
        );
    }

    #[test]
    fn upsample_then_decimate_conserves_sample_count() {
        // 6× up, 6× down. Net 1:1 means each src sample drives exactly
        // one decimator output (minus transients from the FIR startup).
        let mut rt = load(&producer_rate_dtmf_amod_decim(6, 6), 512);
        rt.init().unwrap();
        rt.start().unwrap();

        let totals = tick_and_sum_produced(&mut rt, 16, &["src", "decim"], 0);
        let src_total = totals["src"];
        let decim_total = totals["decim"];
        assert!(src_total > 0);
        // Net rate is 1:1; allow ±5 % for filter transients and the
        // decimator's phase accumulator draining at tick boundaries.
        let low = src_total.saturating_mul(95) / 100;
        let high = src_total.saturating_mul(105) / 100 + 2;
        assert!(
            (low..=high).contains(&decim_total),
            "decim produced {decim_total}, expected {low}..={high} for src={src_total}",
        );
    }

    #[test]
    fn fft_emits_one_bin_block_per_size_samples() {
        // Accumulating FFT: given N input samples, the block emits
        // floor(N / size) output frames total with nothing dropped.
        // The FFT leaves surplus in the ring each call rather than
        // silently discarding it, so the accum stays intact across
        // re-invocations within a tick and across tick boundaries.
        // A low `rate_hz` keeps the source ring sized by
        // `frames_hint`, not by rate-aware sizing, so this test isn't
        // sensitive to tick-period defaults.
        let size = 128_usize;
        let mut rt = load(
            &format!(
                r#"{{
                    "name":"fft-accum","environments":["browser"],
                    "blocks":{{
                        "src": {{"type":"SineSource","placement":"browser",
                                 "params":{{"rate_hz":1000.0,"tone_freq_abs_hz":10.0,
                                            "center_freq_hz":0.0,"amplitude":0.5}}}},
                        "fft": {{"type":"FFT","placement":"browser",
                                 "params":{{"size":{size},"window":"hann"}}}}
                    }},
                    "wires":[["src.out","fft.in"]]
                }}"#,
            ),
            64,
        );
        rt.init().unwrap();
        rt.start().unwrap();
        // frames_hint=64 → source emits 64 iq samples/tick. After 12
        // ticks we have 768 samples → 6 FFT frames of size 128.
        let ticks = 12;
        let totals = tick_and_sum_produced(&mut rt, ticks, &["src", "fft"], 0);
        let expected_frames = (totals["src"] / size) * size;
        assert_eq!(
            totals["fft"], expected_frames,
            "fft produced {} samples, expected {} (src={}, size={})",
            totals["fft"], expected_frames, totals["src"], size,
        );
        assert!(totals["fft"] > 0, "FFT should have fired at least once");
    }

    #[test]
    fn panic_message_decodes_str_literal_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_payload_message(&payload), "boom");
    }

    #[test]
    fn panic_message_decodes_string_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("kaboom"));
        assert_eq!(panic_payload_message(&payload), "kaboom");
    }

    #[test]
    fn panic_message_falls_back_for_non_string_payloads() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42_u32);
        assert_eq!(panic_payload_message(&payload), "non-string panic payload");
    }
}
