import path from 'node:path';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  plugins: [sveltekit()],
  resolve: {
    alias: {
      // Mirror vite.config.ts — the wasm-bindgen-generated shim
      // for ferrite-blocks does `import * as _ from
      // "wasi_snapshot_preview1"` (because we link wasi-libc on the
      // wasm32 path for liquid-dsp's malloc / cexpf / etc.). Vitest
      // doesn't inherit vite.config.ts's resolve.alias, so we
      // duplicate it here so the test runner can load the WASM glue
      // for runner/runnerDsp/wbfmE2E suites.
      wasi_snapshot_preview1: path.resolve(__dirname, 'src/lib/wasm/wasi-stubs.ts'),
    },
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.{test,spec}.{ts,js}'],
    passWithNoTests: false,
  },
});
