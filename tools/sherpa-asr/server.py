#!/usr/bin/env python3
"""Ferrite sherpa-onnx ASR sidecar — server-side (heavy) transcription.

A purpose-built streaming WebSocket service so the node-side `SherpaTranscribe`
block stays a thin client. Deliberately *not* sherpa-onnx's own WS server: we
own a dead-simple protocol and emit the exact segment shape Ferrite's
`ui:transcribe` store expects, so no glue is needed on the Rust side.

Protocol (one WS connection per transcribing block):
  client -> server  : text  "{"sample_rate": <hz>}"  -> set the input rate
                              (sent once up front; the server resamples to
                              16 kHz, so the block forwards audio untouched)
                      binary frames = raw little-endian float32 PCM @ <hz>
                      text  "{"reset":true}"  -> drop the in-flight stream
  server -> client  : text lines, one JSON object each:
      {"text": "...", "final": false}              # rolling partial
      {"text": "...", "final": true,               # endpointed segment
       "t0": <sec>, "t1": <sec>}

Endpointing (silence-gated) segments the audio the way radio PTT bursts
already arrive, so each transmission becomes one `final` segment.

Env:
  SHERPA_ASR_MODEL_DIR   model dir (tokens.txt + encoder/decoder/joiner .onnx)
  SHERPA_ASR_PORT        listen port (default 10003)
  SHERPA_ASR_HOST        bind host  (default 127.0.0.1)
  SHERPA_ASR_THREADS     decode threads (default 2)
  SHERPA_ASR_INT8        "1" (default) → use *.int8.onnx, else fp32
"""

import asyncio
import json
import os
import glob

import numpy as np
import sherpa_onnx
import websockets


def _pick(model_dir: str, stem: str, int8: bool) -> str:
    """Find encoder/decoder/joiner, preferring int8 when asked."""
    pats = ([f"{stem}*.int8.onnx"] if int8 else []) + [f"{stem}*.onnx"]
    for p in pats:
        hits = sorted(g for g in glob.glob(os.path.join(model_dir, p)) if ".int8." in g or not int8)
        if hits:
            return hits[0]
    # Fallback: any matching onnx
    hits = sorted(glob.glob(os.path.join(model_dir, f"{stem}*.onnx")))
    if not hits:
        raise FileNotFoundError(f"no {stem}*.onnx in {model_dir}")
    return hits[0]


def build_recognizer() -> sherpa_onnx.OnlineRecognizer:
    model_dir = os.environ.get("SHERPA_ASR_MODEL_DIR")
    if not model_dir or not os.path.isdir(model_dir):
        raise SystemExit(f"SHERPA_ASR_MODEL_DIR not set or missing: {model_dir!r}")
    int8 = os.environ.get("SHERPA_ASR_INT8", "1") != "0"
    threads = int(os.environ.get("SHERPA_ASR_THREADS", "2"))
    return sherpa_onnx.OnlineRecognizer.from_transducer(
        tokens=os.path.join(model_dir, "tokens.txt"),
        encoder=_pick(model_dir, "encoder", int8),
        decoder=_pick(model_dir, "decoder", int8),
        joiner=_pick(model_dir, "joiner", int8),
        num_threads=threads,
        provider="cpu",
        # Silence-gated segmentation — one transmission ≈ one final segment.
        enable_endpoint_detection=True,
        rule1_min_trailing_silence=2.4,
        rule2_min_trailing_silence=1.2,
        rule3_min_utterance_length=300,
    )


SAMPLE_RATE = 16000


def _resample_16k(samples: np.ndarray, src_rate: int) -> np.ndarray:
    if src_rate == SAMPLE_RATE:
        return samples
    n = int(len(samples) * SAMPLE_RATE / src_rate)
    if n <= 0:
        return np.empty(0, dtype=np.float32)
    xp = np.arange(len(samples))
    x = np.linspace(0, len(samples), n, endpoint=False)
    return np.interp(x, xp, samples).astype(np.float32)


async def handle(ws, recognizer: sherpa_onnx.OnlineRecognizer):
    stream = recognizer.create_stream()
    seg_start = 0.0  # seconds of audio consumed at the current segment's start
    consumed = 0.0
    last_partial = ""
    src_rate = SAMPLE_RATE  # overridden by the {"sample_rate":…} handshake
    async for msg in ws:
        if isinstance(msg, (bytes, bytearray)):
            samples = np.frombuffer(msg, dtype=np.float32)
            if samples.size == 0:
                continue
            samples = _resample_16k(samples, src_rate)
            if samples.size == 0:
                continue
            consumed += samples.size / SAMPLE_RATE
            stream.accept_waveform(SAMPLE_RATE, samples)
            while recognizer.is_ready(stream):
                recognizer.decode_stream(stream)
            text = recognizer.get_result(stream).strip()
            if recognizer.is_endpoint(stream):
                if text:
                    await ws.send(json.dumps({"text": text, "final": True,
                                              "t0": round(seg_start, 2),
                                              "t1": round(consumed, 2)}))
                recognizer.reset(stream)
                seg_start = consumed
                last_partial = ""
            elif text and text != last_partial:
                last_partial = text
                await ws.send(json.dumps({"text": text, "final": False}))
        else:
            try:
                obj = json.loads(msg)
            except Exception:
                continue
            if "sample_rate" in obj:
                try:
                    src_rate = max(1, int(obj["sample_rate"]))
                except (TypeError, ValueError):
                    pass
            if obj.get("reset"):
                recognizer.reset(stream)
                seg_start = consumed
                last_partial = ""


async def main():
    recognizer = build_recognizer()
    host = os.environ.get("SHERPA_ASR_HOST", "127.0.0.1")
    port = int(os.environ.get("SHERPA_ASR_PORT", "10003"))

    async def _conn(ws):
        try:
            await handle(ws, recognizer)
        except websockets.ConnectionClosed:
            pass

    async with websockets.serve(_conn, host, port, max_size=None):
        print(f"sherpa-asr listening ws://{host}:{port}  model={os.environ.get('SHERPA_ASR_MODEL_DIR')}",
              flush=True)
        await asyncio.Future()


if __name__ == "__main__":
    asyncio.run(main())
