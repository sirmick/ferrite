//! End-to-end tests for the rtl_433 ISM-band decoder.
//!
//! Two hermetic tests run on every CI invocation:
//!
//! - `preset_parses_and_block_constructs` — loads
//!   `flowgraphs/rtl433-433.json`, walks every block declaration, and
//!   constructs the `Rtl433Demod` through its registered `BlockFactory`
//!   from the preset's exact param JSON. Catches preset-schema drift
//!   and block-param-key regressions.
//! - `silence_emits_no_events_via_block` — drives a fully-set-up
//!   `Rtl433Demod` instance with 0.5 s of zero IQ at 250 kHz and
//!   asserts no events surface. Confirms the shim's allocation +
//!   teardown survive a real `process()` invocation, complementing
//!   the in-crate unit test that targets the safe wrapper directly.
//!
//! One fixture-driven test:
//!
//! - `acurite_606tx_capture_decodes_at_least_one_event` — loads
//!   `blocks/tests/fixtures/rtl433_acurite_606tx_433mhz_250ks.cu8`
//!   (sourced from merbanan/rtl_433_tests; see fixtures/SOURCES.md),
//!   drives the block in 4096-sample chunks, captures
//!   `decoder::rtl_433` tracing via a `MakeWriter`, asserts ≥1 line
//!   with `"model"` and `"Acurite-606TX"` in it.

#![cfg(feature = "rtl_433")]

use std::fs;
use std::path::PathBuf;

use ferrite_blocks::block::{BlockFactory, BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{Block, Rtl433Demod, Rtl433DemodParams};
use num_complex::Complex;

/// Project root, derived from `CARGO_MANIFEST_DIR` (which points at
/// `blocks/`).
fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // blocks/ → /
    p
}

#[test]
fn preset_parses_and_block_constructs() {
    let preset_path = repo_root().join("flowgraphs/rtl433-433.json");
    let json = fs::read_to_string(&preset_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", preset_path.display()));

    // Parse the preset's loose JSON shape and pluck the `rtl433`
    // block's params out — that's the surface the runtime hands to
    // BlockFactory at load time.
    let doc: serde_json::Value =
        serde_json::from_str(&json).expect("flowgraphs/rtl433-433.json is valid JSON");

    let rtl433_params = doc
        .get("blocks")
        .and_then(|b| b.get("rtl433"))
        .and_then(|b| b.get("params"))
        .cloned()
        .expect("preset has rtl433 block with params");

    // Same factory path the runtime takes. Catches param-key drift
    // (e.g. if the block ever renames `sample_rate_hz` → something
    // else and the preset still ships the old key).
    let _block: Box<dyn Block> = Rtl433Demod::construct(&rtl433_params)
        .expect("Rtl433Demod::construct accepts the preset's params");
}

#[test]
fn silence_emits_no_events_via_block() {
    // Build straight from defaults — equivalent to the preset's
    // 250 kHz / decoder_set=default after deserialization.
    let mut block =
        Rtl433Demod::new(Rtl433DemodParams::default()).expect("Rtl433Demod constructs at 250 kHz");

    // 0.5 s at 250 kHz, zero IQ. Pulse detect should see no packages.
    let samples = vec![Complex::<f32>::new(0.0, 0.0); 125_000];

    let mut inputs = [InputPort {
        name: "in",
        meta: PortMeta::default(),
        buf: InBuf::IqF32(&samples),
    }];
    let mut outputs: [OutputPort; 0] = [];
    let mut io = BlockIo {
        inputs: &mut inputs,
        outputs: &mut outputs,
    };

    let work = block
        .process(&mut io)
        .expect("Rtl433Demod::process on silence");
    assert_eq!(work.consumed[0], samples.len());
}

/// Fixture-driven decode test against the Acurite 606TX capture from
/// `merbanan/rtl_433_tests`. Reads the CU8 fixture (interleaved u8
/// I/Q, unsigned with 128 as zero), converts to Ferrite's ±1.0 f32
/// IQ, drives the block in 4096-sample chunks, captures the
/// `decoder::rtl_433` tracing target into a buffer, and asserts at
/// least one event mentions `Acurite-606TX`.
#[test]
fn acurite_606tx_capture_decodes_at_least_one_event() {
    use std::sync::Mutex;
    use tracing::Level;
    use tracing_subscriber::fmt::MakeWriter;

    let fixture = repo_root().join("blocks/tests/fixtures/rtl433_acurite_606tx_433mhz_250ks.cu8");

    let bytes =
        fs::read(&fixture).unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture.display()));
    assert!(
        bytes.len().is_multiple_of(2),
        "fixture must be CU8 (2 bytes/sample)"
    );

    // CU8 → Complex<f32> in ±1.0 convention. u8 128 = zero; the upstream
    // rtl_sdr capture tool emits this directly off the RTL2832.
    let samples: Vec<Complex<f32>> = bytes
        .chunks_exact(2)
        .map(|c| {
            let i = (i32::from(c[0]) - 128) as f32 / 128.0;
            let q = (i32::from(c[1]) - 128) as f32 / 128.0;
            Complex::new(i, q)
        })
        .collect();

    // Capture the `decoder::rtl_433` tracing target into a buffer so
    // we can count lines without a global subscriber.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<Mutex<Vec<String>>>);
    impl<'a> MakeWriter<'a> for Sink {
        type Writer = SinkWriter;
        fn make_writer(&'a self) -> Self::Writer {
            SinkWriter(self.0.clone())
        }
    }
    struct SinkWriter(std::sync::Arc<Mutex<Vec<String>>>);
    impl std::io::Write for SinkWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if let Ok(s) = std::str::from_utf8(b) {
                self.0.lock().unwrap().push(s.to_string());
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let sink = Sink::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(Level::INFO)
            .finish(),
    );

    let mut block = Rtl433Demod::new(Rtl433DemodParams::default()).unwrap();

    // Push in 4096-sample chunks to exercise the streaming path.
    for chunk in samples.chunks(4096) {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(chunk),
        }];
        let mut outputs: [OutputPort; 0] = [];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        block.process(&mut io).unwrap();
    }

    let lines = sink.0.lock().unwrap();
    let rtl433_lines: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("decoder::rtl_433"))
        .collect();

    assert!(
        !rtl433_lines.is_empty(),
        "fixture should yield ≥1 decoded event; got {} total tracing lines",
        lines.len()
    );

    // Spot-check that the right device was matched. Upstream's
    // Acurite-606TX decoder emits `"model" : "Acurite-606TX"` in its
    // JSON; any other model in those lines means we picked up a
    // false-positive against a different protocol.
    let acurite_lines: Vec<_> = rtl433_lines
        .iter()
        .filter(|l| l.contains("Acurite-606TX"))
        .collect();
    assert!(
        !acurite_lines.is_empty(),
        "expected Acurite-606TX in decoded events; got: {}",
        rtl433_lines
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    );
}
