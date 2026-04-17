import tailwindcss from '@tailwindcss/vite';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';
import { coopCoep } from './src/lib/vite/coop-coep';

const ferritedTarget = process.env.FERRITED_URL ?? 'http://127.0.0.1:8088';

export default defineConfig({
  plugins: [coopCoep(), tailwindcss(), sveltekit()],
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
