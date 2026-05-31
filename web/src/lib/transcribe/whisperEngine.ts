// whisper.cpp WASM boundary.
//
// Lazy-loads the Emscripten module emitted by
// blocks/native/whisper/emscripten/build.sh. Until that artifact is
// vendored+compiled (`pnpm wasm:build:whisper`) the dynamic import
// throws and `load()` rejects with EngineUnavailableError — the Worker
// surfaces that as the panel's `unavailable` state and keeps the audio
// path 100% intact (the VoiceTranscribe block is pure passthrough).
// This is the same "inert until the toolchain runs" stance as the
// fldigi bridge.
//
// The model ggml is fetched once and kept in the Cache API so a
// "leave it running" session pays the download (small.en-q5 ≈ 180 MB)
// exactly once across reloads.

/** Whisper's required input sample rate. The shared Rust core
 *  (`WasmTranscriber`) resamples to this before inference; re-exported
 *  here for any consumer that needs the constant. */
export const WHISPER_RATE = 16_000;

export class EngineUnavailableError extends Error {
  constructor(detail: string) {
    super(`whisper.cpp WASM not built — ${detail}`);
    this.name = 'EngineUnavailableError';
  }
}

export interface WhisperModelDef {
  readonly id: string;
  readonly label: string;
  /** ggml model URL. Same-origin /models/ by default so it's cacheable
   *  and works offline once primed; override via the panel later. */
  readonly url: string;
  /** Approx download size, for the panel's first-load notice. */
  readonly approxMB: number;
}

/** MODELS[0] is the default the worker loads. tiny.en-q5 is first
 *  because it's the model committed/fetched into static/models — the
 *  feature works out of the box. small.en-q5 (best on noisy SSB) and
 *  base.en-q5 are selectable once their .bin is dropped in
 *  web/static/models/ (see blocks/native/whisper/README.md). */
export const MODELS: ReadonlyArray<WhisperModelDef> = [
  {
    id: 'tiny.en-q5',
    label: 'tiny.en (q5 · fast, bundled default)',
    url: '/models/ggml-tiny.en-q5_1.bin',
    approxMB: 32,
  },
  {
    id: 'small.en-q5',
    label: 'small.en (q5 · best for noisy SSB)',
    url: '/models/ggml-small.en-q5_1.bin',
    approxMB: 182,
  },
  {
    // tinydiarize fine-tune: emits a per-segment speaker-turn flag,
    // which `TranscriptView` renders as a divider between speakers.
    // English-only `small` size; drop the .bin into web/static/models
    // (see blocks/native/whisper/README.md).
    id: 'small.en-tdrz',
    label: 'small.en-tdrz (speaker-turn detection)',
    url: '/models/ggml-small.en-tdrz.bin',
    approxMB: 465,
  },
  {
    id: 'base.en-q5',
    label: 'base.en (q5 · faster, lighter)',
    url: '/models/ggml-base.en-q5_1.bin',
    approxMB: 57,
  },
];

const VAD_MODEL_URL = '/models/ggml-silero-v5.1.2.bin';
const MODEL_CACHE = 'ferrite-whisper-models';

export interface WhisperToken {
  readonly text: string;
  readonly p: number;
}
export interface WhisperSegment {
  readonly t0: number;
  readonly t1: number;
  readonly text: string;
  readonly tokens: WhisperToken[];
  readonly avgLogprob: number;
  readonly noSpeechProb: number;
  /** tinydiarize speaker-turn flag — true when whisper.cpp's
   *  `whisper_full_get_segment_speaker_turn_next` fires for this
   *  segment. Always false on non-tdrz models. */
  readonly speakerTurn?: boolean;
}
export interface WhisperResult {
  readonly segments: WhisperSegment[];
}

/** Cache-first fetch of a (large) model blob. */
async function fetchModel(url: string): Promise<Uint8Array> {
  try {
    const cache = await caches.open(MODEL_CACHE);
    const hit = await cache.match(url);
    if (hit) return new Uint8Array(await hit.arrayBuffer());
    const resp = await fetch(url);
    if (!resp.ok) throw new Error(`model fetch ${url}: HTTP ${resp.status}`);
    await cache.put(url, resp.clone());
    return new Uint8Array(await resp.arrayBuffer());
  } catch (e) {
    // No Cache API (some worker contexts) — fall back to a plain fetch.
    const resp = await fetch(url);
    if (!resp.ok) throw new Error(`model fetch ${url}: HTTP ${resp.status} (${String(e)})`);
    return new Uint8Array(await resp.arrayBuffer());
  }
}

// Explicit shape of the Emscripten module's glue ABI. NOT derived from
// `import('whisper.mjs')`: once the artifact is built that file carries
// `// @ts-nocheck`, so its inferred type collapses to `{}` and shadows
// the whisper.d.ts ambient. Declaring it here (the fldigi "treat the
// Module as a known opaque shape + cast" approach) keeps the typecheck
// stable whether or not the generated .mjs is present.
interface WhisperModule {
  _wsp_init(m: number, ml: number, v: number, vl: number): number;
  _wsp_transcribe(p: number, n: number, prompt: number): number;
  _wsp_vad_available(): number;
  _malloc(bytes: number): number;
  _free(ptr: number): void;
  HEAPU8: Uint8Array;
  HEAPF32: Float32Array;
  stringToUTF8(s: string, ptr: number, max: number): void;
  lengthBytesUTF8(s: string): number;
  UTF8ToString(ptr: number): string;
}

type WhisperFactory = (arg?: Record<string, unknown>) => Promise<WhisperModule>;

export class WhisperEngine {
  private mod: WhisperModule | undefined;
  private ready = false;
  vadAvailable = false;

  /** Load the WASM module + ggml model (+ Silero VAD if present).
   *  Throws EngineUnavailableError when the artifact isn't built. */
  async load(model: WhisperModelDef): Promise<void> {
    let factory: WhisperFactory;
    try {
      // Literal specifier so vite bundles the Emscripten ESM + emits
      // its `whisper.wasm` sidecar. The generated .mjs is @ts-nocheck,
      // so cast its default export to the known factory shape.
      const m = await import('../wasm/whisper/whisper.mjs');
      factory = m.default as unknown as WhisperFactory;
    } catch (e) {
      throw new EngineUnavailableError(
        `module import failed (run pnpm wasm:build:whisper): ${String(e)}`,
      );
    }
    this.mod = await factory({});

    const [modelBytes, vadBytes] = await Promise.all([
      fetchModel(model.url),
      fetchModel(VAD_MODEL_URL).catch(() => new Uint8Array(0)),
    ]);

    const M = this.mod;
    const mPtr = M._malloc(modelBytes.length);
    M.HEAPU8.set(modelBytes, mPtr);
    let vPtr = 0;
    if (vadBytes.length > 0) {
      vPtr = M._malloc(vadBytes.length);
      M.HEAPU8.set(vadBytes, vPtr);
    }
    const ok = M._wsp_init(mPtr, modelBytes.length, vPtr, vadBytes.length);
    M._free(mPtr);
    if (vPtr) M._free(vPtr);
    if (ok !== 1) throw new Error('whisper context init failed');
    this.ready = true;
    this.vadAvailable = M._wsp_vad_available() === 1;
  }

  get loaded(): boolean {
    return this.ready;
  }

  /** Transcribe one mono 16 kHz utterance. `prompt` biases the decode
   *  toward recently-heard ham vocabulary (rolling initial_prompt). */
  transcribe(pcm16k: Float32Array, prompt: string): WhisperResult {
    const M = this.mod;
    if (!M || !this.ready) throw new Error('whisper engine not loaded');
    const pcmPtr = M._malloc(pcm16k.length * 4);
    M.HEAPF32.set(pcm16k, pcmPtr / 4);
    const promptLen = M.lengthBytesUTF8(prompt) + 1;
    const promptPtr = M._malloc(promptLen);
    M.stringToUTF8(prompt, promptPtr, promptLen);

    const jsonPtr = M._wsp_transcribe(pcmPtr, pcm16k.length, promptPtr);
    M._free(pcmPtr);
    M._free(promptPtr);
    // A genuine "no speech" result is still valid JSON (`{segments:[]}`);
    // a null pointer or unparseable blob is an engine/ABI fault — throw
    // so the worker flips to `error` instead of silently looking like
    // permanent silence.
    if (!jsonPtr) throw new Error('whisper: _wsp_transcribe returned null');
    const json = M.UTF8ToString(jsonPtr);
    M._free(jsonPtr);
    try {
      return JSON.parse(json) as WhisperResult;
    } catch (e) {
      throw new Error(`whisper: result not JSON (${String(e)})`);
    }
  }
}
