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
  import { connectDecoders } from '$lib/decoders/store.svelte';
  import { connectEventBus } from '$lib/events/connect';
  import { connectAi } from '$lib/ai/client';
  import { browserRuntime } from '$lib/runner/browserRuntime.svelte';
  import { wsUrlFor } from '$lib/api/errors';
  import { applyControl } from '$lib/control/dispatch';
  import { viewState, type LeftTab } from '$lib/viz/viewState.svelte';
  import { tools } from '$lib/tools/tools.svelte';
  import SpecAnWorkspace from '$lib/specan/SpecAnWorkspace.svelte';
  import SweepBandsPanel from '$lib/specan/SweepBandsPanel.svelte';
  import SpecAnSettings from '$lib/specan/SpecAnSettings.svelte';
  import { onMount } from 'svelte';

  // Default the tab on mount so ViewBridge ships a meaningful `left_tab`
  // on the first `view_state` push. Module-level store ⇒ ferrited can
  // read it via `GET /api/view`, and (next commit) write to it via the
  // command channel so the AI can switch tabs.
  if (viewState.leftTab === null) viewState.leftTab = 'bands';
  // Each radio tool shows its own subset of sidebar tabs; a tab the
  // active tool doesn't have falls back to its first (Bands).
  let tabs = $derived(tools.def().sidebarTabs);
  let leftTab = $derived<LeftTab>(
    viewState.leftTab && tabs.includes(viewState.leftTab) ? viewState.leftTab : tabs[0],
  );
  let spectrum = $derived(tools.active === 'spectrum');
  const TAB_LABEL: Record<LeftTab, string> = {
    bands: 'Bands',
    catalog: 'Catalog',
    settings: 'Settings',
    logs: 'Logs',
    flow: 'Flow',
    ai: 'AI',
  };

  // Opening the Logs tab acks the error badge. While the tab is open,
  // unreadErrors changes also re-fire this effect (it reads both reactive
  // sources), so live arrivals clear too — one effect covers both cases.
  $effect(() => {
    if (leftTab === 'logs' && logs.unreadErrors > 0) logs.ackErrors();
  });

  onMount(() => {
    patchConsole();
    // Subscribers first, then the transport: the bus fires `onConnect`
    // for already-registered listeners, so the mirror's snapshot pull
    // and the log sidebar both see the first connect.
    const disconnectLogs = connectServerLogs();
    const disconnectDecodes = connectDecoders();
    const disconnectBus = connectEventBus();
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
      disconnectDecodes();
      disconnectBus();
      disconnectAi();
      pipeline.teardown();
    };
  });

  // Browser runtime lifecycle: keep it in sync with the server-side
  // pipeline. The doc we run is always the server's authoritative
  // *browser half* (`GET /api/flowgraph/browser-half`, mirrored on
  // `pipeline.browserHalf`) — composed, profile-applied, transcribe-
  // injected and env-split by the *same* code path that produced the
  // running node half. That makes the auto-inserted `WsBridge` stream
  // ids line up by construction, so cross-env audio reaches the browser
  // AudioSink under EVERY `audio_split`. The browser used to re-derive
  // its own graph (`composeSource` + `injectVoiceTranscribe`, with no
  // `apply_profile`); that diverged from the node's slot numbering and
  // silently dropped audio (node→1000, browser→1001 on balanced; the
  // analogous mismatch on server). Sourcing one doc from the server kills
  // that whole class. Transcripts: server-side (sherpa) lands in the
  // decoder store; browser-side (whisper) feeds the same `transcript`
  // store via its Worker — the panel renders either.
  //
  // Structural changes trigger a reload; start/stop follow pipeline.status.
  // Both calls are idempotent.
  $effect(() => {
    const doc = pipeline.browserHalf;
    if (!doc) return;
    browserRuntime.syncFlowgraph(doc, wsUrlFor('/ws/preset'));
  });
  $effect(() => {
    browserRuntime.syncStatus(pipeline.status === 'running');
  });
  // The graph epoch travels with the `graph` record (same delta that
  // carries a new browser half). Hand it to the runner *before* the
  // half-driven reload above can run: `setEpoch` records it
  // synchronously, so the enqueued `load` picks up the new value.
  $effect(() => {
    browserRuntime.setEpoch(pipeline.graphEpoch);
  });

  // Transcribe ⇒ voice-NR, in one place. Engaging transcription (UI
  // tri-state Off|On|Transcribe, or `ferrite-ctl transcribe on` —
  // surfaced here once the `graph` record carries the profile) should
  // default the noise-reduction chain to the `voice` preset, exactly
  // as clicking the Transcribe chip used to do inline. Edge-triggered
  // on the off→true transition only, so the operator can still pick a
  // different NR preset by hand while transcription stays on.
  let prevTranscribe = false;
  $effect(() => {
    const on = pipeline.profile.transcribe;
    if (on && !prevTranscribe) void applyControl('client.audio.nrPreset', 'voice');
    prevTranscribe = on;
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
            {#each tabs as t (t)}
              <button
                type="button"
                class="relative flex-1 px-2 py-1 text-[11px] font-bold uppercase tracking-wider text-[color:var(--color-muted)] hover:bg-slate-900 hover:text-[color:var(--color-fg)]"
                class:tab-active={leftTab === t}
                onclick={() => (viewState.leftTab = t)}
              >
                {TAB_LABEL[t]}
                {#if t === 'logs' && logs.unreadErrors > 0 && leftTab !== 'logs'}
                  <span
                    class="absolute right-1 top-1 h-1.5 w-1.5 rounded-full bg-rose-500"
                    title="{logs.unreadErrors} unread error{logs.unreadErrors === 1 ? '' : 's'}"
                    aria-label="{logs.unreadErrors} unread errors"
                  ></span>
                {/if}
              </button>
            {/each}
          </div>
          <div class="min-h-0 flex-1">
            {#if leftTab === 'bands' && spectrum}
              <SweepBandsPanel />
            {:else if leftTab === 'settings' && spectrum}
              <SpecAnSettings />
            {:else if leftTab === 'bands'}
              <BandsPanel />
            {:else if leftTab === 'catalog'}
              <SignalCatalog
                activeSlug={pipeline.flowgraph?.name ?? null}
                onPick={(slug) => void pipeline.loadPreset(slug)}
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
          {#if pipeline.client && spectrum}
            <SpecAnWorkspace />
          {:else if pipeline.client}
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
