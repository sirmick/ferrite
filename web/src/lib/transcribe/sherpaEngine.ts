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

// The two Emscripten sidecars, imported as URLs so vite emits them and we
// can resolve them via `Module.locateFile` (the generated loader fetches
// `sherpa-onnx-wasm-main-asr.wasm` and the preloaded `.data` by basename).
import sherpaWasmUrl from '../wasm/sherpa/sherpa-onnx-wasm-main-asr.wasm?url';
import sherpaDataUrl from '../wasm/sherpa/sherpa-onnx-wasm-main-asr.data?url';

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

export class SherpaEngine {
  private mod: SherpaModule | undefined;
  private recognizer: OnlineRecognizer | undefined;

  get loaded(): boolean {
    return this.recognizer !== undefined;
  }

  /** Instantiate the WASM module + create the streaming recognizer.
   *  Throws `EngineUnavailableError` when the artifact isn't built (the
   *  dynamic import fails) so the Worker can degrade gracefully. */
  async load(): Promise<void> {
    let factory: SherpaModuleFactory;
    let createOnlineRecognizer: (mod: SherpaModule) => OnlineRecognizer;
    try {
      // Literal specifiers so vite bundles the Emscripten ESM + the API
      // glue. Both carry `// @ts-nocheck` once built, so cast to the
      // shapes we use.
      const modFactory = await import('../wasm/sherpa/sherpa-onnx-wasm-main-asr.js');
      const api = await import('../wasm/sherpa/sherpa-onnx-asr.js');
      factory = modFactory.default as unknown as SherpaModuleFactory;
      createOnlineRecognizer = (
        api as unknown as {
          createOnlineRecognizer: (mod: SherpaModule) => OnlineRecognizer;
        }
      ).createOnlineRecognizer;
    } catch (e) {
      throw new EngineUnavailableError(
        `sherpa-onnx wasm import failed (run pnpm wasm:build:sherpa): ${String(e)}`,
      );
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
  }

  /** Transcribe one closed 16 kHz mono clip → one segment. Drives the
   *  online recognizer to completion on the clip (with a short zero tail
   *  so the endpointer flushes the final words), reads the text, and
   *  resets the stream. Returns the whisper-compatible shape the Rust
   *  core's `ingestInflight` expects. */
  transcribe(pcm16k: Float32Array): SherpaResult {
    const rec = this.recognizer;
    if (!rec) throw new Error('SherpaEngine.transcribe: not loaded');
    const stream = rec.createStream();
    try {
      stream.acceptWaveform(SHERPA_RATE, pcm16k);
      // Tail padding flushes the last frames through the feature window
      // so the final word isn't clipped (mirrors the streaming demo).
      stream.acceptWaveform(SHERPA_RATE, new Float32Array(Math.round(SHERPA_RATE * 0.3)));
      while (rec.isReady(stream)) rec.decode(stream);
      const text = rec.getResult(stream).text.trim();
      const t1 = pcm16k.length / SHERPA_RATE;
      return {
        segments: text ? [{ t0: 0, t1, text, tokens: [], avgLogprob: 0, noSpeechProb: 0 }] : [],
      };
    } finally {
      stream.free();
    }
  }

  free(): void {
    this.recognizer?.free();
    this.recognizer = undefined;
    this.mod = undefined;
  }
}
