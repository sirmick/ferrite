<script lang="ts">
  import { logs, type LogLevel } from '$lib/logs/store.svelte';
  import { tick } from 'svelte';

  let scroller: HTMLDivElement | undefined = $state();
  let stickToBottom = $state(true);
  let categoryMenuOpen = $state(false);

  const LEVEL_COLORS: Record<LogLevel, string> = {
    error: 'text-rose-400',
    warn: 'text-amber-400',
    info: 'text-slate-200',
    debug: 'text-slate-400',
    trace: 'text-slate-500',
  };

  // Stable colour palette for category badges. Hash the category
  // string into an index so the same target always picks the same
  // colour — quickly recognisable as "decoder::pocsag is teal" etc
  // without needing a legend. Server / client / vite without a
  // parsed category fall back to a fixed slate badge so untagged
  // free-text from the browser still has a visible pill.
  const CATEGORY_PALETTE = [
    'bg-indigo-900/60 text-indigo-200',
    'bg-emerald-900/60 text-emerald-200',
    'bg-amber-900/60 text-amber-200',
    'bg-rose-900/60 text-rose-200',
    'bg-sky-900/60 text-sky-200',
    'bg-purple-900/60 text-purple-200',
    'bg-cyan-900/60 text-cyan-200',
    'bg-pink-900/60 text-pink-200',
    'bg-lime-900/60 text-lime-200',
    'bg-orange-900/60 text-orange-200',
  ];
  const FALLBACK_BADGE = 'bg-slate-800 text-slate-400';

  function hashStr(s: string): number {
    let h = 0;
    for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
    return h;
  }

  function badgeFor(category: string | null): string {
    if (!category) return FALLBACK_BADGE;
    return CATEGORY_PALETTE[Math.abs(hashStr(category)) % CATEGORY_PALETTE.length];
  }

  function fmtTime(t: number): string {
    const d = new Date(t);
    return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}:${String(d.getSeconds()).padStart(2, '0')}.${String(d.getMilliseconds()).padStart(3, '0')}`;
  }

  // Visible entries = stored minus muted-category entries. The mute set
  // is read reactively, so toggling a checkbox re-filters in place.
  let visibleEntries = $derived(
    logs.entries.filter((e) => e.category === null || !logs.mutedCategories.has(e.category)),
  );
  // Sorted list of every category the panel has seen this session.
  let categoryList = $derived([...logs.knownCategories].sort());
  let mutedCount = $derived(logs.mutedCategories.size);

  $effect(() => {
    void visibleEntries.length;
    if (!stickToBottom || !scroller) return;
    void tick().then(() => {
      if (scroller) scroller.scrollTop = scroller.scrollHeight;
    });
  });

  function onScroll() {
    if (!scroller) return;
    const d = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    stickToBottom = d < 32;
  }
</script>

<div
  class="flex h-full w-full flex-col border-r border-slate-800 bg-[color:var(--color-bg)] text-xs"
>
  <div class="flex items-center justify-between border-b border-slate-800 px-2 py-1">
    <span class="font-semibold text-[color:var(--color-muted)]">Logs</span>
    <div class="flex items-center gap-2">
      <div class="relative">
        <button
          type="button"
          class="rounded border border-slate-700 px-1.5 py-0.5 text-[10px] hover:border-slate-500"
          class:border-amber-700={mutedCount > 0}
          class:text-amber-300={mutedCount > 0}
          onclick={() => (categoryMenuOpen = !categoryMenuOpen)}
          title="filter by category"
        >
          categories{mutedCount > 0 ? ` (${mutedCount} muted)` : ''}
        </button>
        {#if categoryMenuOpen}
          <!-- click-outside dismiss -->
          <button
            type="button"
            aria-label="close category filter"
            class="fixed inset-0 z-10 cursor-default bg-transparent"
            onclick={() => (categoryMenuOpen = false)}
          ></button>
          <div
            class="absolute right-0 top-full z-20 mt-1 max-h-72 w-56 overflow-y-auto rounded border border-slate-700 bg-slate-900 py-1 shadow-lg"
            role="menu"
          >
            <div
              class="flex items-center justify-between border-b border-slate-800 px-2 pb-1 text-[10px] text-[color:var(--color-muted)]"
            >
              <span>{categoryList.length} seen</span>
              <button
                type="button"
                class="rounded border border-slate-700 px-1.5 text-[10px] leading-tight hover:border-slate-500 disabled:opacity-40"
                disabled={mutedCount === 0}
                onclick={() => logs.unmuteAll()}
              >
                show all
              </button>
            </div>
            {#if categoryList.length === 0}
              <div class="px-2 py-1 text-[10px] text-[color:var(--color-muted)]">
                no categories yet
              </div>
            {:else}
              {#each categoryList as cat (cat)}
                <label
                  class="flex cursor-pointer items-center gap-2 px-2 py-0.5 hover:bg-slate-800"
                >
                  <input
                    type="checkbox"
                    checked={!logs.mutedCategories.has(cat)}
                    onchange={() => logs.toggleMute(cat)}
                  />
                  <span class="font-mono text-[11px] text-slate-200">{cat}</span>
                </label>
              {/each}
            {/if}
          </div>
        {/if}
      </div>
      <label class="flex items-center gap-1 text-[10px] text-[color:var(--color-muted)]">
        <input type="checkbox" bind:checked={stickToBottom} /> follow
      </label>
      <button
        type="button"
        class="rounded border border-slate-700 px-1.5 py-0.5 text-[10px] hover:border-slate-500"
        onclick={() => logs.clear()}>clear</button
      >
    </div>
  </div>
  <div
    bind:this={scroller}
    onscroll={onScroll}
    class="flex-1 overflow-y-auto font-mono text-[11px] leading-tight"
  >
    {#each visibleEntries as entry (entry.id)}
      <div class="flex gap-1 border-b border-slate-900 px-2 py-0.5">
        <span class="shrink-0 text-[color:var(--color-muted)]">{fmtTime(entry.t)}</span>
        <span
          class="shrink-0 rounded px-1 text-[10px] {badgeFor(entry.category)}"
          title={entry.category
            ? `category: ${entry.category} · source: ${entry.source}`
            : `source: ${entry.source}`}
        >
          {entry.category ?? entry.source}
        </span>
        <span class="min-w-0 break-words {LEVEL_COLORS[entry.level]}">{entry.text}</span>
      </div>
    {/each}
    {#if visibleEntries.length === 0}
      <div class="p-3 text-[color:var(--color-muted)]">
        {logs.entries.length === 0
          ? 'no log entries yet…'
          : `${logs.entries.length} entries hidden by category filter`}
      </div>
    {/if}
  </div>
</div>
