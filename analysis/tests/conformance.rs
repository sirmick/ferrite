//! Conformance: the Rust analysis (what the daemon's `spectrum` /
//! `spectrogram` / `envelope` routes and the in-app AI run) against the
//! golden output that `tools/analysis` (the offline Python port) is also
//! tested against — so the two can't drift apart silently.
//!
//! Cases: `tools/analysis/tests/conformance_cases.json`, fixtures from
//! `samples/`. Golden: `tools/analysis/tests/golden/conformance.json`.
//!
//! - `cargo test -p ferrite-analysis --test conformance` compares the
//!   current Rust output with the golden (small float tolerance — rustfft
//!   picks SIMD paths per CPU).
//! - `FERRITE_CONFORMANCE_WRITE=1 …` rewrites the golden after an
//!   intentional definition change (then update the Python port too).
//! - `FERRITE_CONFORMANCE_OUT=<path> …` also writes the current output
//!   there (the Python suite's live mode uses it).

use std::path::{Path, PathBuf};

use ferrite_analysis as fa;
use serde_json::{json, Value};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// IQ as cf32 bytes: raw `.cf32`/`.iq`, or a 16-bit stereo WAV (L = I,
/// R = Q, ÷ 32768 — the daemon's file-source convention).
fn load_cf32(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if path.extension().and_then(|e| e.to_str()) != Some("wav") {
        return bytes;
    }
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let mut i = 12;
    let (mut channels, mut bits) = (0u16, 0u16);
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &bytes[i + 8..(i + 8 + len).min(bytes.len())];
        if id == b"fmt " {
            channels = u16::from_le_bytes([body[2], body[3]]);
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            assert_eq!((channels, bits), (2, 16), "IQ WAV must be s16 stereo");
            let mut out = Vec::with_capacity(body.len() * 2);
            for s in body.as_chunks::<2>().0 {
                let v = f32::from(i16::from_le_bytes(*s)) / 32768.0;
                out.extend_from_slice(&v.to_le_bytes());
            }
            return out;
        }
        i += 8 + len + (len & 1);
    }
    panic!("{}: no data chunk", path.display());
}

fn sidecar(path: &Path) -> Value {
    let name = path.file_name().unwrap().to_string_lossy();
    for cand in [
        path.with_file_name(format!("{name}.json")),
        path.with_extension("json"),
    ] {
        if let Ok(b) = std::fs::read(&cand) {
            return serde_json::from_slice(&b).unwrap();
        }
    }
    panic!("no sidecar for {}", path.display());
}

fn f(c: &Value, k: &str) -> f64 {
    c[k].as_f64()
        .unwrap_or_else(|| panic!("case {} needs {k}", c["name"]))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn run_case(c: &Value) -> Value {
    let fixture = repo().join(c["fixture"].as_str().unwrap());
    let side = sidecar(&fixture);
    let rate = side["sample_rate_hz"].as_f64().unwrap();
    let center = side["center_freq_hz"].as_f64().unwrap();
    let iq = load_cf32(&fixture);
    let meta = fa::IqMeta {
        rate_hz: rate,
        center_hz: center,
    };
    match c["tool"].as_str().unwrap() {
        "spectrum" => {
            let p = fa::SpectrumParams {
                rate,
                center,
                lo: f(c, "lo_hz"),
                hi: f(c, "hi_hz"),
                res_hz: f(c, "res_hz"),
                max_peaks: f(c, "max_peaks") as usize,
                profile_points: f(c, "profile_points") as usize,
                dc_guard_hz: f(c, "dc_guard_hz"),
            };
            let r = fa::spectrum(|| Ok(std::io::Cursor::new(iq.clone())), p).unwrap();
            serde_json::to_value(r).unwrap()
        }
        "envelope" => {
            let (fq, bw) = (f(c, "freq_hz"), f(c, "bw_hz"));
            let sl = fa::slice(
                std::io::Cursor::new(&iq),
                meta,
                fq - bw / 2.0,
                fq + bw / 2.0,
            )
            .unwrap();
            let e = fa::envelope(
                &sl,
                fa::EnvelopeParams {
                    window_ms: f(c, "window_ms"),
                },
            )
            .unwrap();
            serde_json::to_value(e).unwrap()
        }
        "spectrogram" => {
            let (lo, hi) = (f(c, "lo_hz"), f(c, "hi_hz"));
            let pad = (hi - lo) * 0.1;
            let sl = fa::slice(std::io::Cursor::new(&iq), meta, lo - pad, hi + pad).unwrap();
            let mut p = fa::SpectrogramParams::new(lo, hi);
            p.res_hz = f(c, "res_hz");
            p.step_ms = f(c, "step_ms");
            let sg = fa::spectrogram(&sl, p).unwrap();
            // Cells summarised (row / column means) — the full grid is too
            // big for a golden and the means catch any real divergence.
            let (rows, cols) = (sg.rows, sg.cols);
            #[allow(clippy::cast_precision_loss)]
            let row_mean: Vec<f64> = (0..rows)
                .map(|r| {
                    sg.cells[r * cols..(r + 1) * cols]
                        .iter()
                        .map(|&v| f64::from(v))
                        .sum::<f64>()
                        / cols as f64
                })
                .collect();
            #[allow(clippy::cast_precision_loss)]
            let col_mean: Vec<f64> = (0..cols)
                .map(|cc| {
                    (0..rows)
                        .map(|r| f64::from(sg.cells[r * cols + cc]))
                        .sum::<f64>()
                        / rows as f64
                })
                .collect();
            let mut v = serde_json::to_value(&sg).unwrap();
            v["row_mean"] = json!(row_mean);
            v["col_mean"] = json!(col_mean);
            v
        }
        other => panic!("unknown tool {other}"),
    }
}

/// Recursive approximate equality: numbers within `tol` (absolute),
/// everything else exact. Peaks are matched by frequency, not rank.
fn close(path: &str, a: &Value, b: &Value, errs: &mut Vec<String>) {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            let tol = 1e-3_f64.max(1e-6 * x.abs().max(y.abs()));
            if (x - y).abs() > tol {
                errs.push(format!("{path}: {x} vs {y}"));
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                errs.push(format!("{path}: len {} vs {}", x.len(), y.len()));
                return;
            }
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                close(&format!("{path}[{i}]"), p, q, errs);
            }
        }
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                match y.get(k) {
                    Some(w) => close(&format!("{path}.{k}"), v, w, errs),
                    None => errs.push(format!("{path}.{k}: missing")),
                }
            }
        }
        _ if a == b => {}
        _ => errs.push(format!("{path}: {a} vs {b}")),
    }
}

#[test]
fn rust_analysis_matches_the_shared_golden() {
    let dir = repo().join("tools/analysis/tests");
    let cases: Vec<Value> =
        serde_json::from_slice(&std::fs::read(dir.join("conformance_cases.json")).unwrap())
            .unwrap();
    let mut out = serde_json::Map::new();
    for c in &cases {
        out.insert(c["name"].as_str().unwrap().to_string(), run_case(c));
    }
    let out = Value::Object(out);
    let pretty = serde_json::to_string_pretty(&out).unwrap() + "\n";
    if let Ok(p) = std::env::var("FERRITE_CONFORMANCE_OUT") {
        std::fs::write(p, &pretty).unwrap();
    }
    let golden = dir.join("golden/conformance.json");
    if std::env::var("FERRITE_CONFORMANCE_WRITE").is_ok() {
        std::fs::write(&golden, &pretty).unwrap();
        return;
    }
    let want: Value = serde_json::from_slice(&std::fs::read(&golden).unwrap_or_else(|_| {
        panic!(
            "{} missing — run with FERRITE_CONFORMANCE_WRITE=1",
            golden.display()
        )
    }))
    .unwrap();
    let mut errs = Vec::new();
    close("", &want, &out, &mut errs);
    assert!(
        errs.is_empty(),
        "Rust analysis drifted from the golden (intentional? FERRITE_CONFORMANCE_WRITE=1 and update tools/analysis/ferrite_analyze/dsp.py):\n{}",
        errs.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );
}
