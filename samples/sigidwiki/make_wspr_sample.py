#!/usr/bin/env python3
"""Derive a preset-playable 12 kHz WSPR audio fixture + thumbnail from
the canonical wsprsim reference I/Q the decoder e2e already trusts.

`WSPR_refSignal_0dB_iq-f32-375hz.iq` is one 120 s slot of complex
baseband at 375 Hz that `blocks/tests/wspr_e2e.rs` decodes to
"K1JT FN20 20". The WSPR preset chain, however, wants **12 kHz mono
USB audio** (it carries its own front-end: NCO down 1500 Hz, then
÷32 decimate to 375 Hz — see `blocks/src/wspr.rs`).

So we invert exactly that front-end:

    375 Hz complex  ──(×32 interpolate)──▶ 12 kHz complex
                    ──(mix +1500 Hz)────▶ analytic at audio centre
                    ──(take real part)──▶ 12 kHz mono USB audio

Feeding the result back through the block's front-end recovers the
original 375 Hz window, so the sample is guaranteed decodable and is
fully reproducible offline — no network, no fresh off-air capture.
License rides along from the GPL-3.0 reference.

Re-runnable; overwrites its three outputs. Run from this directory.
"""
from __future__ import annotations

import json
import wave
from pathlib import Path

import numpy as np
from PIL import Image
from scipy.signal import resample_poly

HERE = Path(__file__).resolve().parent
REF = HERE / "WSPR_refSignal_0dB_iq-f32-375hz.iq"
WAV = HERE / "WSPR_refsim_12k.wav"
SIDECAR = HERE / "WSPR_refsim_12k.wav.json"
THUMB = HERE / "images" / "WSPR_signal.png"

IQ_RATE = 375
AUDIO_RATE = 12_000
UP = AUDIO_RATE // IQ_RATE  # 32 — exactly the block's decimation
AUDIO_CENTER_HZ = 1_500.0  # WSPR_AUDIO_CENTER_HZ in wspr.rs


def load_ref() -> np.ndarray:
    """Interleaved little-endian f32 (I, Q, …). Q uses the wsprsim
    sign convention — negate on read, matching read_ref_iq() in
    blocks/tests/wspr_e2e.rs."""
    raw = np.fromfile(REF, dtype="<f4")
    i = raw[0::2].astype(np.float64)
    q = -raw[1::2].astype(np.float64)
    return i + 1j * q


def main() -> None:
    base = load_ref()  # 45000 complex @ 375 Hz = 120 s

    # ×32 polyphase interpolation, real and imaginary independently
    # (resample_poly is real-only). Linear/poly images land far
    # outside WSPR's ~6 Hz occupied band and the block's anti-alias
    # LPF removes them anyway.
    up_i = resample_poly(base.real, UP, 1)
    up_q = resample_poly(base.imag, UP, 1)
    analytic = up_i + 1j * up_q

    n = len(analytic)
    t = np.arange(n) / AUDIO_RATE
    # Mix the baseband up to the WSPR audio centre and take the real
    # part → honest single-sideband (USB) audio.
    audio = np.real(analytic * np.exp(2j * np.pi * AUDIO_CENTER_HZ * t))

    # −3 dBFS peak normalise (same headroom convention as the other
    # picker fixtures; the block re-normalises internally anyway).
    peak = float(np.max(np.abs(audio))) or 1.0
    audio = audio / peak * (10 ** (-3 / 20))

    # Self-check: re-apply the block's front-end (mix down 1500 Hz,
    # ÷32 decimate) and confirm we recover the original baseband.
    # This is the "is it actually decodable" gate, done offline.
    down = audio * np.exp(-2j * np.pi * AUDIO_CENTER_HZ * t)
    rec_i = resample_poly(down.real, 1, UP)
    rec_q = resample_poly(down.imag, 1, UP)
    rec = rec_i + 1j * rec_q
    m = min(len(rec), len(base))
    a = rec[:m] - rec[:m].mean()
    b = base[:m] - base[:m].mean()
    corr = abs(np.vdot(a, b)) / (np.linalg.norm(a) * np.linalg.norm(b))
    assert corr > 0.95, f"round-trip correlation {corr:.4f} — front-end inverse is wrong"

    pcm = np.clip(audio * 32_767, -32_768, 32_767).astype("<i2")
    with wave.open(str(WAV), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(AUDIO_RATE)
        w.writeframes(pcm.tobytes())

    # Thumbnail: a spectrogram of the audio, sized like FT8_signal.png
    # (~225×375 RGB). WSPR shows as a single faint near-horizontal
    # trace drifting slowly — distinctive at this zoom.
    win = 4096
    hop = win // 2
    cols = 1 + (len(audio) - win) // hop
    spec = np.empty((win // 2, cols), dtype=np.float64)
    w_han = np.hanning(win)
    for c in range(cols):
        seg = audio[c * hop : c * hop + win] * w_han
        spec[:, c] = np.abs(np.fft.rfft(seg)[: win // 2])
    spec = 20 * np.log10(spec + 1e-9)
    # Crop to the 1400–1600 Hz neighbourhood where WSPR sits.
    f = np.fft.rfftfreq(win, 1 / AUDIO_RATE)[: win // 2]
    band = (f >= 1400) & (f <= 1600)
    spec = spec[band]
    lo, hi = np.percentile(spec, 30), np.percentile(spec, 99.7)
    img = np.clip((spec - lo) / (hi - lo), 0, 1)
    img = np.flipud(img)
    # Simple blue→white colour ramp, resized to the FT8 thumb shape.
    rgb = np.stack([img**1.5, img**1.1, np.sqrt(img)], axis=-1)
    THUMB.parent.mkdir(parents=True, exist_ok=True)
    Image.fromarray((rgb * 255).astype(np.uint8)).resize(
        (225, 375), Image.BILINEAR
    ).save(THUMB)

    SIDECAR.write_text(
        json.dumps(
            {
                "name": "WSPR (wsprsim reference) — 12 kHz audio fixture",
                "file": WAV.name,
                "kind": "audio",
                "mode": "wspr",
                "format": "wav-pcm-s16-mono",
                "sample_rate_hz": AUDIO_RATE,
                "center_freq_hz": 0,
                "modulation": "ssb",
                "expected_decode": "K1JT FN20 20",
                "sigwiki_url": "https://www.sigidwiki.com/wiki/WSPR",
                "source": {
                    "origin": "derived from WSPR_refSignal_0dB_iq-f32-375hz.iq "
                    "(wsprsim 0 dB ref) by inverting the WsprDemod front-end; "
                    "see make_wspr_sample.py"
                },
                "license": {"name": "GPL-3.0"},
            },
            indent=2,
        )
        + "\n"
    )

    print(
        f"wrote {WAV.name} ({len(pcm) / AUDIO_RATE:.1f}s @ {AUDIO_RATE} Hz mono i16), "
        f"thumbnail {THUMB.relative_to(HERE.parent)}, sidecar — "
        f"front-end round-trip corr={corr:.4f}"
    )


if __name__ == "__main__":
    main()
