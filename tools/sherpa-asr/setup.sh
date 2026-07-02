#!/usr/bin/env bash
# Provision the sherpa-onnx ASR sidecar (server-side / heavy transcription).
#
# Creates a self-contained venv and downloads a streaming model. Idempotent —
# a no-op once present. Run it yourself, or let run.sh call it on first use.
#
#   SHERPA_ASR_MODEL_URL   model tarball to fetch (default: 20M streaming EN)
#
# Prints the model dir to stdout's last line so callers can capture it:
#   SHERPA_ASR_MODEL_DIR="$(tools/sherpa-asr/setup.sh | tail -1)"
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

VENV="$HERE/venv"
MODELS="$HERE/models"
# Default: the 20M int8 streaming Zipformer (~42 MB int8). Swap the URL for a
# larger/multilingual model on the heavy tier (e.g. a SenseVoice or a bigger
# streaming Zipformer); the sidecar auto-picks int8 encoder/decoder/joiner.
MODEL_URL="${SHERPA_ASR_MODEL_URL:-https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-20M-2023-02-17.tar.bz2}"
MODEL_NAME="$(basename "$MODEL_URL" .tar.bz2)"
MODEL_DIR="$MODELS/$MODEL_NAME"

if [ ! -x "$VENV/bin/python" ]; then
  echo "[sherpa-asr] creating venv" >&2
  python3 -m venv "$VENV"
fi
# shellcheck disable=SC1091
. "$VENV/bin/activate"
python -c "import sherpa_onnx, websockets, numpy" 2>/dev/null || {
  echo "[sherpa-asr] installing deps (sherpa-onnx, websockets, numpy)" >&2
  pip install --quiet --upgrade pip
  pip install --quiet sherpa-onnx websockets numpy
}

if [ ! -d "$MODEL_DIR" ]; then
  echo "[sherpa-asr] fetching model: $MODEL_NAME" >&2
  mkdir -p "$MODELS"
  tmp="$MODELS/.dl.tar.bz2"
  curl -fsSL --retry 3 -o "$tmp" "$MODEL_URL"
  tar xjf "$tmp" -C "$MODELS"
  rm -f "$tmp"
fi
echo "[sherpa-asr] ready: $MODEL_DIR" >&2
echo "$MODEL_DIR"
