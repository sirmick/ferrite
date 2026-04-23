import tailwindcss from '@tailwindcss/vite';
import basicSsl from '@vitejs/plugin-basic-ssl';
import { sveltekit } from '@sveltejs/kit/vite';
import topLevelAwait from 'vite-plugin-top-level-await';
import wasm from 'vite-plugin-wasm';
import { defineConfig } from 'vite';
import { coopCoep } from './src/lib/vite/coop-coep';

const ferritedTarget = process.env.FERRITED_URL ?? 'http://127.0.0.1:10001';

export default defineConfig({
  // `basicSsl()` flips the dev server to HTTPS with an auto-generated
  // self-signed cert. Gated on `FERRITE_HTTPS=1` so plain-HTTP workflows
  // (tunnels, CI preview) keep working; flip it on for LAN / mobile
  // testing where browsers refuse `AudioWorklet` + `SharedArrayBuffer`
  // without a secure context. Expect a one-time "not private" warning
  // per browser profile — bypass once, then the cert sticks for the
  // session. If you want zero warnings, switch to mkcert instead.
  plugins: [
    coopCoep(),
    wasm(),
    topLevelAwait(),
    tailwindcss(),
    sveltekit(),
    ...(process.env.FERRITE_HTTPS === '1' ? [basicSsl()] : []),
  ],
  worker: {
    format: 'es',
    plugins: () => [wasm(), topLevelAwait()],
  },
  resolve: {
    alias: {
      // The wasm-bindgen JS shim for `ferrite-blocks` (now linking
      // liquid-dsp + wasi-libc on wasm32-unknown-unknown) does a
      // bare `import * as _ from "wasi_snapshot_preview1"` to
      // satisfy WASI imports referenced by wasi-libc's stdio path.
      // We don't run a WASI runtime in the browser, so map the bare
      // module to our no-op stubs — see src/lib/wasm/wasi-stubs.ts
      // for why no-ops are correct (the syscalls in question are
      // never reached by the DSP code paths).
      wasi_snapshot_preview1: '/src/lib/wasm/wasi-stubs.ts',
    },
  },
  server: {
    port: 10000,
    strictPort: true,
    // Dev-only: accept any `Host` header so tunnelled / proxied names
    // (dev.homezone.be, *.trycloudflare.com, LAN IPs, …) reach the
    // dev server. Vite production builds aren't served by this config,
    // so there's no attack surface to widen.
    allowedHosts: true,
    proxy: {
      '/api': {
        target: ferritedTarget,
        changeOrigin: false,
      },
      '/ws': {
        target: ferritedTarget,
        ws: true,
        changeOrigin: false,
      },
    },
  },
});
