#!/usr/bin/env python3
"""Throwaway test client for the sherpa-asr sidecar: stream a wav, print results."""
import asyncio, json, sys, wave
import numpy as np
import websockets

def read_wav_16k(path):
    w = wave.open(path, "rb")
    sr, ch = w.getframerate(), w.getnchannels()
    d = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(np.float32) / 32768.0
    if ch > 1:
        d = d.reshape(-1, ch).mean(axis=1)
    if sr != 16000:
        n = int(len(d) * 16000 / sr)
        d = np.interp(np.linspace(0, len(d), n, endpoint=False), np.arange(len(d)), d).astype(np.float32)
    return d

async def main(path, url="ws://127.0.0.1:10003"):
    pcm = read_wav_16k(path)
    finals = []
    async with websockets.connect(url, max_size=None) as ws:
        async def recv():
            async for m in ws:
                o = json.loads(m)
                if o.get("final"):
                    finals.append(o)
                    print(f"  [final {o['t0']:.1f}-{o['t1']:.1f}s] {o['text']}")
        rt = asyncio.create_task(recv())
        chunk = 1600  # 100 ms
        for i in range(0, len(pcm), chunk):
            await ws.send(pcm[i:i+chunk].tobytes())
        await asyncio.sleep(2.0)  # let trailing endpoint flush
        await ws.close()
        try:
            await asyncio.wait_for(rt, timeout=2)
        except Exception:
            pass
    print(f"\n=== {len(finals)} final segments ===")

if __name__ == "__main__":
    asyncio.run(main(sys.argv[1]))
