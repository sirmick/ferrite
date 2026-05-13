// End-to-end test for the browser pipeline store driving the WBFM
// round-trip: load preset → retune source → tweak a demod knob. The
// point isn't to exercise the individual REST helpers (each has its
// own tests) but to verify the store's orchestration — that every
// knob turn fans out to the right endpoints and leaves the local
// mirror in lockstep with the synthesised server state.
//
// `fetch` is stubbed with a method+path dispatcher over a tiny
// in-memory server model; no `pipeline.init()` so we skip the
// WebSocket + WASM decoder initialisation, which belong to the UI
// frames path and are out of scope here.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { pipeline } from '$lib/pipeline.svelte';
import type { FlowgraphDoc } from '$lib/flowgraph';
import type { PipelineBlock } from '$lib/api/pipelineBlocks';
import type { UiSink } from '$lib/api/uiSinks';
import type { SourceConfig } from '$lib/api/source';
import type { BlockSchema, ParamSchema } from '$lib/api/flowgraph';

// --- block schemas ---------------------------------------------------

const P = (key: string, scope: 'self' | 'downstream' | 'sourceRestart'): ParamSchema => ({
  key,
  label: key,
  reconfig_scope: scope,
  ai_notes: '',
  kind: 'range',
  min: 0,
  max: 1e9,
  step: 1,
  default: 0,
  unit: '',
});

const SOURCE_SCHEMA: BlockSchema = {
  type_name: 'Source',
  placement: 'native',
  inputs: [],
  outputs: [{ name: 'out', port_type: 'iq_f32' }],
  params: [
    P('sample_rate_hz', 'sourceRestart'),
    P('center_freq_hz', 'self'),
    P('bandwidth_hz', 'self'),
  ],
  ai_notes: '',
};

const FM_SCHEMA: BlockSchema = {
  type_name: 'FmDemod',
  placement: 'either',
  inputs: [{ name: 'in', port_type: 'iq_f32' }],
  outputs: [{ name: 'out', port_type: 'real_f32' }],
  params: [P('sample_rate_hz', 'sourceRestart'), P('max_deviation_hz', 'downstream')],
  ai_notes: '',
};

// --- fake server state -----------------------------------------------

interface ServerState {
  flowgraph: FlowgraphDoc;
  source: SourceConfig;
  blockValues: Record<string, Record<string, unknown>>;
  uiSinks: UiSink[];
  calls: { method: string; path: string; body?: unknown }[];
}

function wbfmDoc(): FlowgraphDoc {
  return {
    name: 'wbfm',
    environments: ['node', 'browser'],
    blocks: {
      src: {
        type: 'Source',
        placement: 'node',
        params: { center_freq_hz: 100_100_000, sample_rate_hz: 2_400_000 },
      },
      demod: {
        type: 'FmDemod',
        placement: 'browser',
        params: { sample_rate_hz: 48_000, max_deviation_hz: 75_000 },
      },
    },
    wires: [['src.out', 'demod.in']],
  };
}

function freshServer(): ServerState {
  return {
    flowgraph: wbfmDoc(),
    source: {
      type: 'SoapySource',
      params: { args: 'driver=rtlsdr', center_freq_hz: 100_100_000, sample_rate_hz: 2_400_000 },
    },
    blockValues: {
      src: { center_freq_hz: 100_100_000, sample_rate_hz: 2_400_000 },
      demod: { sample_rate_hz: 48_000, max_deviation_hz: 75_000 },
    },
    uiSinks: [{ name: 'fft', stream_id: 1001, payload_type: 'FftU8' }],
    calls: [],
  };
}

function pipelineBlocks(state: ServerState): PipelineBlock[] {
  return [
    {
      id: 'src',
      type_name: 'Source',
      placement: 'node',
      spec: SOURCE_SCHEMA,
      values: state.blockValues.src,
    },
    {
      id: 'demod',
      type_name: 'FmDemod',
      placement: 'browser',
      spec: FM_SCHEMA,
      values: state.blockValues.demod,
    },
  ];
}

function okJson(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

function installFetch(state: ServerState) {
  const handler = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
    const method = (init?.method ?? 'GET').toUpperCase();
    const path = new URL(url, 'http://localhost').pathname;
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    state.calls.push({ method, path, body });

    if (method === 'GET' && path === '/api/flowgraph') return okJson(state.flowgraph);
    if (method === 'GET' && path === '/api/source') return okJson(state.source);
    if (method === 'GET' && path === '/api/pipeline/blocks') return okJson(pipelineBlocks(state));
    if (method === 'GET' && path === '/api/ui-sinks') return okJson(state.uiSinks);

    if (method === 'POST' && path === '/api/preset') {
      // Simulate: server loads the named preset and swaps it in. Tests
      // only drive the `wbfm` name, so the canned doc is fine.
      state.flowgraph = wbfmDoc();
      return okJson({
        name: state.flowgraph.name,
        reconfigure: {
          applied: true,
          overall: 'sourceRestart',
          changes: [],
          structural_count: 1,
          noop: false,
        },
      });
    }

    if (method === 'PATCH' && path === '/api/source') {
      state.source = body as SourceConfig;
      if (typeof state.source.params.center_freq_hz === 'number') {
        state.blockValues.src.center_freq_hz = state.source.params.center_freq_hz;
      }
      return okJson({
        applied: true,
        overall: 'self',
        changes: [],
        structural_count: 0,
        noop: false,
      });
    }

    const blockParamsMatch = /^\/api\/pipeline\/blocks\/([^/]+)\/params$/.exec(path);
    if (method === 'POST' && blockParamsMatch) {
      const id = decodeURIComponent(blockParamsMatch[1]);
      Object.assign(state.blockValues[id] ?? (state.blockValues[id] = {}), body ?? {});
      return okJson({
        applied: true,
        overall: 'downstream',
        changes: [],
        structural_count: 0,
        noop: false,
      });
    }

    if (method === 'GET' && path === '/api/source/capabilities') {
      return okJson({ kind: 'software', type_name: 'Source' });
    }

    throw new Error(`unhandled fetch: ${method} ${path}`);
  });
  vi.stubGlobal('fetch', handler);
  return handler;
}

// --- tests -----------------------------------------------------------

describe('WBFM preset — pipeline store end-to-end', () => {
  let state: ServerState;

  beforeEach(() => {
    state = freshServer();
    installFetch(state);
    // Seed the store as if init() had run, minus the WebSocket/WASM
    // bits. Each test gets a clean mirror.
    pipeline.flowgraph = state.flowgraph;
    pipeline.source = state.source;
    pipeline.uiSinks = Object.fromEntries(state.uiSinks.map((s) => [s.name, s]));
    pipeline.blocks = Object.fromEntries(pipelineBlocks(state).map((b) => [b.id, b]));
    pipeline.phase = 'ready';
    pipeline.status = 'running';
    pipeline.errorMessage = null;
  });

  test('loadPreset re-reads flowgraph + composed state from server', async () => {
    await pipeline.loadPreset('wbfm');

    const presetCall = state.calls.find((c) => c.method === 'POST' && c.path === '/api/preset');
    expect(presetCall?.body).toEqual({ name: 'wbfm' });

    const paths = state.calls.map((c) => `${c.method} ${c.path}`);
    expect(paths).toContain('GET /api/flowgraph');
    expect(paths).toContain('GET /api/pipeline/blocks');
    expect(paths).toContain('GET /api/ui-sinks');

    expect(pipeline.flowgraph?.name).toBe('wbfm');
    expect(pipeline.uiSinks.fft?.stream_id).toBe(1001);
  });

  test('patchSourceParams routes a centre-frequency tune through /api/source', async () => {
    await pipeline.patchSourceParams({ center_freq_hz: 101_000_000 });

    const patch = state.calls.find((c) => c.method === 'PATCH' && c.path === '/api/source');
    expect(patch).toBeDefined();
    expect((patch!.body as SourceConfig).params.center_freq_hz).toBe(101_000_000);

    expect(pipeline.source?.params.center_freq_hz).toBe(101_000_000);
    // refreshComposed pulls fresh blocks — the src mirror follows the
    // server's new value.
    expect(pipeline.blocks.src.values?.center_freq_hz).toBe(101_000_000);
  });

  test('setBlockParam routes to /api/pipeline/blocks/:id/params and refreshes mirror', async () => {
    const resp = await pipeline.setBlockParam('demod', 'max_deviation_hz', 50_000);
    expect(resp?.overall).toBe('downstream');

    const post = state.calls.find(
      (c) => c.method === 'POST' && c.path === '/api/pipeline/blocks/demod/params',
    );
    expect(post?.body).toEqual({ max_deviation_hz: 50_000 });

    expect(pipeline.blocks.demod.values?.max_deviation_hz).toBe(50_000);
    expect(pipeline.blocks.demod.values?.sample_rate_hz).toBe(48_000);
  });

  test('full WBFM flow: load preset, tune, tweak demod — one integration pass', async () => {
    await pipeline.loadPreset('wbfm');
    await pipeline.patchSourceParams({ center_freq_hz: 88_700_000 });
    await pipeline.setBlockParam('demod', 'max_deviation_hz', 50_000);

    expect(pipeline.flowgraph?.name).toBe('wbfm');
    expect(pipeline.source?.params.center_freq_hz).toBe(88_700_000);
    expect(pipeline.blocks.demod.values?.max_deviation_hz).toBe(50_000);
    expect(pipeline.phase).toBe('ready');
    expect(pipeline.errorMessage).toBeNull();

    const methods = state.calls.map((c) => `${c.method} ${c.path}`);
    expect(methods).toContain('POST /api/preset');
    expect(methods).toContain('PATCH /api/source');
    expect(methods).toContain('POST /api/pipeline/blocks/demod/params');
  });
});
