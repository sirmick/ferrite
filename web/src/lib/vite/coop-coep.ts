import type { Plugin } from 'vite';

/**
 * Sets the COOP/COEP headers required for cross-origin isolation
 * (SharedArrayBuffer access used by the AudioWorklet ring buffer).
 *
 * Dev-only: in production the same headers are set by ferrited's static
 * asset handler. See docs/06-build.md.
 */
export function coopCoep(): Plugin {
  const set = (res: { setHeader: (k: string, v: string) => void }) => {
    res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
    res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
  };
  return {
    name: 'ferrite:coop-coep',
    configureServer(server) {
      server.middlewares.use((_req, res, next) => {
        set(res);
        next();
      });
    },
    configurePreviewServer(server) {
      server.middlewares.use((_req, res, next) => {
        set(res);
        next();
      });
    },
  };
}
