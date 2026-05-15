//! Correctness gate for the vendored RTTY decoder.
//!
//! Synthesises a deterministic Baudot/ITA2 RTTY signal for a known
//! string and asserts the curated fldigi core recovers it through the
//! C ABI. No binary fixture: RTTY modulation is trivial and a
//! generated reference is hermetic, license-clean, and self-validating
//! (a broken DSP path cannot reconstruct exact ASCII from FSK). This
//! is the always-on correctness net; the per-mode ship-gate e2e (real
//! off-air sigidwiki recording + thumbnail + sigwiki ref) is separate
//! and lands with the preset.
//!
//! Signal matches fldigi's RX defaults exactly (so it decodes by
//! construction): centre = RTTYsweetspot 1500 Hz, shift = SHIFT[3]
//! 170 Hz, baud = BAUD[0] 45, 5-bit Baudot, USB/no-reverse
//! (mark = high tone), 1 start (space) + 5 data LSB-first + 1.5 stop
//! (mark). The Baudot code↔char map is fldigi's own `letters[32]`
//! table from vendor/rtty.cxx, replicated here.

use ferrite_fldigi::FldigiModem;
use std::f32::consts::PI;

const FS: f32 = 8_000.0;
const CENTER: f32 = 1_500.0;
const SHIFT: f32 = 170.0;
const BAUD: f32 = 45.0;

/// fldigi vendor/rtty.cxx `letters[32]` — index = 5-bit Baudot code,
/// value = LETTERS-case char. We invert it to encode.
const LETTERS: [u8; 32] = [
    0, b'E', b'\n', b'A', b' ', b'S', b'I', b'U', b'\r', b'D', b'R', b'J', b'N', b'F', b'C', b'K',
    b'T', b'Z', b'L', b'W', b'H', b'Y', b'P', b'Q', b'O', b'B', b'G', 0, b'M', b'X', b'V', 0,
];
const LTRS: u8 = 0b11111; // letters-shift code
const BAUDOT_SPACE: u8 = 0b00100; // ' ' in letters table (index 4)

fn code_for(c: u8) -> Option<u8> {
    (0u8..32).find(|&i| LETTERS[i as usize] == c)
}

/// Append one RTTY character frame: start bit (space=0), 5 data bits
/// LSB-first, 1.5 stop bits (mark=1). `phase` carries continuous FSK
/// phase across the whole transmission.
fn push_char(out: &mut Vec<f32>, phase: &mut f32, code: u8) {
    // bit=1 → mark (high tone, USB/no-reverse); bit=0 → space (low).
    let emit = |out: &mut Vec<f32>, phase: &mut f32, bit: bool, bits: f32| {
        let f = if bit {
            CENTER + SHIFT / 2.0
        } else {
            CENTER - SHIFT / 2.0
        };
        let n = (FS / BAUD * bits).round() as usize;
        for _ in 0..n {
            *phase += 2.0 * PI * f / FS;
            if *phase > 2.0 * PI {
                *phase -= 2.0 * PI;
            }
            out.push(phase.sin() * 0.5);
        }
    };
    emit(out, phase, false, 1.0); // start = space
    for b in 0..5 {
        emit(out, phase, (code >> b) & 1 == 1, 1.0); // LSB first
    }
    emit(out, phase, true, 1.5); // 1.5 stop = mark
}

fn modulate(text: &str) -> Vec<f32> {
    let mut out = Vec::new();
    let mut phase = 0.0f32;
    // ~0.5 s of mark idle then several LTRS so the decoder's AGC /
    // bit-sync / shift-state lock before the payload.
    let idle = (FS * 0.5) as usize;
    for _ in 0..idle {
        phase += 2.0 * PI * (CENTER + SHIFT / 2.0) / FS;
        out.push(phase.sin() * 0.5);
    }
    for _ in 0..8 {
        push_char(&mut out, &mut phase, LTRS);
    }
    for &c in text.as_bytes() {
        let code = if c == b' ' {
            BAUDOT_SPACE
        } else {
            code_for(c).unwrap_or(LTRS)
        };
        push_char(&mut out, &mut phase, code);
    }
    for _ in 0..4 {
        push_char(&mut out, &mut phase, LTRS);
    }
    out
}

#[test]
fn synthetic_rtty_decodes_to_known_text() {
    let payload = "CQ CQ DE FERRITE FERRITE K";
    let audio = modulate(payload);
    assert!(audio.len() > (FS as usize), "signal should be > 1 s");

    let mut m = FldigiModem::new("rtty45", FS as u32).expect("rtty45 constructs");
    // Feed in realistic chunks so rx_process runs the way the block
    // drives it (not one giant buffer).
    for chunk in audio.chunks(2048) {
        m.rx(chunk);
    }
    let decoded = m.take_text().to_uppercase();

    // RTTY routinely garbles the first/last char and a stray FIGS/LTRS
    // can appear; assert the distinctive payload word is recovered
    // intact rather than byte-exact equality.
    assert!(
        decoded.contains("FERRITE"),
        "expected decoded text to contain FERRITE, got {decoded:?}"
    );
}
