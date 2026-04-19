<script lang="ts">
  import { catalog, type CatalogEntry } from './catalog';

  interface Props {
    /** Called when the user picks a preset. */
    onPick?: (entry: CatalogEntry) => void;
    /** Slug of the currently-loaded preset, for highlight. */
    activeSlug?: string | null;
  }

  let { onPick, activeSlug = null }: Props = $props();

  let query = $state('');
  let filtered = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (!q) return catalog;
    return catalog.filter(
      (e) =>
        e.label.toLowerCase().includes(q) ||
        e.slug.toLowerCase().includes(q) ||
        (e.description ?? '').toLowerCase().includes(q),
    );
  });
</script>

<div
  class="flex h-full w-full flex-col border-r border-slate-800 bg-[color:var(--color-bg)] text-xs"
>
  <div class="flex items-center justify-between border-b border-slate-800 px-2 py-1">
    <span class="font-semibold text-[color:var(--color-muted)]">Signal Catalog</span>
    <span class="text-[10px] text-[color:var(--color-muted)]">{catalog.length} presets</span>
  </div>
  <div class="border-b border-slate-900 px-2 py-1">
    <input
      type="search"
      bind:value={query}
      placeholder="filter…"
      class="w-full rounded border border-slate-800 bg-slate-950 px-2 py-0.5 text-[11px] text-slate-200 placeholder:text-slate-600 focus:border-sky-600 focus:outline-none"
    />
  </div>
  <div class="min-h-0 flex-1 overflow-y-auto">
    {#if filtered.length === 0}
      <div class="px-2 py-3 text-[11px] text-[color:var(--color-muted)]">
        {catalog.length === 0 ? 'no presets installed' : 'no matches'}
      </div>
    {:else}
      <ul>
        {#each filtered as entry (entry.slug)}
          <li>
            <button
              type="button"
              class="flex w-full flex-col gap-0.5 border-b border-slate-900 px-2 py-1 text-left hover:bg-slate-800/70"
              class:active={activeSlug === entry.slug}
              onclick={() => onPick?.(entry)}
            >
              <span class="flex items-baseline justify-between gap-2">
                <span class="truncate font-medium text-slate-200">{entry.label}</span>
                <span class="shrink-0 font-mono text-[10px] text-slate-500">{entry.slug}</span>
              </span>
              {#if entry.description}
                <span class="line-clamp-2 text-[10px] text-[color:var(--color-muted)]">
                  {entry.description}
                </span>
              {/if}
              <span class="flex gap-1 text-[9px] text-slate-600">
                {#each entry.environments as env (env)}
                  <span class="rounded border border-slate-800 px-1">{env}</span>
                {/each}
              </span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
</div>

<style>
  .active {
    background: rgba(125, 211, 252, 0.1);
  }
  .active :global(.font-medium) {
    color: #7dd3fc;
  }
</style>
