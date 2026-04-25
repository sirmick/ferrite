#!/usr/bin/env python3
"""Resample sigidwiki originals to 22 050 Hz mono 16-bit PCM.

Multimon's decoders (and the `analyze-*-wav` binaries that wrap them)
expect that exact format. The originals on sigidwiki are a mix of
mono/stereo MP3 at 24 / 44.1 kHz and one stereo 32-bit WAV at 44.1
kHz; this script reads each one, downmixes to mono, normalises, and
writes the result under `22050_mono/`. Idempotent — already-converted
files are skipped.

Run from this directory; needs `soundfile` and `scipy`. The repo's
default Python doesn't have either, so a one-shot venv is the
typical path::

    python3 -m venv /tmp/ferrite-venv
    /tmp/ferrite-venv/bin/pip install soundfile scipy numpy
    /tmp/ferrite-venv/bin/python convert.py
"""

import os
from math import gcd

import numpy as np
import soundfile as sf
from scipy.signal import resample_poly

SRC_DIR = os.path.dirname(os.path.abspath(__file__))
OUT_DIR = os.path.join(SRC_DIR, "22050_mono")
TARGET_RATE = 22_050

os.makedirs(OUT_DIR, exist_ok=True)
for fn in sorted(os.listdir(SRC_DIR)):
    src = os.path.join(SRC_DIR, fn)
    if not os.path.isfile(src):
        continue
    if not fn.lower().endswith((".mp3", ".wav", ".ogg", ".flac")):
        continue
    out = os.path.join(OUT_DIR, os.path.splitext(fn)[0] + ".wav")
    if os.path.exists(out):
        print(f"  skip {fn}: already converted")
        continue
    try:
        data, sr = sf.read(src, always_2d=False)
    except Exception as e:
        print(f"  fail {fn}: {e}")
        continue
    if data.ndim > 1:
        data = data.mean(axis=1)
    data = data.astype(np.float64)
    peak = float(np.max(np.abs(data))) or 1.0
    data /= peak
    if sr != TARGET_RATE:
        g = gcd(int(sr), TARGET_RATE)
        data = resample_poly(data, TARGET_RATE // g, sr // g)
    pcm = np.clip(data * 32_767, -32_768, 32_767).astype(np.int16)
    sf.write(out, pcm, TARGET_RATE, subtype="PCM_16")
    print(f"  wrote {fn}: {sr} Hz -> {TARGET_RATE} Hz mono i16, {len(pcm) / TARGET_RATE:.1f}s")
