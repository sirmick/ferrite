"""Readers for Ferrite's capture formats and their JSON sidecars.

Formats (see ``samples/README.md`` and the tee blocks in ``blocks/src``):

- **cf32 IQ** (``.cf32``, ``.iq``, ``.f32``): interleaved little-endian
  float32 I,Q — what the live tees write (``iq_record_tap.rs``: snapshot,
  wideband capture, ``record_path`` on ``src`` or a channelizer).
- **WAV IQ**: 16-bit stereo PCM, L = I, R = Q (``*_iq-s16.wav``) —
  normalised by 32768 exactly like the daemon's file source.
- **WAV audio**: 16-bit mono PCM (``audio_record_tap.rs`` → 48 kHz).
- **FFT u8** (``.bin``): ``frame_size``-byte ``LogMagU8`` rows, the
  waterfall bytes, quantised over [-160, 0] dBFS.

Sidecars are ``<file>.json`` (``foo.wav.json``) or ``<stem>.json``
(``foo.json`` — what the tees write); either is accepted, ``<file>.json``
first. Keys used here: ``sample_rate_hz``, ``center_freq_hz``,
``format``, ``kind``, ``frame_size`` and an optional free-form ``tags``
object (``antenna``, ``gain_db``, ``spot`` …) for ``compare`` /
``ladder`` / ``report``.
"""
from __future__ import annotations

import json
import re
import wave
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np

# LogMagU8's fixed quantisation window (blocks/src/log_mag_u8.rs).
LOGMAG_FLOOR_DBFS = -160.0
LOGMAG_CEIL_DBFS = 0.0

IQ_RAW_SUFFIXES = (".cf32", ".iq", ".f32")


def sidecar_path(path: Path) -> Path | None:
    """The sidecar for ``path``: ``<file>.json`` else ``<stem>.json``."""
    path = Path(path)
    for cand in (path.with_name(path.name + ".json"), path.with_suffix(".json")):
        if cand != path and cand.exists():
            return cand
    return None


def read_sidecar(path: Path) -> dict:
    side = sidecar_path(path)
    if side is None:
        return {}
    try:
        return json.loads(side.read_text())
    except json.JSONDecodeError as e:
        raise ValueError(f"sidecar {side} is not valid JSON: {e}") from e


def _wav_info(path: Path) -> tuple[int, int, int]:
    with wave.open(str(path), "rb") as w:
        return w.getframerate(), w.getnchannels(), w.getsampwidth()


def classify(path: Path, side: dict | None = None) -> str:
    """``iq`` / ``audio`` / ``fft`` from the sidecar, else the file."""
    path = Path(path)
    side = side if side is not None else read_sidecar(path)
    kind = side.get("kind")
    fmt = str(side.get("format", ""))
    if kind in ("iq", "audio", "fft"):
        return kind
    if "frame_size" in side or path.suffix == ".bin":
        return "fft"
    if fmt.endswith("-iq") or fmt in ("cf32", "f32"):
        return "iq"
    if path.suffix in IQ_RAW_SUFFIXES:
        return "iq"
    if path.suffix == ".wav":
        _, ch, _ = _wav_info(path)
        return "iq" if ch == 2 else "audio"
    n = path.name.lower()
    return "iq" if ("_iq-" in n or "-iq" in n) else "audio"


@dataclass
class Capture:
    """One capture on disk plus what its sidecar says about it."""

    path: Path
    kind: str
    rate_hz: float
    center_hz: float
    sidecar: dict = field(default_factory=dict)

    @property
    def tags(self) -> dict:
        """Free-form tags: sidecar ``tags`` merged over filename hints."""
        t = filename_tags(self.path)
        t.update(self.sidecar.get("tags") or {})
        for k in ("antenna", "gain_db", "spot"):
            if k in self.sidecar and k not in t:
                t[k] = self.sidecar[k]
        return t

    def iq(self) -> np.ndarray:
        return load_iq(self.path)

    def audio(self) -> np.ndarray:
        return load_audio(self.path)


_GAIN_RE = re.compile(r"(?:^|[_\-.])(?:gain|g)(\d+(?:\.\d+)?)(?=$|[_\-.])", re.I)
_ANT_RE = re.compile(r"(?:^|[_\-.])(?:ant|antenna)[_\-]?([A-Za-z0-9]+)(?=$|[_\-.])", re.I)


def filename_tags(path: Path) -> dict:
    """Tags from a filename: ``…_g40_…`` / ``…gain40…`` → ``gain_db``,
    ``…_antB_…`` → ``antenna``. Sidecar ``tags`` override these."""
    stem = Path(path).stem
    out: dict = {}
    if m := _GAIN_RE.search(stem):
        out["gain_db"] = float(m.group(1))
    if m := _ANT_RE.search(stem):
        out["antenna"] = m.group(1)
    return out


def open_capture(path: Path) -> Capture:
    path = Path(path)
    if not path.exists():
        raise FileNotFoundError(path)
    side = read_sidecar(path)
    kind = classify(path, side)
    rate = float(side.get("sample_rate_hz") or 0.0)
    if rate <= 0 and path.suffix == ".wav":
        rate = float(_wav_info(path)[0])
    center = float(side.get("center_freq_hz") or 0.0)
    if rate <= 0:
        raise ValueError(
            f"{path}: no sample rate (sidecar `sample_rate_hz` missing/0 and not a WAV)"
        )
    return Capture(path=path, kind=kind, rate_hz=rate, center_hz=center, sidecar=side)


def load_iq(path: Path) -> np.ndarray:
    """IQ samples as complex64 (cf32 raw, or s16 stereo WAV / 32768)."""
    path = Path(path)
    if path.suffix == ".wav":
        with wave.open(str(path), "rb") as w:
            ch, width = w.getnchannels(), w.getsampwidth()
            if ch != 2 or width != 2:
                raise ValueError(
                    f"{path}: IQ WAV must be 16-bit stereo (got {ch} ch, {8 * width}-bit)"
                )
            raw = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2")
        iq = raw.astype(np.float32) / np.float32(32768.0)
        return (iq[0::2] + 1j * iq[1::2]).astype(np.complex64)
    raw = np.fromfile(path, dtype="<f4")
    raw = raw[: len(raw) - len(raw) % 2]
    return (raw[0::2] + 1j * raw[1::2]).astype(np.complex64)


def load_audio(path: Path) -> np.ndarray:
    """Mono audio as float32 in [-1, 1) (16-bit PCM WAV)."""
    with wave.open(str(path), "rb") as w:
        if w.getsampwidth() != 2:
            raise ValueError(f"{path}: only 16-bit PCM audio is supported")
        ch = w.getnchannels()
        raw = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2")
    a = raw.astype(np.float32) / np.float32(32768.0)
    return a[0::ch] if ch > 1 else a


def load_fft_u8(path: Path) -> tuple[np.ndarray, dict]:
    """``LogMagU8`` rows (frames × frame_size, uint8) and the sidecar."""
    side = read_sidecar(path)
    frame = int(side.get("frame_size", 0))
    if frame <= 0:
        raise ValueError(f"{path}: sidecar needs a positive `frame_size`")
    raw = np.fromfile(path, dtype=np.uint8)
    n = len(raw) // frame
    if n == 0:
        raise ValueError(f"{path}: no complete {frame}-byte frame ({len(raw)} B)")
    return raw[: n * frame].reshape(n, frame), side


def byte_to_dbfs(b):
    return LOGMAG_FLOOR_DBFS + (np.asarray(b, dtype=np.float64) / 255.0) * (
        LOGMAG_CEIL_DBFS - LOGMAG_FLOOR_DBFS
    )


def find_captures(root: Path, kinds: tuple[str, ...] = ("iq",)) -> list[Capture]:
    """Every capture under ``root`` (recursive) of the given kinds, sorted."""
    out: list[Capture] = []
    for p in sorted(Path(root).rglob("*")):
        if not p.is_file() or p.suffix not in (".wav", ".bin", *IQ_RAW_SUFFIXES):
            continue
        try:
            c = open_capture(p)
        except (ValueError, wave.Error, EOFError):
            continue
        if c.kind in kinds:
            out.append(c)
    return out
