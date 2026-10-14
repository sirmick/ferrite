//! `DmrDecoder` against a real off-air capture, with dsd-fme as the
//! oracle.
//!
//! Fixture: `fixtures/dmr_441850_disc48k.s8` — 40 s of the 441.850 MHz
//! DMR repeater (SF Bay Area, colour code 1) received 2026-10-13 21:39
//! UTC on the RSPdx, channelised to 48 kHz and FM-discriminated (full
//! scale = 5 kHz deviation), stored as int8. Running dsd-fme 1.x
//! (`-fs -s 48000`) over the same audio reports:
//!
//! - slot 2: `TGT=31068 SRC=3107998 Group Call` with ~110 voice
//!   superframes,
//! - slot 1: `TGT=93 SRC=3228520 Group Call`,
//! - talker alias beginning `KR1JOE` (dsd-fme prints its alias lines
//!   with the 0-based slot index — `"Slot %d", slot` — so its "Slot 1"
//!   is timeslot 2, the slot 3107998 is talking on),
//! - colour code 01.
//!
//! The decoder is pure Rust (it also runs in WASM), so this is the gate
//! that its sync, slot, BPTC/RS and embedded-LC paths agree with the
//! reference implementation on a real signal.

use std::path::PathBuf;

use ferrite_blocks::DmrDecoder;

fn fixture() -> Vec<f32> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/dmr_441850_disc48k.s8");
    std::fs::read(&p)
        .unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
        .into_iter()
        .map(|b| f32::from(b as i8) / 127.0)
        .collect()
}

fn run(
    chunk: usize,
) -> (
    Vec<ferrite_blocks::dsd_helper::DsdEvent>,
    ferrite_blocks::dmr_decode::DmrStats,
) {
    let mut dec = DmrDecoder::new(48_000.0, false);
    dec.trace = std::env::var_os("DMR_TRACE").is_some();
    let mut events = Vec::new();
    for c in fixture().chunks(chunk) {
        dec.push(c);
        events.extend(dec.drain_events());
    }
    (events, dec.stats)
}

#[test]
fn decodes_the_same_calls_as_dsd_fme() {
    let (events, stats) = run(4_096);
    let lines: Vec<String> = events
        .iter()
        .map(|e| {
            format!(
                "{} S{:?} {:?}<-{:?} cc{:?} | {}",
                e.event, e.slot, e.tgt, e.src, e.cc, e.text
            )
        })
        .collect();
    let dump = format!("{stats:?}\n{}", lines.join("\n"));

    let has_call = |slot: u8, tgt: &str, src: &str| {
        events.iter().any(|e| {
            e.event == "call"
                && e.slot == Some(slot)
                && e.tgt.as_deref() == Some(tgt)
                && e.src.as_deref() == Some(src)
        })
    };
    assert!(
        has_call(2, "31068", "3107998"),
        "slot 2 TG 31068 call missing:\n{dump}"
    );
    assert!(
        has_call(1, "93", "3228520"),
        "slot 1 TG 93 call missing:\n{dump}"
    );
    assert!(
        events
            .iter()
            .filter(|e| e.event == "call")
            .all(|e| e.cc == Some(1)),
        "colour code must be 1:\n{dump}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.event == "alias" && e.slot == Some(2) && e.text.starts_with("KR1JOE")),
        "talker alias missing:\n{dump}"
    );
    // Nothing the oracle didn't see: every call is one of the two.
    for e in events.iter().filter(|e| e.event == "call") {
        let pair = (e.tgt.as_deref(), e.src.as_deref());
        assert!(
            pair == (Some("31068"), Some("3107998")) || pair == (Some("93"), Some("3228520")),
            "unexpected call {pair:?}:\n{dump}"
        );
    }
    // Exactly one verified-sync announcement for a continuous capture.
    assert_eq!(
        events.iter().filter(|e| e.event == "sync").count(),
        1,
        "{dump}"
    );
    // One `end` per call at most (repeaters repeat the terminator).
    let calls = events.iter().filter(|e| e.event == "call").count();
    let ends = events.iter().filter(|e| e.event == "end").count();
    assert!(ends <= calls, "{ends} ends for {calls} calls:\n{dump}");
    // FEC sanity: every LC the fixture carries checked out.
    assert_eq!(stats.lc_bad, 0, "{dump}");
    assert!(stats.lc_ok >= 20, "{dump}");
    assert_eq!(stats.emb_bad, 0, "{dump}");
    assert!(stats.emb_ok >= 10, "too few embedded LCs:\n{dump}");
    eprintln!("{dump}");
}

#[test]
fn chunking_does_not_change_the_result() {
    let key = |ev: &[ferrite_blocks::dsd_helper::DsdEvent]| {
        ev.iter()
            .map(|e| {
                (
                    e.event,
                    e.slot,
                    e.tgt.clone(),
                    e.src.clone(),
                    e.text.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let (a, _) = run(4_096);
    let (b, _) = run(37);
    assert_eq!(key(&a), key(&b));
}

/// An idle channel (discriminator noise) must produce nothing: the bare
/// sync correlation false-locks on noise now and then, but every event
/// is gated on an FEC/CRC-valid burst. Live, before the gate, an idle
/// repeater channel emitted a `sync` record every second.
#[test]
fn noise_produces_no_events() {
    let mut dec = DmrDecoder::new(48_000.0, false);
    // Deterministic LCG → roughly Gaussian (sum of 4 uniforms).
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };
    let noise: Vec<f32> = (0..48_000 * 30)
        .map(|_| (next() + next() + next() + next()) * 0.6)
        .collect();
    let mut events = Vec::new();
    for c in noise.chunks(4_096) {
        dec.push(c);
        events.extend(dec.drain_events());
    }
    assert!(events.is_empty(), "noise produced events: {events:?}");
    assert_eq!(dec.stats.lc_ok + dec.stats.emb_ok, 0);
}

/// Voice through the vocoder: the slot 2 call in the fixture is ~40 s
/// of AMBE+2 speech. With the `vocoder` feature the decoder must
/// synthesise it (and nothing on slot 1 beyond its short call).
/// `DMR_VOICE_DUMP=<path>` writes slot 2 as raw f32 for inspection.
#[cfg(feature = "vocoder")]
#[test]
fn synthesises_slot_voice() {
    let mut dec = DmrDecoder::new(48_000.0, false);
    let mut slots: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
    let mut buf = vec![0.0_f32; 16_000];
    for c in fixture().chunks(4_096) {
        dec.push(c);
        for (i, s) in slots.iter_mut().enumerate() {
            let n = dec.take_voice(i, &mut buf);
            s.extend_from_slice(&buf[..n]);
        }
    }
    if let Some(p) = std::env::var_os("DMR_VOICE_DUMP") {
        let bytes: Vec<u8> = slots[1].iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(p, bytes).unwrap();
    }
    let secs = |v: &[f32]| v.len() as f64 / 8_000.0;
    let rms = |v: &[f32]| {
        (v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
    };
    eprintln!(
        "slot1 {:.1} s rms {:.4}, slot2 {:.1} s rms {:.4}, {:?}",
        secs(&slots[0]),
        rms(&slots[0]),
        secs(&slots[1]),
        rms(&slots[1]),
        dec.stats
    );
    assert!(secs(&slots[1]) > 30.0, "slot 2 speech too short");
    assert!(rms(&slots[1]) > 0.01, "slot 2 speech is silent");

    // Oracle: dsd-fme's synthesis of the same audio (`-fs -w`), reduced
    // to log10 RMS per 20 ms frame (`fixtures/dmr_441850_dsd_slot2_env
    // .txt`, 1991 frames). mbelib synthesises unvoiced bands from random
    // noise, so waveforms never match sample-for-sample; the speech
    // envelope and the voiced/silent pattern must. Measured when this
    // test was written: correlation 0.876, frame agreement 0.945.
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/dmr_441850_dsd_slot2_env.txt");
    let oracle: Vec<f64> = std::fs::read_to_string(&p)
        .unwrap()
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect();
    let ours: Vec<f64> = slots[1]
        .as_chunks::<160>()
        .0
        .iter()
        .map(|c| (rms(c) + 1e-5).log10())
        .collect();
    let n = oracle.len().min(ours.len());
    let (a, b) = (&oracle[..n], &ours[..n]);
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let (ma, mb) = (mean(a), mean(b));
    let cov: f64 = a.iter().zip(b).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f64 = a.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f64 = b.iter().map(|y| (y - mb).powi(2)).sum();
    let corr = cov / (va * vb).sqrt();
    let agree = a
        .iter()
        .zip(b)
        .filter(|(x, y)| (**x > -2.5) == (**y > -2.5))
        .count() as f64
        / n as f64;
    eprintln!("envelope corr {corr:.3}, voiced-frame agreement {agree:.3}");
    assert!(
        corr > 0.8,
        "speech envelope diverges from dsd-fme: {corr:.3}"
    );
    assert!(
        agree > 0.9,
        "voiced/silent pattern diverges from dsd-fme: {agree:.3}"
    );
}
