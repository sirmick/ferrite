//! Compact text renderings — for terminals and for agents that can't see
//! the UI. The UI draws the structured data instead.

use crate::envelope::Envelope;
use crate::spectrogram::Spectrogram;
use crate::spectrum::SpectrumResult;

const RAMP: &[u8] = b" .:-=+*#%@";

/// Character-cell spectrogram, time downward, at most `width` × `height`.
#[must_use]
pub fn spectrogram(sg: &Spectrogram, width: usize, height: usize) -> String {
    let (cw, ch) = (sg.cols.min(width).max(1), sg.rows.min(height).max(1));
    let mut out = String::new();
    out.push_str(&format!(
        "{:.4} MHz {:>w$} {:.4} MHz  ({:.0} s, {:.1} Hz/col)\n",
        sg.f0_hz / 1e6,
        "",
        (sg.f0_hz + sg.df_hz * (sg.cols as f64 - 1.0)) / 1e6,
        sg.dt_s * sg.rows as f64,
        sg.df_hz * sg.cols as f64 / cw as f64,
        w = cw.saturating_sub(26)
    ));
    for r in 0..ch {
        let (r0, r1) = (
            r * sg.rows / ch,
            ((r + 1) * sg.rows / ch).max(r * sg.rows / ch + 1),
        );
        for c in 0..cw {
            let (c0, c1) = (
                c * sg.cols / cw,
                ((c + 1) * sg.cols / cw).max(c * sg.cols / cw + 1),
            );
            let mut v = 0u8;
            for rr in r0..r1 {
                for cc in c0..c1 {
                    v = v.max(sg.cells[rr * sg.cols + cc]);
                }
            }
            let i = usize::from(v) * (RAMP.len() - 1) / 255;
            out.push(char::from(RAMP[i]));
        }
        out.push('\n');
    }
    out
}

/// `#` on / `.` off per window, wrapped at `width` with time labels.
#[must_use]
pub fn envelope(e: &Envelope, width: usize) -> String {
    let marks: Vec<char> = e
        .power_db
        .iter()
        .map(|&v| if v > e.threshold_db { '#' } else { '.' })
        .collect();
    let mut out = String::new();
    for (i, line) in marks.chunks(width.max(10)).enumerate() {
        let t = (i * width.max(10)) as f64 * e.window_s;
        out.push_str(&format!("{t:6.2}s "));
        out.extend(line.iter());
        out.push('\n');
    }
    out
}

/// One-line sparkline of the spectrum profile.
#[must_use]
pub fn spectrum(s: &SpectrumResult) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = s.profile.iter().map(|p| p[1]).fold(1.0, f64::max);
    s.profile
        .iter()
        .map(|p| BARS[((p[1].max(0.0) / max) * 7.0).round() as usize])
        .collect()
}

/// The inspect feature bundle + a small spectrogram.
#[must_use]
pub fn inspect(f: &crate::Features, sg: &Spectrogram) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let _ = writeln!(o, "{}", f.hint);
    let _ = writeln!(
        o,
        "centre {:.4} MHz ({:+.0} Hz)  occupied {:.0} Hz  peak {:.1} dBFS  power {:.1} dBFS  floor {:.1} dBFS/{:.0} Hz  SNR {:.1} dB",
        f.freq_hz / 1e6,
        f.offset_hz,
        f.occupied_hz,
        f.peak_dbfs,
        f.power_dbfs,
        f.floor_dbfs,
        f.res_hz,
        f.snr_db
    );
    let _ = writeln!(
        o,
        "duty {:.2}  keyed {}  swing {:.1} dB  bursts {} (on {:.0} ms{}{})  envelope cv {}{}",
        f.duty,
        f.keyed,
        f.swing_db,
        f.bursts.count,
        f.bursts.on_ms_median,
        f.bursts
            .gap_ms_median
            .map(|g| format!(", gap {g:.0} ms"))
            .unwrap_or_default(),
        f.bursts
            .period_s
            .map(|p| format!(", every {p:.3} s"))
            .unwrap_or_default(),
        f.envelope_cv
            .map_or_else(|| "–".to_string(), |c| format!("{c:.2}")),
        f.envelope_period_s
            .map(|p| format!(
                "  envelope period {:.1} ms (r={:.2})",
                p * 1e3,
                f.envelope_period_strength
            ))
            .unwrap_or_default(),
    );
    if !f.tones.is_empty() {
        let t: Vec<String> = f
            .tones
            .iter()
            .map(|t| {
                format!(
                    "{:+.0} Hz {:.0} dB {:.0}%",
                    t.freq_hz - f.freq_hz,
                    t.rel_db,
                    t.share * 100.0
                )
            })
            .collect();
        let _ = writeln!(
            o,
            "tones: {}{}",
            t.join(" | "),
            f.fsk_shift_hz
                .map(|s| format!("  → FSK shift {s:.0} Hz"))
                .unwrap_or_default()
        );
    }
    o.push_str(&spectrogram(sg, 64, 16));
    o
}
