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
