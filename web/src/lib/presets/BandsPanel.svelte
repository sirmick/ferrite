<script lang="ts">
  import { bands, type BandEntry } from './bands';
  import { session } from '$lib/session.svelte';

  let openGroups = $state<Record<string, boolean>>({});
  function toggle(name: string) {
    openGroups[name] = !(openGroups[name] ?? defaultOpen(name));
  }
  function defaultOpen(name: string): boolean {
    return bands[0]?.name === name;
  }

  function fmtHz(hz: number): string {
    if (hz >= 1e9) return `${(hz / 1e9).toFixed(3)} GHz`;
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(3)} MHz`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(3)} kHz`;
    return `${hz} Hz`;
  }

  function tune(e: BandEntry) {
    if (!session.state) return;
    void session.patch({ center_freq_hz: e.hz });
  }

  let active = $derived(session.state?.center_freq_hz ?? null);
</script>

<div
  class="flex h-full w-full flex-col border-r border-slate-800 bg-[color:var(--color-bg)] text-xs"
>
  <div class="flex items-center justify-between border-b border-slate-800 px-2 py-1">
    <span class="font-semibold text-[color:var(--color-muted)]">Bands</span>
    <span class="text-[10px] text-[color:var(--color-muted)]">click to tune</span>
  </div>
  <div class="min-h-0 flex-1 overflow-y-auto">
    {#each bands as group (group.name)}
      {@const isOpen = openGroups[group.name] ?? defaultOpen(group.name)}
      <div class="border-b border-slate-900">
        <button
          type="button"
          class="flex w-full items-center justify-between px-2 py-1 text-left hover:bg-slate-900/60"
          onclick={() => toggle(group.name)}
        >
          <span class="text-[color:var(--color-muted)]">{group.name}</span>
          <span class="text-[10px] text-[color:var(--color-muted)]">{isOpen ? '▾' : '▸'}</span>
        </button>
        {#if isOpen}
          <ul class="pb-1">
            {#each group.entries as entry (entry.hz + entry.label)}
              <li>
                <button
                  type="button"
                  class="flex w-full items-center justify-between gap-2 px-3 py-0.5 text-left text-[11px] hover:bg-slate-800/70"
                  class:active={active === entry.hz}
                  disabled={!session.state}
                  onclick={() => tune(entry)}
                >
                  <span class="truncate">{entry.label}</span>
                  <span class="shrink-0 font-mono text-[10px] text-slate-400">
                    {fmtHz(entry.hz)}
                    {#if entry.mode}
                      <span class="ml-1 text-[9px] text-slate-500">{entry.mode}</span>
                    {/if}
                  </span>
                </button>
              </li>
            {/each}
          </ul>
        {/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .active {
    background: rgba(125, 211, 252, 0.15);
    color: #7dd3fc;
  }
</style>
