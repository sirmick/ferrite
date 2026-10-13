//! `DmrDecode` — DMR (ETSI TS 102 361) framing and call metadata in pure
//! Rust, so it runs node-side *and* in the browser (WASM).
//!
//! Input is FM-discriminator audio (an `FmDemod` at 48 kHz, 5 kHz
//! deviation — the same tap `DsdHelper` uses); output is `events`
//! (`ui:dsd`) in the `DsdHelper` record shape, so the Journal and the
//! AI treat both identically:
//! `{"t":"dsd","event":"call"|"end"|"alias"|"sync","protocol":"DMR",
//!   "slot","tgt","src","cc","enc","text"}`.
//!
//! No vocoder: voice stays pluggable (`DsdHelper` today). This block is
//! everything *around* the AMBE frames:
//!
//! 1. **Symbols.** 4FSK at 4800 Bd. A short moving average, then a
//!    sliding correlation of the 24-symbol (48-bit) sync patterns at the
//!    symbol stride, at every sample phase. A hit gives the burst's
//!    timing and the ±3 levels (DC offset from mistuning cancels).
//! 2. **Bursts.** A repeater's outbound stream is continuous, 30 ms per
//!    timeslot (12-symbol CACH + 132-symbol burst), so once locked every
//!    burst is predicted at +1440 samples and re-anchored on each sync.
//!    Voice bursts B–F carry embedded signalling instead of sync and are
//!    sliced with the last levels.
//! 3. **Slot.** CACH TACT, Hamming(7,4); TC is the timeslot.
//! 4. **Data bursts.** Slot type (colour code, data type), then
//!    BPTC(196,96) — deinterleave ×181 mod 196, Hamming(15,11) rows and
//!    (13,9) columns — to the 96-bit full LC. Voice LC header and
//!    terminator are validated by RS(12,9) over GF(2⁸) after removing
//!    the CRC mask (0x969696 / 0x999999).
//! 5. **Voice superframes.** The EMB fragments of bursts B–E assemble
//!    the embedded LC: BPTC(128,77), Hamming(16,11,4) rows, column
//!    parity, 5-bit checksum (sum of the 9 LC bytes mod 31). That gives
//!    late entry, and the talker alias (FLCO 4–7).
//!
//! Every stage checks its FEC / CRC, so a bad slice drops a burst
//! instead of printing a wrong talkgroup. Golay(20,8) on the slot type
//! is not decoded; the colour code must match the lock's and the LC
//! RS/CRC catches the rest. Parameters are from the standard, cross-
//! checked against dsd-fme (`bptc.c`, `fec.c`, `rs-12-9.c`, `dmr_bs.c`).

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    drain_events_port, Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind,
    ParamSpec, Placement, PortSpec, PortType, ReconfigureScope, Work,
};
use crate::dsd_helper::DsdEvent;

/// DMR symbol rate.
pub const SYMBOL_RATE_HZ: f64 = 4_800.0;
/// Burst geometry, in symbols: 12 CACH + 132 burst per 30 ms timeslot.
const SLOT_SYMBOLS: usize = 144;
/// Index (within the 132-symbol burst) of the last sync symbol.
const SYNC_END: usize = 77;
/// Burst symbols after the sync.
const AFTER_SYNC: usize = 132 - SYNC_END - 1;
/// Correlation needed to call a sync (1.0 = perfect ±3 pattern).
const SYNC_THRESHOLD: f32 = 0.82;
/// Predicted bursts with no sync before the lock is dropped (≈ 1 s).
const MAX_MISSES: u32 = 32;
/// Sample tolerance for matching a sync to a predicted burst.
const ANCHOR_TOL: i64 = 4;

/// 48-bit sync words (ETSI TS 102 361-1 §9.1.1). Data syncs are the
/// bitwise inverse of the voice syncs, i.e. the negated symbol pattern.
const BS_VOICE: u64 = 0x755F_D7DF_75F7;
const MS_VOICE: u64 = 0x7F7D_5DD5_7DFD;

/// ±1 symbol signs (for the ±3 symbols) of a sync word, MSB first.
/// Dibit 01 → +3, 11 → −3 (sync words only use the outer symbols).
fn sync_signs(word: u64) -> [f32; 24] {
    let mut s = [0.0; 24];
    for (k, v) in s.iter_mut().enumerate() {
        let dibit = (word >> (46 - 2 * k)) & 0b11;
        *v = if dibit == 0b01 { 1.0 } else { -1.0 };
    }
    s
}

// ---------------------------------------------------------------- FEC

/// Single-error correction against a parity-check matrix given as
/// rows. Returns false when the syndrome matches no column.
fn hamming_correct(bits: &mut [u8], h: &[&[u8]]) -> bool {
    let n = bits.len();
    let syn: Vec<u8> = h
        .iter()
        .map(|row| {
            row.iter()
                .zip(bits.iter())
                .fold(0, |a, (&r, &b)| a ^ (r & b))
        })
        .collect();
    if syn.iter().all(|&s| s == 0) {
        return true;
    }
    for k in 0..n {
        if h.iter().zip(syn.iter()).all(|(row, &s)| row[k] == s) {
            bits[k] ^= 1;
            return true;
        }
    }
    false
}

const H_7_4: [&[u8]; 3] = [
    &[1, 1, 1, 0, 1, 0, 0],
    &[0, 1, 1, 1, 0, 1, 0],
    &[1, 1, 0, 1, 0, 0, 1],
];
const H_15_11: [&[u8]; 4] = [
    &[1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0],
    &[0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0],
    &[0, 0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0],
    &[1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 1],
];
const H_13_9: [&[u8]; 4] = [
    &[1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0],
    &[1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0],
    &[1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0],
    &[1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 1],
];
const H_16_11_4: [&[u8]; 5] = [
    &[1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0],
    &[0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0],
    &[0, 0, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0],
    &[1, 1, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 1, 0],
    &[1, 0, 1, 0, 0, 1, 1, 0, 1, 1, 1, 0, 0, 0, 0, 1],
];

/// GF(2⁸), primitive polynomial x⁸+x⁴+x³+x²+1 (0x11D).
struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

impl Gf {
    #[allow(clippy::needless_range_loop)]
    fn new() -> Self {
        let mut exp = [0_u8; 512];
        let mut log = [0_u8; 256];
        let mut x: u16 = 1;
        for i in 0..255 {
            #[allow(clippy::cast_possible_truncation)]
            {
                exp[i] = x as u8;
                log[x as usize] = i as u8;
            }
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11D;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        Self { exp, log }
    }
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[usize::from(self.log[usize::from(a)]) + usize::from(self.log[usize::from(b)])]
        }
    }
}

/// RS(12,9) syndromes (roots α¹..α³, codeword MSB-first) are all zero.
fn rs_12_9_ok(gf: &Gf, cw: &[u8; 12]) -> bool {
    (1..=3).all(|j| {
        let a = gf.exp[j];
        cw.iter().fold(0_u8, |s, &b| b ^ gf.mul(a, s)) == 0
    })
}

fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| c.iter().fold(0_u8, |a, &b| (a << 1) | (b & 1)))
        .collect()
}

#[allow(clippy::needless_range_loop)] // row/column matrix code reads clearer indexed
/// BPTC(196,96) → 96 data bits, or `None` if a row/column is
/// uncorrectable.
fn bptc_196_96(info: &[u8; 196]) -> Option<[u8; 96]> {
    // Deinterleave: deinterleaved bit i is received bit (i × 181) mod
    // 196. (The opposite direction leaves every Hamming row dirty —
    // checked on the 441.850 MHz fixture.)
    let mut d = [0_u8; 196];
    for (i, v) in d.iter_mut().enumerate() {
        *v = info[(i * 181) % 196];
    }
    let mut m = [[0_u8; 15]; 13];
    for (r, row) in m.iter_mut().enumerate() {
        row.copy_from_slice(&d[1 + r * 15..1 + r * 15 + 15]);
    }
    for _pass in 0..2 {
        for row in m.iter_mut().take(9) {
            hamming_correct(row, &H_15_11);
        }
        for c in 0..15 {
            let mut col: Vec<u8> = (0..13).map(|r| m[r][c]).collect();
            hamming_correct(&mut col, &H_13_9);
            for r in 0..13 {
                m[r][c] = col[r];
            }
        }
    }
    let rows_ok = m.iter().take(9).all(|row| {
        let mut r = *row;
        hamming_correct(&mut r, &H_15_11) && r == *row
    });
    if !rows_ok {
        return None;
    }
    let mut out = [0_u8; 96];
    let mut k = 0;
    for &b in &m[0][3..11] {
        out[k] = b;
        k += 1;
    }
    for row in &m[1..9] {
        for &b in &row[..11] {
            out[k] = b;
            k += 1;
        }
    }
    Some(out)
}

#[allow(clippy::needless_range_loop)]
/// Embedded LC: BPTC(128,77) from the four 32-bit fragments of voice
/// bursts B–E → the 72 LC bits, or `None` on FEC / checksum failure.
fn embedded_lc(frags: &[u8]) -> Option<[u8; 72]> {
    if frags.len() != 128 {
        return None;
    }
    let mut m = [[0_u8; 16]; 8];
    for (k, &b) in frags.iter().enumerate() {
        m[k % 8][k / 8] = b;
    }
    for row in m.iter_mut().take(7) {
        if !hamming_correct(row, &H_16_11_4) {
            return None;
        }
    }
    for c in 0..16 {
        let p = (0..7).fold(0, |a, r| a ^ m[r][c]);
        if p != m[7][c] {
            return None;
        }
    }
    let mut bits = [0_u8; 77];
    let mut k = 0;
    for row in &m[..2] {
        for &b in &row[..11] {
            bits[k] = b;
            k += 1;
        }
    }
    for row in &m[2..7] {
        for &b in &row[..10] {
            bits[k] = b;
            k += 1;
        }
    }
    for row in &m[2..7] {
        bits[k] = row[10];
        k += 1;
    }
    let bytes = bits_to_bytes(&bits[..72]);
    let sum: u32 = bytes.iter().map(|&b| u32::from(b)).sum();
    let cs = bits[72..77]
        .iter()
        .fold(0_u32, |a, &b| (a << 1) | u32::from(b));
    if sum % 31 != cs {
        return None;
    }
    let mut lc = [0_u8; 72];
    lc.copy_from_slice(&bits[..72]);
    Some(lc)
}

/// The fields of a full / embedded LC that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lc {
    pub flco: u8,
    pub fid: u8,
    pub service: u8,
    pub tgt: u32,
    pub src: u32,
}

impl Lc {
    fn from_bytes(b: &[u8]) -> Self {
        Self {
            flco: b[0] & 0x3F,
            fid: b[1],
            service: b[2],
            tgt: (u32::from(b[3]) << 16) | (u32::from(b[4]) << 8) | u32::from(b[5]),
            src: (u32::from(b[6]) << 16) | (u32::from(b[7]) << 8) | u32::from(b[8]),
        }
    }
    fn is_voice_call(&self) -> bool {
        self.flco == 0x00 || self.flco == 0x03
    }
    fn kind(&self) -> &'static str {
        if self.flco == 0x03 {
            "Private Call"
        } else {
            "Group Call"
        }
    }
}

// ---------------------------------------------------------- decoder

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncKind {
    Voice,
    Data,
}

#[derive(Debug, Clone, Copy)]
struct SyncHit {
    t: u64,
    kind: SyncKind,
    /// Raw correlation energy — peaks at the symbol centre; the
    /// normalised score saturates over several phases, so it can't
    /// pick the timing.
    energy: f32,
    hi: f32,
    lo: f32,
}

#[derive(Debug, Default)]
struct SlotState {
    /// Bursts of this slot since its last voice sync (0 = burst A).
    voice_burst: Option<u8>,
    frags: Vec<u8>,
    call: Option<(u32, u32, u64)>,
    alias_len: usize,
    alias_size: u8,
    alias_bits: Vec<u8>,
    alias_have: [bool; 4],
    alias_sent: String,
}

/// Streaming DMR decoder over discriminator samples. Pure, no I/O.
pub struct DmrDecoder {
    sps: usize,
    fs: f64,
    invert: bool,
    gf: Gf,
    // Moving average (sps/2 taps) over the raw input.
    ma: Vec<f32>,
    ma_sum: f32,
    ma_i: usize,
    // Filtered history ring, indexed by absolute sample number.
    hist: Vec<f32>,
    t: u64,
    signs: [[f32; 24]; 2],
    cand: Option<SyncHit>,
    syncs: Vec<SyncHit>,
    next: Option<u64>,
    misses: u32,
    hi: f32,
    lo: f32,
    cc: Option<u8>,
    last_slot: u8,
    slots: [SlotState; 2],
    /// Sample index of the last FEC-valid burst (for `announce`).
    last_valid: Option<u64>,
    events: Vec<DsdEvent>,
    /// Counters (for tests / diagnostics).
    pub stats: DmrStats,
    /// Print one line per burst to stderr (debugging aid).
    pub trace: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DmrStats {
    pub syncs: u64,
    pub bursts: u64,
    pub lc_ok: u64,
    pub lc_bad: u64,
    pub emb_ok: u64,
    pub emb_bad: u64,
}

const HIST: usize = 4_096;

impl DmrDecoder {
    #[must_use]
    pub fn new(sample_rate_hz: f64, invert: bool) -> Self {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let sps = ((sample_rate_hz / SYMBOL_RATE_HZ).round() as usize).max(2);
        let w = (sps / 2).max(1);
        Self {
            sps,
            fs: sample_rate_hz,
            invert,
            gf: Gf::new(),
            ma: vec![0.0; w],
            ma_sum: 0.0,
            ma_i: 0,
            hist: vec![0.0; HIST],
            t: 0,
            signs: [sync_signs(BS_VOICE), sync_signs(MS_VOICE)],
            cand: None,
            syncs: Vec::new(),
            next: None,
            misses: 0,
            hi: 0.0,
            lo: 0.0,
            cc: None,
            last_slot: 1,
            slots: [SlotState::default(), SlotState::default()],
            last_valid: None,
            events: Vec::new(),
            stats: DmrStats::default(),
            trace: false,
        }
    }

    /// Events produced since the last call.
    pub fn drain_events(&mut self) -> Vec<DsdEvent> {
        std::mem::take(&mut self.events)
    }

    #[inline]
    fn h(&self, t: u64) -> f32 {
        #[allow(clippy::cast_possible_truncation)]
        self.hist[(t as usize) & (HIST - 1)]
    }

    pub fn push(&mut self, samples: &[f32]) {
        for &x in samples {
            let x = if self.invert { -x } else { x };
            self.ma_sum += x - self.ma[self.ma_i];
            self.ma[self.ma_i] = x;
            self.ma_i = (self.ma_i + 1) % self.ma.len();
            #[allow(clippy::cast_precision_loss)]
            let y = self.ma_sum / self.ma.len() as f32;
            #[allow(clippy::cast_possible_truncation)]
            {
                self.hist[(self.t as usize) & (HIST - 1)] = y;
            }
            self.search_sync();
            self.maybe_decode();
            self.t += 1;
        }
    }

    fn search_sync(&mut self) {
        let span = (23 * self.sps) as u64;
        if self.t < span + (SLOT_SYMBOLS * self.sps) as u64 {
            return;
        }
        let mut v = [0.0_f32; 24];
        for (k, vk) in v.iter_mut().enumerate() {
            *vk = self.h(self.t - ((23 - k) * self.sps) as u64);
        }
        let mean = v.iter().sum::<f32>() / 24.0;
        let mag: f32 = v.iter().map(|x| (x - mean).abs()).sum();
        if mag <= 1e-6 {
            return;
        }
        for signs in &self.signs {
            let raw: f32 = signs
                .iter()
                .zip(v.iter())
                .map(|(s, x)| s * (x - mean))
                .sum::<f32>();
            let c = raw / mag;
            let (kind, score, sgn) = if c >= 0.0 {
                (SyncKind::Voice, c, 1.0)
            } else {
                (SyncKind::Data, -c, -1.0)
            };
            if score < SYNC_THRESHOLD {
                continue;
            }
            let (mut hs, mut hn, mut ls, mut ln) = (0.0, 0, 0.0, 0);
            for (s, x) in signs.iter().zip(v.iter()) {
                if s * sgn > 0.0 {
                    hs += x;
                    hn += 1;
                } else {
                    ls += x;
                    ln += 1;
                }
            }
            #[allow(clippy::cast_precision_loss)]
            let hit = SyncHit {
                t: self.t,
                kind,
                energy: raw.abs(),
                hi: hs / hn as f32,
                lo: ls / ln as f32,
            };
            if self.cand.is_none_or(|c| hit.energy > c.energy) {
                self.cand = Some(hit);
            }
        }
        if let Some(c) = self.cand {
            if self.t >= c.t + self.sps as u64 {
                self.cand = None;
                self.stats.syncs += 1;
                self.syncs.push(c);
                if self.syncs.len() > 16 {
                    self.syncs.remove(0);
                }
                if self.next.is_none() {
                    self.next = Some(c.t);
                    self.misses = 0;
                    self.hi = c.hi;
                    self.lo = c.lo;
                }
            }
        }
    }

    fn maybe_decode(&mut self) {
        let Some(next) = self.next else { return };
        // Wait until the burst's last symbol plus the sync finalise delay.
        if self.t != next + ((AFTER_SYNC + 2) * self.sps) as u64 {
            return;
        }
        let hit = self
            .syncs
            .iter()
            .filter(|s| {
                (i64::try_from(s.t).unwrap_or(0) - i64::try_from(next).unwrap_or(0)).abs()
                    <= ANCHOR_TOL
            })
            .max_by(|a, b| a.energy.total_cmp(&b.energy))
            .copied();
        let anchor = if let Some(h) = hit {
            self.hi = h.hi;
            self.lo = h.lo;
            self.misses = 0;
            h.t
        } else {
            self.misses += 1;
            next
        };
        self.syncs.retain(|s| s.t > anchor + ANCHOR_TOL as u64);
        self.decode_burst(anchor, hit.map(|h| h.kind));
        if self.misses > MAX_MISSES {
            self.next = None;
            self.cc = None;
            for s in &mut self.slots {
                s.voice_burst = None;
                s.frags.clear();
            }
            // Re-lock on the most recent sync, if any is still ahead.
            if let Some(s) = self.syncs.first().copied() {
                self.next = Some(s.t);
            }
        } else {
            self.next = Some(anchor + (SLOT_SYMBOLS * self.sps) as u64);
        }
    }

    /// Dibit at burst symbol `k` (may be negative for the CACH).
    fn dibit(&self, sync_end: u64, k: i64) -> u8 {
        let p = i64::try_from(sync_end).unwrap_or(0) + (k - SYNC_END as i64) * self.sps as i64;
        let v = self.h(u64::try_from(p).unwrap_or(0));
        let c = (self.hi + self.lo) / 2.0;
        let half = ((self.hi - self.lo) / 2.0).max(1e-6);
        let u = (v - c) / half;
        if u > 2.0 / 3.0 {
            0b01
        } else if u > 0.0 {
            0b00
        } else if u > -2.0 / 3.0 {
            0b10
        } else {
            0b11
        }
    }

    fn decode_burst(&mut self, sync_end: u64, kind: Option<SyncKind>) {
        self.stats.bursts += 1;
        // CACH → TACT → slot.
        const CACH_IL: [usize; 24] = [
            0, 7, 8, 9, 1, 10, 11, 12, 2, 13, 14, 15, 3, 16, 4, 17, 18, 19, 5, 20, 21, 22, 6, 23,
        ];
        let mut cach = [0_u8; 24];
        for i in 0..12 {
            let d = self.dibit(sync_end, i as i64 - 12);
            cach[CACH_IL[2 * i]] = (d >> 1) & 1;
            cach[CACH_IL[2 * i + 1]] = d & 1;
        }
        let mut tact = [0_u8; 7];
        tact.copy_from_slice(&cach[..7]);
        let tact_ok = hamming_correct(&mut tact, &H_7_4);
        let slot = if tact_ok { tact[1] } else { 1 - self.last_slot };
        self.last_slot = slot;
        let s = usize::from(slot);

        let mut bits = [0_u8; 264];
        for k in 0..132 {
            let d = self.dibit(sync_end, k as i64);
            bits[2 * k] = (d >> 1) & 1;
            bits[2 * k + 1] = d & 1;
        }

        if self.trace {
            let f = |r: std::ops::Range<usize>| {
                bits[r].iter().fold(0_u32, |a, &b| (a << 1) | u32::from(b))
            };
            eprintln!(
                "t={} slot={} tact_ok={} kind={:?} vb={:?} st_cc={} st_dt={} emb_cc={} emb_pi={} lcss={} hi={:.3} lo={:.3}",
                sync_end, slot + 1, tact_ok, kind, self.slots[s].voice_burst,
                f(98..102), f(102..106), f(108..112), f(112..113), f(113..115), self.hi, self.lo
            );
        }
        match kind {
            Some(SyncKind::Voice) => {
                self.slots[s].voice_burst = Some(0);
                self.slots[s].frags.clear();
            }
            Some(SyncKind::Data) => {
                self.slots[s].voice_burst = None;
                self.data_burst(slot, &bits);
            }
            None => {
                let vb = self.slots[s].voice_burst;
                if let Some(n) = vb.filter(|&n| n < 5) {
                    self.slots[s].voice_burst = Some(n + 1);
                    self.embedded(slot, &bits);
                } else {
                    self.slots[s].voice_burst = None;
                }
            }
        }
    }

    /// One `sync` record when DMR is first *verified* — after an LC or
    /// embedded LC passes its FEC/CRC, which noise can't forge (a bare
    /// 24-symbol sync correlation false-locks on an idle channel about
    /// once a second). Re-armed only after 60 s without a valid burst.
    fn announce(&mut self) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let quiet = (60.0 * self.fs) as u64;
        let fresh = self
            .last_valid
            .is_none_or(|t| self.t.saturating_sub(t) > quiet);
        self.last_valid = Some(self.t);
        if fresh {
            self.events.push(DsdEvent {
                t: "dsd",
                event: "sync",
                protocol: Some("DMR".into()),
                slot: None,
                tgt: None,
                src: None,
                cc: self.cc.map(u16::from),
                enc: None,
                text: format!(
                    "DMR verified (native decoder), colour code {}",
                    self.cc.map_or_else(|| "?".into(), |c| c.to_string())
                ),
            });
        }
    }

    fn data_burst(&mut self, slot: u8, bits: &[u8; 264]) {
        let st: Vec<u8> = bits[98..108]
            .iter()
            .chain(&bits[156..166])
            .copied()
            .collect();
        let cc = st[..4].iter().fold(0, |a, &b| (a << 1) | b);
        let dtype = st[4..8].iter().fold(0, |a, &b| (a << 1) | b);
        if self.cc.is_none() {
            self.cc = Some(cc);
        }
        if Some(cc) != self.cc {
            return;
        }
        if dtype != 1 && dtype != 2 {
            return;
        }
        let mut info = [0_u8; 196];
        info[..98].copy_from_slice(&bits[..98]);
        info[98..].copy_from_slice(&bits[166..264]);
        let Some(data) = bptc_196_96(&info) else {
            self.stats.lc_bad += 1;
            return;
        };
        let mut cw = [0_u8; 12];
        cw.copy_from_slice(&bits_to_bytes(&data));
        let mask = if dtype == 1 { 0x96 } else { 0x99 };
        for b in &mut cw[9..] {
            *b ^= mask;
        }
        if !rs_12_9_ok(&self.gf, &cw) {
            self.stats.lc_bad += 1;
            return;
        }
        self.stats.lc_ok += 1;
        self.announce();
        let lc = Lc::from_bytes(&cw);
        if !lc.is_voice_call() {
            return;
        }
        if dtype == 1 {
            self.call(slot, lc, "");
        } else {
            self.end_call(slot, lc);
        }
    }

    fn embedded(&mut self, slot: u8, bits: &[u8; 264]) {
        let emb: Vec<u8> = bits[108..116]
            .iter()
            .chain(&bits[148..156])
            .copied()
            .collect();
        let cc = emb[..4].iter().fold(0, |a, &b| (a << 1) | b);
        let lcss = (emb[5] << 1) | emb[6];
        if self.cc.is_some_and(|c| c != cc) {
            self.stats.emb_bad += 1;
            self.slots[usize::from(slot)].frags.clear();
            return;
        }
        let frag = &bits[116..148];
        let st = &mut self.slots[usize::from(slot)];
        match lcss {
            1 => {
                st.frags.clear();
                st.frags.extend_from_slice(frag);
            }
            3 if !st.frags.is_empty() => st.frags.extend_from_slice(frag),
            2 if !st.frags.is_empty() => {
                st.frags.extend_from_slice(frag);
                let frags = std::mem::take(&mut st.frags);
                match embedded_lc(&frags) {
                    Some(lc_bits) => {
                        self.stats.emb_ok += 1;
                        self.announce();
                        self.embedded_lc(slot, &lc_bits);
                    }
                    None => self.stats.emb_bad += 1,
                }
            }
            _ => st.frags.clear(),
        }
    }

    fn embedded_lc(&mut self, slot: u8, lc_bits: &[u8; 72]) {
        let bytes = bits_to_bytes(lc_bits);
        let lc = Lc::from_bytes(&bytes);
        if lc.is_voice_call() {
            self.call(slot, lc, " (late entry)");
        } else if (0x04..=0x07).contains(&lc.flco) && (lc.fid == 0 || lc.fid == 0x68) {
            self.alias_part(slot, lc.flco - 4, lc_bits);
        }
    }

    fn call(&mut self, slot: u8, lc: Lc, how: &str) {
        let st = &mut self.slots[usize::from(slot)];
        if st.call.is_some_and(|(t, s, _)| t == lc.tgt && s == lc.src) {
            return;
        }
        st.call = Some((lc.tgt, lc.src, self.t));
        st.alias_have = [false; 4];
        st.alias_bits.clear();
        st.alias_sent.clear();
        let enc = (lc.service & 0x40 != 0).then_some(true);
        self.events.push(DsdEvent {
            t: "dsd",
            event: "call",
            protocol: Some("DMR".into()),
            slot: Some(slot + 1),
            tgt: Some(lc.tgt.to_string()),
            src: Some(lc.src.to_string()),
            cc: self.cc.map(u16::from),
            enc,
            text: format!(
                "SLOT {} TGT={} SRC={} {}{}",
                slot + 1,
                lc.tgt,
                lc.src,
                lc.kind(),
                how
            ),
        });
    }

    fn end_call(&mut self, slot: u8, lc: Lc) {
        // Repeaters repeat the terminator through their hang time: only
        // the first one ends a call.
        let Some((_, _, t0)) = self.slots[usize::from(slot)].call.take() else {
            return;
        };
        #[allow(clippy::cast_precision_loss)]
        let secs = (self.t - t0) as f64 / self.fs;
        self.events.push(DsdEvent {
            t: "dsd",
            event: "end",
            protocol: Some("DMR".into()),
            slot: Some(slot + 1),
            tgt: Some(lc.tgt.to_string()),
            src: Some(lc.src.to_string()),
            cc: self.cc.map(u16::from),
            enc: None,
            text: format!(
                "SLOT {} call end TGT={} SRC={} ({secs:.1} s)",
                slot + 1,
                lc.tgt,
                lc.src
            ),
        });
    }

    /// Talker alias (ETSI TS 102 361-2 §7.2.18): header FLCO 4 carries
    /// format (2 bits) + length in characters (5 bits) + 48 data bits
    /// (49 for 7-bit); blocks FLCO 5–7 carry 56 data bits each.
    fn alias_part(&mut self, slot: u8, block: u8, lc: &[u8; 72]) {
        let st = &mut self.slots[usize::from(slot)];
        let b = usize::from(block);
        if b == 0 {
            let format = (lc[16] << 1) | lc[17];
            st.alias_len = lc[18..23].iter().fold(0, |a, &x| (a << 1) | usize::from(x));
            st.alias_size = match format {
                0 => 7,
                1 | 2 => 8,
                _ => 16,
            };
            let start = if st.alias_size == 7 { 23 } else { 24 };
            st.alias_bits.resize(49 + 3 * 56, 0);
            let n = 72 - start;
            st.alias_bits[..n].copy_from_slice(&lc[start..72]);
        } else {
            if st.alias_size == 0 {
                return;
            }
            st.alias_bits.resize(49 + 3 * 56, 0);
            let off = if st.alias_size == 7 { 49 } else { 48 } + (b - 1) * 56;
            st.alias_bits[off..off + 56].copy_from_slice(&lc[16..72]);
        }
        st.alias_have[b] = true;
        let size = usize::from(st.alias_size);
        let need_bits = st.alias_len * size;
        let have_bits = if st.alias_size == 7 { 49 } else { 48 }
            + 56 * (st.alias_have[1..].iter().take_while(|&&h| h).count());
        if !st.alias_have[0] || st.alias_len == 0 || have_bits < need_bits {
            return;
        }
        let chars: Vec<u32> = st.alias_bits[..need_bits]
            .chunks(size)
            .map(|c| c.iter().fold(0_u32, |a, &x| (a << 1) | u32::from(x)))
            .collect();
        let text: String = if size == 16 {
            char::decode_utf16(chars.iter().map(|&c| u16::try_from(c).unwrap_or(0xFFFD)))
                .map(|r| r.unwrap_or('\u{FFFD}'))
                .collect()
        } else {
            chars.iter().filter_map(|&c| char::from_u32(c)).collect()
        };
        let text = text
            .trim_matches(|c: char| c == '\0' || c.is_whitespace())
            .to_string();
        if text.is_empty() || text == st.alias_sent {
            return;
        }
        st.alias_sent.clone_from(&text);
        self.events.push(DsdEvent {
            t: "dsd",
            event: "alias",
            protocol: Some("DMR".into()),
            slot: Some(slot + 1),
            tgt: None,
            src: st.call.map(|(_, s, _)| s.to_string()),
            cc: self.cc.map(u16::from),
            enc: None,
            text,
        });
    }
}

// ------------------------------------------------------------ block

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DmrDecodeParams {
    /// Input (discriminator) sample rate; init's live rate wins.
    pub sample_rate_hz: f64,
    /// Invert the discriminator (receivers whose FM output is negated).
    pub invert: bool,
}

impl Default for DmrDecodeParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,
            invert: false,
        }
    }
}

pub struct DmrDecode {
    params: DmrDecodeParams,
    dec: DmrDecoder,
    events_out: Vec<u8>,
}

impl DmrDecode {
    pub fn new(params: DmrDecodeParams) -> Result<Self> {
        let dec = DmrDecoder::new(params.sample_rate_hz, params.invert);
        Ok(Self {
            params,
            dec,
            events_out: Vec::new(),
        })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for DmrDecode {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "DmrDecode",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[PortSpec {
                name: "events",
                port_type: PortType::Events,
            }],
            params: &[
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Input sample rate",
                    kind: ParamKind::Range {
                        min: 19_200.0,
                        max: 96_000.0,
                        step: 4_800.0,
                        default: 48_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Discriminator rate; a multiple of 4800 (48 kHz = 10 samples/symbol). The live input rate wins at init.",
                },
                ParamSpec {
                    key: "invert",
                    label: "Invert",
                    kind: ParamKind::Toggle { default: false },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Negate the discriminator. Only needed if a receiver inverts FM polarity: symptom is sync found but every LC failing (data and voice syncs are each other's inverse).",
                },
            ],
            ai_notes: "Native DMR (Tier II conventional / repeater) metadata decoder in pure Rust — runs node-side or in the browser. Feed a 48 kHz FmDemod (max_deviation_hz 5000) tap. Emits `ui:dsd` records: call (slot, TG, source ID, colour code, ENC flag; from the voice LC header or the embedded LC = late entry), end (terminator, with duration), alias (talker alias), sync. No voice audio — that is DsdHelper's job. FEC/CRC-checked, so it prints nothing rather than a wrong ID.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.params.sample_rate_hz).abs() > 1.0 {
                self.params.sample_rate_hz = rate;
                self.dec = DmrDecoder::new(rate, self.params.invert);
            }
        }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let consumed = match io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        {
            Some(src) => {
                self.dec.push(src);
                src.len()
            }
            None => 0,
        };
        for ev in self.dec.drain_events() {
            if let Ok(s) = serde_json::to_string(&ev) {
                tracing::info!(target: "decoder::dmr", "{}", ev.text);
                self.events_out.extend_from_slice(s.as_bytes());
                self.events_out.push(b'\n');
            }
        }
        let mut w = Work::new();
        w.consumed[0] = consumed;
        drain_events_port(io, &mut self.events_out, &mut w);
        Ok(w)
    }
}

impl BlockFactory for DmrDecode {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: DmrDecodeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(DmrDecode::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bits_to_bytes, embedded_lc, hamming_correct, rs_12_9_ok, sync_signs, Gf, BS_VOICE, H_15_11,
        H_7_4,
    };

    #[test]
    fn sync_words_use_outer_symbols() {
        let s = sync_signs(BS_VOICE);
        // 0x7 = 01 11 → +3, −3 ; 0x5 = 01 01 → +3, +3.
        assert_eq!(&s[..4], &[1.0, -1.0, 1.0, 1.0]);
    }

    #[test]
    fn hamming_corrects_one_bit() {
        // Zero is a codeword of every linear code.
        let mut z = [0_u8; 15];
        z[6] = 1;
        assert!(hamming_correct(&mut z, &H_15_11));
        assert_eq!(z, [0; 15]);
        let mut t = [0_u8; 7];
        t[2] = 1;
        assert!(hamming_correct(&mut t, &H_7_4));
        assert_eq!(t, [0; 7]);
    }

    #[test]
    fn rs_zero_codeword_and_single_error_detected() {
        let gf = Gf::new();
        let mut cw = [0_u8; 12];
        assert!(rs_12_9_ok(&gf, &cw));
        cw[4] = 0x5A;
        assert!(!rs_12_9_ok(&gf, &cw));
    }

    #[test]
    fn embedded_lc_rejects_garbage() {
        let bits: Vec<u8> = (0..128).map(|i| u8::from(i % 3 == 0)).collect();
        assert!(embedded_lc(&bits).is_none());
        assert_eq!(bits_to_bytes(&[1, 0, 0, 0, 0, 0, 0, 1]), vec![0x81]);
    }
}
