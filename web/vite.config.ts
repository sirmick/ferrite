import tailwindcss from '@tailwindcss/vite';
import { sveltekit } from '@sveltejs/kit/vite';
import topLevelAwait from 'vite-plugin-top-level-await';
import wasm from 'vite-plugin-wasm';
import { defineConfig } from 'vite';
import { coopCoep } from './src/lib/vite/coop-coep';

const ferritedTarget = process.env.FERRITED_URL ?? 'http://127.0.0.1:8088';

export default defineConfig({
  plugins: [coopCoep(), wasm(), topLevelAwait(), tailwindcss(), sveltekit()],
  worker: {
    format: 'es',
    plugins: () => [wasm(), topLevelAwait()],
  },
  server: {
    port: 5173,
    strictPort: true,
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
