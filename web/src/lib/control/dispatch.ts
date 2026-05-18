// Single UI write entry point.
//
// Every user-facing control writes through `applyControl(path, value)`.
// The path's namespace prefix determines where the effect lands:
//
//   flow.<blockId>.<param>     → server flowgraph. Existing server
//                                routing picks hot-apply vs rebuild
//                                based on BlockSpec.reconfig_scope.
//                                Includes the `src` placeholder — goes
//                                through `patchSourceParams`.
//   browser.<blockId>.<param>  → browser-half WASM runtime in the
//                                worker. Hot-apply via the block's
//                                `apply_live_params`, else block-rebuild.
//                                No network, no full reload.
//   client.<scope>.<param>     → local JSON store. Zero round-trip,
//                                persisted to localStorage.
//
// Motivation: before this, components picked from four APIs
// (`setBlockParam`, `patchSourceParams`, `patchFlowgraph`, ad-hoc
// client state) and it was never clear at the call site what the
// write would cost. Paths make it grep-able; the dispatcher centralises
// the routing logic.

import { audioPanel } from '$lib/audio/audioPanel.svelte';
import { browserRuntime } from '$lib/runner/browserRuntime.svelte';
import { pipeline } from '$lib/pipeline.svelte';
import { clientControls, type ClientPath, type ClientValue } from './clientStore.svelte';

/** Parse a path into `{namespace, rest}` where `rest` is the
 *  namespace-specific remainder. Throws on malformed paths so the
 *  dispatcher surfaces bad input loudly rather than silently routing
 *  nothing. */
function splitPath(path: string): { namespace: string; rest: string } {
  const dot = path.indexOf('.');
  if (dot === -1) throw new Error(`applyControl: malformed path ${JSON.stringify(path)}`);
  return { namespace: path.slice(0, dot), rest: path.slice(dot + 1) };
}

/** Split `<blockId>.<param>` (the "rest" of a flow or browser path). */
function splitBlockParam(rest: string, path: string): { blockId: string; param: string } {
  const dot = rest.indexOf('.');
  if (dot === -1) {
    throw new Error(`applyControl: path ${JSON.stringify(path)} needs <blockId>.<param>`);
  }
  return { blockId: rest.slice(0, dot), param: rest.slice(dot + 1) };
}

/**
 * Route a single control write. Always returns a Promise<void> so
 * callers can `await` uniformly regardless of which namespace. Errors
 * surface to the logs store at the callee level; callers don't have
 * to catch.
 */
export async function applyControl(path: string, value: unknown): Promise<void> {
  const { namespace, rest } = splitPath(path);
  switch (namespace) {
    case 'flow': {
      const { blockId, param } = splitBlockParam(rest, path);
      if (blockId === 'src') {
        // Source params use the dedicated source endpoint — the server
        // diffs against the stored source config and decides hot vs
        // rebuild. Same semantics as any block's apply_live_params,
        // just through a different REST route.
        await pipeline.patchSourceParams({ [param]: value });
      } else {
        await pipeline.setBlockParam(blockId, param, value);
      }
      return;
    }
    case 'browser': {
      const { blockId, param } = splitBlockParam(rest, path);
      await browserRuntime.reconfigureBlock(blockId, { [param]: value });
      return;
    }
    case 'client': {
      // The full path is the store key — `client.` prefix included —
      // so grepping for `client.audio.volume` finds the declaration
      // alongside every reader and writer.
      clientControls.set(path as ClientPath, value as ClientValue<ClientPath>);
      // Audio paths also need to reach the AudioWorklet immediately
      // (it's the one "client" target with off-thread consumers). Fan
      // out so the worklet gain / mute update in real time rather than
      // only on the next attach. Other client.* paths terminate at the
      // store.
      if (path === 'client.audio.volume') audioPanel.setVolume(value as number);
      else if (path === 'client.audio.muted') audioPanel.setMuted(value as boolean);
      // The whisper prompt has an off-thread consumer too (the
      // transcription Worker) — push it live, same fan-out shape.
      else if (path === 'client.transcribe.prompt')
        browserRuntime.setTranscribePrompt(value as string);
      // NR preset → push the param bundle to the live `audio_nr`
      // block (browserRuntime reads the just-set store value).
      else if (path === 'client.audio.nrPreset') browserRuntime.applyNrPreset();
      return;
    }
    default:
      throw new Error(
        `applyControl: unknown namespace ${JSON.stringify(namespace)} in ${JSON.stringify(path)}`,
      );
  }
}
