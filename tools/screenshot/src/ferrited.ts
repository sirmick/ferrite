// Spawn / wait / kill helper for ferrited under a Playwright run.
// Spawns ferrited with the built SvelteKit bundle mounted via
// FERRITE_STATIC_ROOT (so the e2e exercises the real static-served UI,
// not the vite dev server run.sh uses). Requires --flowgraph (a
// placeholder; the pipeline isn't started until we POST
// /api/pipeline/start), and binds to 127.0.0.1 on a random port.

import { spawn, type ChildProcess } from 'node:child_process';
import { createServer } from 'node:net';
import * as fs from 'node:fs';
import * as path from 'node:path';

export interface FerritedHandle {
  readonly url: string;
  readonly port: number;
  readonly proc: ChildProcess;
  stop(): Promise<void>;
}

export interface SpawnOptions {
  /** Repo root. Defaults to walking up from cwd until Cargo.toml is found. */
  repoRoot?: string;
  /** Override the placeholder flowgraph (anything in flowgraphs/ works). */
  placeholderFlowgraph?: string;
  /** Override port; otherwise an ephemeral port is picked. */
  port?: number;
  /** RUST_LOG override; default `warn` (tests want quiet stderr). */
  rustLog?: string;
  /** Forward [ferrited] output to this stream. Default: process.stderr. */
  log?: NodeJS.WritableStream | null;
}

const FERRITED_BIN = 'target/release/ferrited';
const STATIC_ROOT = 'web/build';
const DEFAULT_PLACEHOLDER = 'flowgraphs/wbfm.json';

function findRepoRoot(start: string): string {
  let dir = path.resolve(start);
  for (;;) {
    if (fs.existsSync(path.join(dir, 'Cargo.toml')) && fs.existsSync(path.join(dir, 'web'))) {
      return dir;
    }
    const parent = path.dirname(dir);
    if (parent === dir) throw new Error(`couldn't locate Ferrite repo root above ${start}`);
    dir = parent;
  }
}

async function pickPort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const srv = createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address();
      if (addr && typeof addr === 'object') {
        const { port } = addr;
        srv.close(() => resolve(port));
      } else {
        reject(new Error('listen() returned no address'));
      }
    });
  });
}

async function waitForReady(url: string, timeoutMs = 30_000): Promise<void> {
  const start = Date.now();
  let lastErr: unknown = null;
  while (Date.now() - start < timeoutMs) {
    try {
      const r = await fetch(`${url}/api/hello`);
      if (r.ok) return;
    } catch (e) {
      lastErr = e;
    }
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`ferrited never answered /api/hello at ${url}: ${String(lastErr)}`);
}

/**
 * Spawn ferrited on a free port with the built UI mounted at
 * FERRITE_STATIC_ROOT, wait for /api/hello, and return a handle whose
 * `stop()` SIGTERMs the child and waits for exit.
 *
 * The caller is expected to do the preset/source/start dance over REST
 * — this helper only stands the server up.
 */
export async function spawnFerrited(opts: SpawnOptions = {}): Promise<FerritedHandle> {
  const repoRoot = opts.repoRoot ?? findRepoRoot(process.cwd());
  const binary = path.join(repoRoot, FERRITED_BIN);
  const staticRoot = path.join(repoRoot, STATIC_ROOT);
  const flowgraph = path.join(repoRoot, opts.placeholderFlowgraph ?? DEFAULT_PLACEHOLDER);

  if (!fs.existsSync(binary)) {
    throw new Error(
      `${binary} missing — run \`./build.sh\` (or \`cargo build --release -p ferrited\`) first`,
    );
  }
  if (!fs.existsSync(path.join(staticRoot, 'index.html'))) {
    throw new Error(
      `${staticRoot}/index.html missing — run \`pnpm -C web build\` first so ferrited has a UI to serve`,
    );
  }
  if (!fs.existsSync(flowgraph)) {
    throw new Error(`placeholder flowgraph not found: ${flowgraph}`);
  }

  const port = opts.port ?? (await pickPort());
  const url = `http://127.0.0.1:${port}`;
  const log = opts.log === null ? null : (opts.log ?? process.stderr);

  // If the repo has a local SoapySDR build (soapysdr/env.sh), source
  // it via a shell wrapper so LD_LIBRARY_PATH + SOAPY_SDR_PLUGIN_PATH
  // are set the same way as `run.sh`. Otherwise spawn directly.
  const envScript = path.join(repoRoot, 'soapysdr/env.sh');
  const useShellWrapper = fs.existsSync(envScript);

  const childEnv = {
    ...process.env,
    FERRITE_STATIC_ROOT: staticRoot,
    RUST_LOG: opts.rustLog ?? process.env.RUST_LOG ?? 'warn',
  };

  const proc = useShellWrapper
    ? spawn(
        'bash',
        [
          '-c',
          '. ./soapysdr/env.sh && exec "$@"',
          'ferrited-wrapper',
          binary,
          '--bind',
          `127.0.0.1:${port}`,
          '--flowgraph',
          flowgraph,
        ],
        { cwd: repoRoot, env: childEnv, stdio: ['ignore', 'pipe', 'pipe'] },
      )
    : spawn(
        binary,
        ['--bind', `127.0.0.1:${port}`, '--flowgraph', flowgraph],
        { cwd: repoRoot, env: childEnv, stdio: ['ignore', 'pipe', 'pipe'] },
      );

  if (log) {
    proc.stdout?.on('data', (d: Buffer) => log.write(`[ferrited] ${d}`));
    proc.stderr?.on('data', (d: Buffer) => log.write(`[ferrited] ${d}`));
  }

  // If the child dies before /api/hello answers, surface the exit code
  // rather than letting waitForReady stall to its full timeout.
  let earlyExit: { code: number | null; signal: NodeJS.Signals | null } | null = null;
  proc.once('exit', (code, signal) => {
    earlyExit = { code, signal };
  });

  const ready = waitForReady(url).catch((e: unknown) => {
    if (earlyExit) {
      throw new Error(
        `ferrited exited (code=${earlyExit.code}, signal=${earlyExit.signal}) before /api/hello`,
      );
    }
    throw e;
  });
  await ready;

  return {
    url,
    port,
    proc,
    async stop() {
      if (proc.exitCode !== null || proc.signalCode) return;
      proc.kill('SIGTERM');
      await new Promise<void>((resolve) => {
        const timer = setTimeout(() => {
          proc.kill('SIGKILL');
          resolve();
        }, 5000);
        proc.once('exit', () => {
          clearTimeout(timer);
          resolve();
        });
      });
    },
  };
}
