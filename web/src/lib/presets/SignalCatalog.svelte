<script lang="ts">
  import { catalog, presetDocBySlug, type CatalogEntry } from './catalog';

  interface Props {
    /** Called with the loadable slug the user picked. */
    onPick?: (slug: string) => void;
    /** Slug of the currently-loaded preset, for highlight. For a
     *  variant the server returns the *base* name, so a family parent
     *  highlights when its base matches. */
    activeSlug?: string | null;
  }

  let { onPick, activeSlug = null }: Props = $props();

  let query = $state('');

  // Sample preview: at most one clip plays at a time, created lazily
  // and torn down on end / on switching preview.
  let playingSlug = $state<string | null>(null);
  let audio: HTMLAudioElement | null = null;

  const presetCount = presetDocBySlug.size;

  function entryMatches(e: CatalogEntry, q: string): boolean {
    if (
      e.label.toLowerCase().includes(q) ||
      e.slug.toLowerCase().includes(q) ||
      (e.category ?? '').toLowerCase().includes(q) ||
      (e.description ?? '').toLowerCase().includes(q)
    ) {
      return true;
    }
    return (e.variants ?? []).some(
      (v) => v.label.toLowerCase().includes(q) || v.slug.toLowerCase().includes(q),
    );
  }

  let filtered = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (!q) return catalog;
    return catalog.filter((e) => entryMatches(e, q));
  });

  // Top level = category groups; a variant family is a single
  // expandable entry *within* a category that owns the shared
  // thumbnail/sample; its variants nest under it.
  const OTHER = 'Other';
  let groups = $derived.by(() => {
    const by = new Map<string, CatalogEntry[]>();
    for (const e of filtered) {
      const k = e.category ?? OTHER;
      (by.get(k) ?? by.set(k, []).get(k)!).push(e);
    }
    return [...by.entries()]
      .sort(([a], [b]) => (a === OTHER ? 1 : b === OTHER ? -1 : a.localeCompare(b)))
      .map(([category, entries]) => ({ category, entries }));
  });

  // Two independent collapse axes: category groups and family rows.
  // A search forces everything open so matches are never hidden.
  let openGroups = $state<Record<string, boolean>>({});
  let openFamilies = $state<Record<string, boolean>>({});
  let searching = $derived(query.trim().length > 0);

  function groupOpen(category: string, idx: number): boolean {
    if (searching) return true;
    return openGroups[category] ?? idx === 0;
  }
  function toggleGroup(category: string, idx: number) {
    openGroups[category] = !(openGroups[category] ?? idx === 0);
  }
  function familyOpen(slug: string): boolean {
    return searching || (openFamilies[slug] ?? false);
  }
  function toggleFamily(slug: string) {
    openFamilies[slug] = !(openFamilies[slug] ?? false);
  }

  function toggleSample(slug: string, url: string, ev: Event) {
    ev.stopPropagation();
    if (playingSlug === slug && audio) {
      audio.pause();
      audio = null;
      playingSlug = null;
      return;
    }
    if (audio) {
      audio.pause();
      audio = null;
    }
    const a = new Audio(url);
    const clear = () => {
      if (audio === a) {
        audio = null;
        playingSlug = null;
      }
    };
    a.addEventListener('ended', clear);
    a.addEventListener('error', clear);
    void a.play();
    audio = a;
    playingSlug = slug;
  }
</script>

<div
  class="flex h-full w-full flex-col border-r border-slate-800 bg-[color:var(--color-bg)] text-xs"
>
  <div class="flex items-center justify-between border-b border-slate-800 px-2 py-1">
    <span class="font-semibold text-[color:var(--color-muted)]">Signal Catalog</span>
    <span class="text-[10px] text-[color:var(--color-muted)]">{presetCount} presets</span>
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
      {#each groups as g, gi (g.category)}
        <div class="border-b border-slate-900">
          <button
            type="button"
            class="flex w-full items-center justify-between px-2 py-1 text-left text-xs font-bold uppercase tracking-wide text-[color:var(--color-fg)] hover:bg-slate-800/50"
            onclick={() => toggleGroup(g.category, gi)}
          >
            <span>{g.category}</span>
            <span class="font-mono text-slate-600"
              >{groupOpen(g.category, gi) ? '▾' : '▸'} {g.entries.length}</span
            >
          </button>
          {#if groupOpen(g.category, gi)}
            <ul>
              {#each g.entries as entry (entry.slug)}
                <li class="border-t border-slate-900">
                  <!-- Row is a div + role/tabindex (not <button>) so the
                       sigwiki image link and sample button can nest
                       without a button-in-button hydration warning. -->
                  <div
                    role="button"
                    tabindex="0"
                    aria-pressed={activeSlug === entry.slug}
                    class="flex w-full cursor-pointer flex-col gap-1 px-2 py-1.5 text-left hover:bg-slate-800/70"
                    class:active={activeSlug === entry.slug}
                    onclick={() =>
                      entry.variants ? toggleFamily(entry.slug) : onPick?.(entry.slug)}
                    onkeydown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        if (entry.variants) toggleFamily(entry.slug);
                        else onPick?.(entry.slug);
                      }
                    }}
                  >
                    <span class="flex items-baseline justify-between gap-2">
                      <span class="truncate font-medium text-slate-200">{entry.label}</span>
                      {#if entry.variants}
                        <span class="shrink-0 font-mono text-[10px] text-slate-500"
                          >{familyOpen(entry.slug) ? '▾' : '▸'} {entry.variants.length}</span
                        >
                      {:else}
                        <span class="shrink-0 font-mono text-[10px] text-slate-500"
                          >{entry.slug}</span
                        >
                      {/if}
                    </span>

                    {#if entry.signalWikiImageUrl || entry.description}
                      <div class="flex gap-2">
                        {#if entry.signalWikiImageUrl}
                          {#if entry.signalWikiUrl}
                            <a
                              href={entry.signalWikiUrl}
                              target="_blank"
                              rel="noopener noreferrer"
                              onclick={(e) => e.stopPropagation()}
                              class="shrink-0"
                              title="Open sigidwiki page"
                            >
                              <img
                                src={entry.signalWikiImageUrl}
                                alt="{entry.label} waterfall"
                                class="h-12 w-16 rounded border border-slate-800 object-cover"
                                loading="lazy"
                              />
                            </a>
                          {:else}
                            <img
                              src={entry.signalWikiImageUrl}
                              alt="{entry.label} waterfall"
                              class="h-12 w-16 shrink-0 rounded border border-slate-800 object-cover"
                              loading="lazy"
                            />
                          {/if}
                        {/if}
                        {#if entry.description}
                          <span class="line-clamp-3 text-[10px] text-[color:var(--color-muted)]">
                            {entry.description}
                          </span>
                        {/if}
                      </div>
                    {/if}

                    <span class="flex flex-wrap items-center gap-1 text-[9px] text-slate-600">
                      {#each entry.environments as env (env)}
                        <span class="rounded border border-slate-800 px-1">{env}</span>
                      {/each}
                      {#if entry.sampleUrl}
                        <button
                          type="button"
                          onclick={(e) => toggleSample(entry.slug, entry.sampleUrl!, e)}
                          class="rounded border px-1 text-[9px]"
                          class:playing={playingSlug === entry.slug}
                          title="Preview a representative sample of this signal"
                        >
                          {playingSlug === entry.slug ? '⏸ sample' : '▶ sample'}
                        </button>
                      {/if}
                      {#if entry.signalWikiUrl}
                        <a
                          href={entry.signalWikiUrl}
                          target="_blank"
                          rel="noopener noreferrer"
                          onclick={(e) => e.stopPropagation()}
                          class="rounded border border-slate-700 px-1 text-slate-400 hover:border-slate-500 hover:text-slate-200"
                          title="Signal Identification Guide"
                        >
                          wiki ↗
                        </a>
                      {/if}
                    </span>
                  </div>

                  {#if entry.variants && familyOpen(entry.slug)}
                    <ul class="border-t border-slate-900 bg-slate-950/40">
                      {#each entry.variants as v (v.slug)}
                        <li>
                          <button
                            type="button"
                            class="flex w-full items-center justify-between gap-2 py-1 pl-5 pr-2 text-left hover:bg-slate-800/70"
                            class:active={activeSlug === v.slug}
                            onclick={() => onPick?.(v.slug)}
                          >
                            <span class="truncate text-slate-300">
                              {v.label}
                              {#if v.isDefault}
                                <span class="text-[9px] text-slate-500">• default</span>
                              {/if}
                            </span>
                            <span class="shrink-0 font-mono text-[10px] text-slate-600"
                              >{v.slug}</span
                            >
                          </button>
                        </li>
                      {/each}
                    </ul>
                  {/if}
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      {/each}
    {/if}
  </div>
</div>

<style>
  .active {
    background: rgba(125, 211, 252, 0.1);
  }
  .active :global(.font-medium),
  .active.active {
    color: #7dd3fc;
  }
  /* Sample button: muted by default, accent when playing. */
  button.rounded.border:not(.playing) {
    border-color: rgb(51 65 85);
    color: rgb(148 163 184);
  }
  button.rounded.border:not(.playing):hover {
    border-color: rgb(100 116 139);
    color: rgb(226 232 240);
  }
  button.rounded.border.playing {
    border-color: rgb(125 211 252);
    color: rgb(125 211 252);
  }
</style>
