<script lang="ts">
  import Workspace from '$lib/layout/Workspace.svelte';
  import LogPanel from '$lib/layout/LogPanel.svelte';
  import FlowPanel from '$lib/layout/FlowPanel.svelte';
  import AiPanel from '$lib/layout/AiPanel.svelte';
  import Split from '$lib/layout/Split.svelte';
  import BandsPanel from '$lib/presets/BandsPanel.svelte';
  import SettingsPanel from '$lib/presets/SettingsPanel.svelte';
  import SignalCatalog from '$lib/presets/SignalCatalog.svelte';
  import { pipeline } from '$lib/pipeline.svelte';
  import { logs, patchConsole } from '$lib/logs/store.svelte';
  import { connectServerLogs } from '$lib/logs/client';
  import { connectAi } from '$lib/ai/client';
  import { installAutoSettingsEffect } from '$lib/controls/autoSettings.svelte';
  import { browserRuntime } from '$lib/runner/browserRuntime.svelte';
  import { wsUrlFor } from '$lib/api/errors';
  import { composeSource, injectVoiceTranscribe } from '$lib/flowgraph';
  import { onMount } from 'svelte';

  type LeftTab = 'bands' | 'catalog' | 'settings' | 'logs' | 'flow' | 'ai';
  let leftTab = $state<LeftTab>('bands');

  // Opening the Logs tab acks the error badge. While the tab is open,
  // unreadErrors changes also re-fire this effect (it reads both reactive
  // sources), so live arrivals clear too — one effect covers both cases.
  $effect(() => {
    if (leftTab === 'logs' && logs.unreadErrors > 0) logs.ackErrors();
  });

  // Auto-toggle hardware notch filters / driver-specific settings
  // when the centre frequency crosses a configured band — see
  // web/src/lib/controls/sdr-presets/<driver>.json `auto_settings`.
  installAutoSettingsEffect();

  onMount(() => {
    patchConsole();
    const disconnectLogs = connectServerLogs();
    const disconnectAi = connectAi();
    void pipeline.init();
    browserRuntime.init();
    // Gesture-unlock: `AudioContext.resume()` is a no-op until the
    // user has interacted with the page. Keep these listeners
    // *permanent*, not `once:true` — HMR and preset reloads create
    // fresh AudioContexts that each need their own gesture to resume,
    // so every click/keydown gets a shot at unlocking the current one.
    // `unlockAudio` is idempotent on already-running contexts.
    const unlock = () => void browserRuntime.unlockAudio();
    document.addEventListener('click', unlock);
    document.addEventListener('keydown', unlock);
    return () => {
      document.removeEventListener('click', unlock);
      document.removeEventListener('keydown', unlock);
      void browserRuntime.teardown();
      disconnectLogs();
      disconnectAi();
      pipeline.teardown();
    };
  });

  // Browser runtime lifecycle: keep it in sync with the server-side
  // pipeline. We compose preset + source into a runnable doc here
  // (mirror of Rust's `compose_source`) — the wasm runtime needs the
  // `src` placeholder replaced with the real source type, otherwise
  // the `Source` sentinel looks like an unknown block. Structural
  // flowgraph changes trigger a reload; start/stop follow the server's
  // pipeline.status. Both calls are idempotent.
  $effect(() => {
    const preset = pipeline.flowgraph;
    const source = pipeline.source;
    if (!preset || !source) return;
    const composed = composeSource(preset, {
      type: source.type,
      params: source.params as Record<string, unknown>,
    });
    // Mirror the server's profile-gated VoiceTranscribe injection
    // (runtime/src/inject_voice_transcribe.rs). The browser runtime
    // builds its own graph from composeSource, so without this the tap
    // exists only node-side. Same `transcribe` profile bit the
    // receiver's Audio control sets — reading it here makes this effect
    // re-sync the browser graph when the operator engages transcription.
    const withVt = pipeline.profile.transcribe ? injectVoiceTranscribe(composed) : composed;
    browserRuntime.syncFlowgraph(withVt, wsUrlFor('/ws/preset'));
  });
  $effect(() => {
    browserRuntime.syncStatus(pipeline.status === 'running');
  });

  // Out-of-band state sync. `ferrite-ctl` (and the AI sidecar driving
  // it) mutates ferrited via REST without going through `pipeline`'s
  // setters, so the source/flowgraph/preset mirror would otherwise
  // stay stale until the user clicked something. The activity-log
  // middleware emits one `ai::activity` line per CLI-driven mutation;
  // tail that and refresh the mirror — single trigger, no new WS.
  let lastAiActivityId = $state(0);
  $effect(() => {
    const latest = logs.entries.findLast((e) => e.category === 'ai::activity');
    if (!latest || latest.id <= lastAiActivityId) return;
    lastAiActivityId = latest.id;
    void pipeline.refreshFromServer();
  });
</script>

<div class="flex h-dvh w-dvw flex-col">
  <div class="flex min-h-0 flex-1">
    <Split
      direction="row"
      defaultFraction={0.22}
      min={0.12}
      max={0.5}
      storageKey="ferrite.split.aside-main"
    >
      {#snippet a()}
        <aside class="flex h-full min-h-0 w-full flex-col">
          <div class="flex items-stretch border-b border-slate-800">
            <!-- Brand mark: a wire-wound ferrite rod (the radio loopstick
                 antenna inside every old AM set). Slate body for the
                 ferrite core, copper-amber for the wire wraps; the rod
                 fill is a vertical gradient so it reads as cylindrical
                 rather than a flat pill. -->
            <span class="flex shrink-0 items-center px-2" title="Ferrite — pre-alpha">
              <svg
                viewBox="0 0 36 18"
                width="40"
                height="20"
                fill="none"
                stroke-linecap="round"
                aria-hidden="true"
              >
                <defs>
                  <linearGradient id="ferrite-rod" x1="0" y1="0" x2="0" y2="1">
                    <stop offset="0%" stop-color="#94a3b8" />
                    <stop offset="55%" stop-color="#475569" />
                    <stop offset="100%" stop-color="#1e293b" />
                  </linearGradient>
                </defs>
                <!-- Ferrite core -->
                <rect
                  x="3"
                  y="6"
                  width="30"
                  height="6"
                  rx="3"
                  fill="url(#ferrite-rod)"
                  stroke="#0f172a"
                  stroke-width="0.8"
                />
                <!-- Six diagonal wire wraps in copper -->
                <path
                  d="M6 3.5 L8 14.5 M11 3.5 L13 14.5 M16 3.5 L18 14.5 M21 3.5 L23 14.5 M26 3.5 L28 14.5"
                  stroke="#f59e0b"
                  stroke-width="1.6"
                />
              </svg>
            </span>
            <button
              type="button"
              class="flex-1 px-2 py-1 text-[11px] font-bold uppercase tracking-wider text-[color:var(--color-muted)] hover:bg-slate-900 hover:text-[color:var(--color-fg)]"
              class:tab-active={leftTab === 'bands'}
              onclick={() => (leftTab = 'bands')}>Bands</button
            >
            <button
              type="button"
              class="flex-1 px-2 py-1 text-[11px] font-bold uppercase tracking-wider text-[color:var(--color-muted)] hover:bg-slate-900 hover:text-[color:var(--color-fg)]"
              class:tab-active={leftTab === 'catalog'}
              onclick={() => (leftTab = 'catalog')}>Catalog</button
            >
            <button
              type="button"
              class="flex-1 px-2 py-1 text-[11px] font-bold uppercase tracking-wider text-[color:var(--color-muted)] hover:bg-slate-900 hover:text-[color:var(--color-fg)]"
              class:tab-active={leftTab === 'settings'}
              onclick={() => (leftTab = 'settings')}>Settings</button
            >
            <button
              type="button"
              class="relative flex-1 px-2 py-1 text-[11px] font-bold uppercase tracking-wider text-[color:var(--color-muted)] hover:bg-slate-900 hover:text-[color:var(--color-fg)]"
              class:tab-active={leftTab === 'logs'}
              onclick={() => (leftTab = 'logs')}
            >
              Logs
              {#if logs.unreadErrors > 0 && leftTab !== 'logs'}
                <span
                  class="absolute right-1 top-1 h-1.5 w-1.5 rounded-full bg-rose-500"
                  title="{logs.unreadErrors} unread error{logs.unreadErrors === 1 ? '' : 's'}"
                  aria-label="{logs.unreadErrors} unread errors"
                ></span>
              {/if}
            </button>
            <button
              type="button"
              class="flex-1 px-2 py-1 text-[11px] font-bold uppercase tracking-wider text-[color:var(--color-muted)] hover:bg-slate-900 hover:text-[color:var(--color-fg)]"
              class:tab-active={leftTab === 'flow'}
              onclick={() => (leftTab = 'flow')}>Flow</button
            >
            <button
              type="button"
              class="flex-1 px-2 py-1 text-[11px] font-bold uppercase tracking-wider text-[color:var(--color-muted)] hover:bg-slate-900 hover:text-[color:var(--color-fg)]"
              class:tab-active={leftTab === 'ai'}
              onclick={() => (leftTab = 'ai')}>AI</button
            >
          </div>
          <div class="min-h-0 flex-1">
            {#if leftTab === 'bands'}
              <BandsPanel />
            {:else if leftTab === 'catalog'}
              <SignalCatalog
                activeSlug={pipeline.flowgraph?.name ?? null}
                onPick={(entry) => void pipeline.loadPreset(entry.slug)}
              />
            {:else if leftTab === 'settings'}
              <SettingsPanel />
            {:else if leftTab === 'flow'}
              <FlowPanel />
            {:else if leftTab === 'ai'}
              <AiPanel />
            {:else}
              <LogPanel />
            {/if}
          </div>
        </aside>
      {/snippet}
      {#snippet b()}
        <div class="h-full min-w-0 w-full">
          {#if pipeline.client}
            <Workspace client={pipeline.client} />
          {:else}
            <div
              class="flex h-full items-center justify-center text-sm text-[color:var(--color-muted)]"
            >
              connecting…
            </div>
          {/if}
        </div>
      {/snippet}
    </Split>
  </div>
</div>

<style>
  .tab-active {
    color: var(--color-fg);
    background: rgba(125, 211, 252, 0.08);
    border-bottom: 1px solid #7dd3fc;
  }
</style>
