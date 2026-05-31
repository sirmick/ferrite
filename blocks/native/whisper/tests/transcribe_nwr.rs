//! End-to-end native whisper test: load the shipped ggml model, feed a
//! slice of the NOAA Weather Radio sample, assert real English comes out.
//!
//! This is the test that validates the C ABI — an rlib build only checks
//! the Rust side compiles; nothing resolves `wsp_*` until something
//! links and *calls* it. So this doubles as the link/signature check.
//!
//! Ignored by default: it needs the vendored whisper.cpp built (heavy,
//! and absent in a clean checkout) plus the model + sample files. Run
//! explicitly:
//!
//!   cargo test -p ferrite-whisper --test transcribe_nwr -- --ignored --nocapture
//!
//! Skips gracefully (returns early, not fail) if the model/sample aren't
//! present, so it never red-builds a machine that just lacks the assets.

use std::path::Path;

use ferrite_whisper::Whisper;

/// Repo root from this crate's manifest dir (`blocks/native/whisper`).
fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("…/blocks/native/whisper has 3 ancestors")
        .to_path_buf()
}

/// Minimal mono PCM-16 WAV reader → f32 in [-1,1], plus the sample rate.
/// Avoids adding a wav crate just for one test. Assumes canonical 44-byte
/// header with a `data` chunk (true for our `samples/` fixtures).
fn read_wav_mono_i16(path: &Path) -> Option<(Vec<f32>, u32)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    // Walk chunks to find `fmt ` (rate) and `data`.
    let mut rate = 0u32;
    let mut data: Option<&[u8]> = None;
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let sz =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let body = i + 8;
        if body + sz > bytes.len() {
            break;
        }
        if id == b"fmt " && sz >= 16 {
            rate = u32::from_le_bytes([
                bytes[body + 4],
                bytes[body + 5],
                bytes[body + 6],
                bytes[body + 7],
            ]);
        } else if id == b"data" {
            data = Some(&bytes[body..body + sz]);
        }
        i = body + sz + (sz & 1); // chunks are word-aligned
    }
    let data = data?;
    let pcm: Vec<f32> = data
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect();
    Some((pcm, rate))
}

/// Crude integer-ratio decimator to 16 kHz (whisper's required rate).
/// Averages each input window into one output sample — good enough to
/// prove decode; production resampling lives in the real pipeline.
fn resample_to_16k(pcm: &[f32], in_rate: u32) -> Vec<f32> {
    const OUT: u32 = 16_000;
    if in_rate == OUT {
        return pcm.to_vec();
    }
    let ratio = in_rate as f64 / OUT as f64;
    let n_out = (pcm.len() as f64 / ratio) as usize;
    (0..n_out)
        .map(|i| {
            let start = (i as f64 * ratio) as usize;
            let end = (((i + 1) as f64 * ratio) as usize)
                .min(pcm.len())
                .max(start + 1);
            let win = &pcm[start..end.min(pcm.len())];
            win.iter().copied().sum::<f32>() / win.len() as f32
        })
        .collect()
}

#[test]
#[ignore = "needs vendored whisper.cpp built + model/sample assets; run with --ignored"]
fn transcribes_nwr_sample_to_english() {
    let root = repo_root();
    let model_path = root.join("web/static/models/ggml-tiny.en-q5_1.bin");
    let wav_path = root.join("samples/nwr_5min.wav");

    if !model_path.exists() || !wav_path.exists() {
        eprintln!("skip: model or sample missing ({model_path:?} / {wav_path:?})");
        return;
    }

    let model = std::fs::read(&model_path).expect("read model");
    let whisper = Whisper::from_model_bytes(&model)
        .expect("engine loads model (vendored whisper.cpp built?)");

    let (pcm, rate) = read_wav_mono_i16(&wav_path).expect("parse wav");
    let pcm16k = resample_to_16k(&pcm, rate);
    // First 20 s is plenty to get several segments without a slow test.
    let clip = &pcm16k[..pcm16k.len().min(16_000 * 20)];

    let segments = whisper
        .transcribe(clip, "")
        .expect("transcribe returns segments");
    let text: String = segments
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();

    eprintln!("--- whisper decode (first 20s of NWR) ---\n{text}\n---");
    assert!(!segments.is_empty(), "got at least one segment");
    // NWR is a weather broadcast — at least one of these near-certain
    // words should appear. Loose on purpose (tiny.en, crude resample).
    let hits = [
        "weather",
        "wind",
        "temperature",
        "forecast",
        "today",
        "degrees",
        "sunny",
        "clear",
    ]
    .iter()
    .filter(|w| text.contains(**w))
    .count();
    assert!(hits >= 1, "expected weather vocabulary in: {text:?}");
}
