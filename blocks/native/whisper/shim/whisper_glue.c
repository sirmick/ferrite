// Thin C ABI over whisper.cpp for the Emscripten/WASM transcription
// worker. Mirrors the fldigi shim approach: a tiny stable surface the
// JS side (web/src/lib/transcribe/whisperEngine.ts) marshals buffers
// across, keeping all libwhisper/ggml complexity behind three exports.
//
// Pinned API: whisper.cpp as vendored at
// blocks/native/whisper/vendor/whisper.cpp (see README for the exact
// commit). If you bump the submodule and the build breaks, the most
// likely culprits are the VAD params struct and
// whisper_full_get_segment_no_speech_prob — both are recent additions.
//
// JSON the worker expects from wsp_transcribe:
//   {"segments":[{"t0":F,"t1":F,"text":"...","avgLogprob":F,
//                 "noSpeechProb":F,"tokens":[{"text":"..","p":F}, ...]}]}

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "whisper.h"

static struct whisper_context *g_ctx = NULL;
static int g_vad_ready = 0;

// Hold the Silero VAD model bytes for the life of the process — recent
// whisper.cpp loads VAD from a file path or buffer per call; we keep
// the buffer and feed it via whisper_full_params each transcribe.
static unsigned char *g_vad_buf = NULL;
static int g_vad_len = 0;

// Returns 1 on success. model/vad are raw ggml bytes copied from the JS
// heap; vadPtr may be 0 (no Silero shipped → energy VAD fallback in JS).
int wsp_init(const unsigned char *model, int model_len,
             const unsigned char *vad, int vad_len) {
  if (g_ctx) {
    whisper_free(g_ctx);
    g_ctx = NULL;
  }
  struct whisper_context_params cparams = whisper_context_default_params();
  // Emscripten build has no GPU; keep it explicit.
  cparams.use_gpu = false;
  g_ctx = whisper_init_from_buffer_with_params((void *)model, (size_t)model_len, cparams);
  if (!g_ctx) return 0;

  // whisper's built-in Silero VAD only loads from a filesystem path
  // (whisper_full_params.vad_model_path); this build is FILESYSTEM=0,
  // and the JS-side EnergyVadSegmenter already gates utterances before
  // they reach here, so the in-whisper VAD pass stays disabled. The
  // vad buffer args are accepted (stable ABI) but unused for now.
  (void)vad;
  (void)vad_len;
  g_vad_ready = 0;
  return 1;
}

int wsp_vad_available(void) { return g_vad_ready; }

// Accuracy-over-latency, exactly the operator's stated workflow: beam
// search, temperature fallback, single-segment greedy off. `prompt`
// biases vocabulary toward recently-heard ham callsigns / Q-codes.
//
// Returns a malloc'd, NUL-terminated JSON string the caller must free
// (whisperEngine.ts calls M._free on it). NULL on failure.
char *wsp_transcribe(const float *pcm, int n_samples, const char *prompt) {
  if (!g_ctx || n_samples <= 0) return NULL;

  struct whisper_full_params p =
      whisper_full_default_params(WHISPER_SAMPLING_BEAM_SEARCH);
  p.print_realtime = false;
  p.print_progress = false;
  p.print_timestamps = false;
  p.print_special = false;
  p.translate = false;
  p.language = "en";
  p.n_threads = 4;                 // emscripten pthread pool (see build.sh)
  p.beam_search.beam_size = 6;     // patient, accurate
  p.greedy.best_of = 5;
  p.temperature_inc = 0.2f;        // fallback on low-confidence decodes
  p.suppress_blank = true;
  p.token_timestamps = true;
  p.no_context = false;
  if (prompt && prompt[0]) p.initial_prompt = prompt;

  // Silero VAD inside whisper.cpp when the model shipped — far better
  // than energy gating on noisy SSB. The JS side also VAD-gates, so
  // this is a second, model-grade pass over the already-segmented
  // utterance (cheap; tightens boundaries / rejects false fires).
  if (g_vad_ready) {
    p.vad = true;
    p.vad_model_path = NULL;       // buffer path set below if supported
  }

  if (whisper_full(g_ctx, p, pcm, n_samples) != 0) return NULL;

  const int n_seg = whisper_full_n_segments(g_ctx);

  // Generously sized growable buffer — segments are short.
  size_t cap = 4096;
  char *out = (char *)malloc(cap);
  if (!out) return NULL;
  size_t len = 0;

#define APPEND(...)                                                     \
  do {                                                                  \
    int need = snprintf(out + len, cap - len, __VA_ARGS__);             \
    if (need < 0) { free(out); return NULL; }                           \
    while ((size_t)need >= cap - len) {                                 \
      cap *= 2;                                                         \
      char *grown = (char *)realloc(out, cap);                          \
      if (!grown) { free(out); return NULL; }                           \
      out = grown;                                                      \
      need = snprintf(out + len, cap - len, __VA_ARGS__);               \
      if (need < 0) { free(out); return NULL; }                         \
    }                                                                   \
    len += (size_t)need;                                                \
  } while (0)

  APPEND("{\"segments\":[");
  for (int s = 0; s < n_seg; ++s) {
    if (s) APPEND(",");
    const float t0 = whisper_full_get_segment_t0(g_ctx, s) / 100.0f;
    const float t1 = whisper_full_get_segment_t1(g_ctx, s) / 100.0f;
    const char *txt = whisper_full_get_segment_text(g_ctx, s);
    float no_speech = 0.0f;
#ifdef WHISPER_HAS_NO_SPEECH_PROB
    no_speech = whisper_full_get_segment_no_speech_prob(g_ctx, s);
#endif
    APPEND("{\"t0\":%.2f,\"t1\":%.2f,\"text\":", t0, t1);

    // JSON-escape the segment text.
    APPEND("\"");
    for (const char *c = txt ? txt : ""; *c; ++c) {
      if (*c == '"' || *c == '\\') APPEND("\\%c", *c);
      else if ((unsigned char)*c < 0x20) APPEND(" ");
      else APPEND("%c", *c);
    }
    APPEND("\",\"noSpeechProb\":%.4f,\"tokens\":[", no_speech);

    const int n_tok = whisper_full_n_tokens(g_ctx, s);
    double logsum = 0.0;
    int counted = 0;
    for (int t = 0; t < n_tok; ++t) {
      const char *tt = whisper_full_get_token_text(g_ctx, s, t);
      // Skip special tokens like [_BEG_], [_TT_..]: they start with '['.
      if (tt && tt[0] == '[') continue;
      const float pp = whisper_full_get_token_p(g_ctx, s, t);
      if (counted) APPEND(",");
      APPEND("{\"text\":\"");
      for (const char *c = tt ? tt : ""; *c; ++c) {
        if (*c == '"' || *c == '\\') APPEND("\\%c", *c);
        else if ((unsigned char)*c < 0x20) APPEND(" ");
        else APPEND("%c", *c);
      }
      APPEND("\",\"p\":%.4f}", pp);
      if (pp > 1e-6f) { logsum += log((double)pp); counted++; }
    }
    const double avg = counted ? logsum / counted : -10.0;
    APPEND("],\"avgLogprob\":%.4f}", avg);
  }
  APPEND("]}");
#undef APPEND
  return out;
}
