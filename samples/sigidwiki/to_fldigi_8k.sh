#!/usr/bin/env bash
# Convert the fldigi-mode sigidwiki MP3 originals to 8 kHz mono 16-bit
# PCM WAV under 8000_mono/ — the exact format FldigiDemod requires
# (it pins sample_rate_hz to 8000). Companion to convert.py, which
# targets 22 050 Hz for the multimon decoders.
#
# Uses GStreamer (gst-launch-1.0) since the default Python here lacks
# soundfile/scipy and there is no ffmpeg/sox. Idempotent: skips files
# already converted. The 8000_mono/ WAVs are committed so the e2e
# tests are self-contained in CI (same rationale as 22050_mono/).
#
#   cd samples/sigidwiki && ./to_fldigi_8k.sh
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p 8000_mono

# Only the fldigi-decodable modes (one MP3 each on sigidwiki).
FILES=(
  "RTTY_170Hz_45.45bd"
  "Olivia_8-500"
  "Contestia_8-500"
  "NAVTEX_SITOR-B"
  "MT63-1000L"
  "DominoEX_16Bd"
  "THROB4"
  "BPSK31"
)

for base in "${FILES[@]}"; do
  src="${base}.mp3"
  out="8000_mono/${base}.wav"
  if [[ ! -f "$src" ]]; then
    echo "  miss $src — skipping"
    continue
  fi
  if [[ -f "$out" ]]; then
    echo "  skip $base: already converted"
    continue
  fi
  gst-launch-1.0 -q filesrc location="$src" ! decodebin ! audioconvert \
    ! audioresample ! "audio/x-raw,format=S16LE,channels=1,rate=8000" \
    ! wavenc ! filesink location="$out"
  echo "  wrote $base -> $out"
done
