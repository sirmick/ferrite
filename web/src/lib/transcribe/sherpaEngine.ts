// sherpa-onnx (streaming Zipformer) inference engine — the light/browser
// speech-to-text engine, replacing whisper.cpp in the browser.
//
// This wraps sherpa-onnx's WASM ASR module + its JS API glue (both built
// by blocks/native/sherpa/emscripten/build.sh into web/src/lib/wasm/sherpa/).
// The model (streaming-zipformer-en-20M, int8) is *baked into the .data*
// file at build time, so there's no runtime model fetch — `load()` just
// instantiates the module and creates the recognizer.
//
// The shape mirrors the old `WhisperEngine` so the transcription Worker's
// per-clip contract is unchanged: the shared Rust core (`WasmTranscriber`)
// still resamples to 16 kHz, VAD-gates into clips, and ham-cleans the
// results — we only swap *which* engine transcribes each clip. sherpa's
// recognizer is an *online* (streaming) model; we drive it per VAD clip
// (acceptWaveform → decode-until-ready → getResult → reset), which is the
// same "finalize a closed utterance" call the streaming demo uses for its
// tail flush. Live partials (a rolling-prediction view) are a later
// enhancement that feeds sherpa continuously instead of per-clip.
//
// ONE STREAM FOR THE WHOLE SESSION. The streaming zipformer keeps encoder
// state across `reset()` (cached attention/conv context plus a running
// average that never stops accumulating), and that state is what makes it
// intelligible: a *fresh* stream per clip is cold every time and, on the
// TTS round-trip corpus, returned nothing at all for most utterances
// (7/38 content words over eight clips). One stream reset at each VAD
// endpoint keeps the context warm — same corpus, 25/44, good from the
// second clip on. Only the first utterance after `load()` is cold, and
// no synthetic warm-up (silence, noise, formant babble, a real speech
// clip) fixes that; it's the model's own cold start.

/** Thrown by `load()` when the sherpa-onnx WASM artifact isn't built
 *  (the dynamic import fails) so the Worker can degrade gracefully —
 *  audio is unaffected (the VoiceTranscribe tap is pure passthrough),
 *  the panel just shows "engine not built". Standalone (no whisper
 *  dependency) — the browser tier is sherpa now. */
export class EngineUnavailableError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'EngineUnavailableError';
  }
}

/** sherpa's model runs at 16 kHz; the Rust core hands us 16 kHz clips. */
export const SHERPA_RATE = 16_000;

/** Minimal shape of one recognized result (matches the old WhisperSegment
 *  enough for the Rust core's `ingestInflight`, which only reads
 *  t0/t1/text/tokens/avgLogprob/noSpeechProb). */
export interface SherpaSegment {
  readonly t0: number;
  readonly t1: number;
  readonly text: string;
  readonly tokens: { text: string; p: number }[];
  readonly avgLogprob: number;
  readonly noSpeechProb: number;
}
export interface SherpaResult {
  readonly segments: SherpaSegment[];
}

// The Emscripten module, its API glue, and the two sidecars (`.wasm` +
// preloaded `.data`) are gitignored build outputs — a tree without emsdk
// legitimately has none of them. `import.meta.glob` is the one import
// form that is *resilient to absence*: present → vite bundles + hashes
// them exactly like a literal import; absent → the map is `{}` and the
// production build still succeeds (a literal `import x from '…?url'`
// fails `vite build` with "Could not resolve"). `load()` turns an empty
// map into `EngineUnavailableError` so the Worker degrades gracefully.
const sherpaModules = import.meta.glob<{ default: SherpaModuleFactory }>(
  '../wasm/sherpa/sherpa-onnx-wasm-main-asr.js',
);
const sherpaApis = import.meta.glob<{
  createOnlineRecognizer: (mod: SherpaModule) => OnlineRecognizer;
}>('../wasm/sherpa/sherpa-onnx-asr.js');
const sherpaAssetUrls = import.meta.glob<string>(
  '../wasm/sherpa/sherpa-onnx-wasm-main-asr.{wasm,data}',
  {
    query: '?url',
    import: 'default',
  },
);

/** The single loader a glob map holds, or `undefined` when the artifact
 *  isn't built (each glob matches exactly one basename). */
function only<T>(map: Record<string, () => Promise<T>>): (() => Promise<T>) | undefined {
  const keys = Object.keys(map);
  return keys.length === 1 ? map[keys[0]] : undefined;
}

/** Opaque Emscripten module handle (the generated glue is `@ts-nocheck`,
 *  so we treat it as an unknown bag the API glue indexes into). */
type SherpaModule = Record<string, unknown>;
type SherpaModuleFactory = (arg?: Record<string, unknown>) => Promise<SherpaModule>;

/** The online recognizer + stream the API glue hands back. */
interface OnlineStream {
  acceptWaveform(sampleRate: number, samples: Float32Array): void;
  free(): void;
}
interface OnlineRecognizer {
  createStream(): OnlineStream;
  isReady(stream: OnlineStream): boolean;
  decode(stream: OnlineStream): void;
  isEndpoint(stream: OnlineStream): boolean;
  reset(stream: OnlineStream): void;
  getResult(stream: OnlineStream): { text: string };
  free(): void;
}

/** Zero tail appended to every clip. The recognizer only decodes a
 *  chunk once chunk + lookahead frames (32 + 7 at 10 ms) are *all*
 *  available, so without ≥ 0.45 s of padding the clip's last frames
 *  would sit undecoded until the next clip arrived — and surface at the
 *  head of *that* result. 0.5 s covers it. Don't go long: runs of
 *  silence through the live stream degrade the carried state (1 s
 *  tails cost the next three clips on the round-trip corpus). */
export const CLIP_TAIL_S = 0.5;

export class SherpaEngine {
  private mod: SherpaModule | undefined;
  private recognizer: OnlineRecognizer | undefined;
  /** The session-long stream (see the header) — created with the
   *  recognizer, reset after every clip, freed with it. */
  private stream: OnlineStream | undefined;

  get loaded(): boolean {
    return this.recognizer !== undefined;
  }

  /** Instantiate the WASM module + create the streaming recognizer.
   *  Throws `EngineUnavailableError` when the artifact isn't built (the
   *  dynamic import fails) so the Worker can degrade gracefully. */
  async load(): Promise<void> {
    const loadModule = only(sherpaModules);
    const loadApi = only(sherpaApis);
    const wasmKey = Object.keys(sherpaAssetUrls).find((k) => k.endsWith('.wasm'));
    const dataKey = Object.keys(sherpaAssetUrls).find((k) => k.endsWith('.data'));
    if (!loadModule || !loadApi || !wasmKey || !dataKey) {
      throw new EngineUnavailableError(
        'sherpa-onnx wasm not built (run pnpm wasm:build:sherpa — needs emsdk + vendored sherpa-onnx + model)',
      );
    }
    let factory: SherpaModuleFactory;
    let createOnlineRecognizer: (mod: SherpaModule) => OnlineRecognizer;
    let sherpaWasmUrl: string;
    let sherpaDataUrl: string;
    try {
      [{ default: factory }, { createOnlineRecognizer }, sherpaWasmUrl, sherpaDataUrl] =
        await Promise.all([
          loadModule(),
          loadApi(),
          sherpaAssetUrls[wasmKey](),
          sherpaAssetUrls[dataKey](),
        ]);
    } catch (e) {
      throw new EngineUnavailableError(`sherpa-onnx wasm import failed: ${String(e)}`);
    }

    // `locateFile` resolves the `.wasm` and the preloaded `.data` to the
    // vite-emitted URLs (the loader asks for them by basename).
    this.mod = await factory({
      locateFile: (path: string) => {
        if (path.endsWith('.wasm')) return sherpaWasmUrl;
        if (path.endsWith('.data')) return sherpaDataUrl;
        return path;
      },
    });
    // No config arg → the recognizer uses the baked model (encoder.onnx /
    // decoder.onnx / joiner.onnx / tokens.txt preloaded from the .data).
    this.recognizer = createOnlineRecognizer(this.mod);
    this.stream = this.recognizer.createStream();
  }

  /** Transcribe one closed 16 kHz mono clip → one segment. Feeds the
   *  clip to the session stream (with a short zero tail so the last
   *  words flush through the feature window), decodes to completion,
   *  reads the text, and resets the stream at this endpoint — which
   *  clears the result but keeps the encoder context warm for the next
   *  clip. Returns the whisper-compatible shape the Rust core's
   *  `ingestInflight` expects. */
  transcribe(pcm16k: Float32Array): SherpaResult {
    const rec = this.recognizer;
    const stream = this.stream;
    if (!rec || !stream) throw new Error('SherpaEngine.transcribe: not loaded');
    try {
      stream.acceptWaveform(SHERPA_RATE, pcm16k);
      stream.acceptWaveform(SHERPA_RATE, new Float32Array(Math.round(SHERPA_RATE * CLIP_TAIL_S)));
      while (rec.isReady(stream)) rec.decode(stream);
      const text = rec.getResult(stream).text.trim();
      const t1 = pcm16k.length / SHERPA_RATE;
      return {
        segments: text ? [{ t0: 0, t1, text, tokens: [], avgLogprob: 0, noSpeechProb: 0 }] : [],
      };
    } finally {
      // The VAD clip boundary *is* the endpoint: start the next clip's
      // result from scratch without discarding the stream's history.
      rec.reset(stream);
    }
  }

  free(): void {
    this.stream?.free();
    this.stream = undefined;
    this.recognizer?.free();
    this.recognizer = undefined;
    this.mod = undefined;
  }
}
