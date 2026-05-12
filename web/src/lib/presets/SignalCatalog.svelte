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

  // Sample preview: at most one entry plays at a time. The Audio
  // element is created lazily on the first click and torn down when
  // playback ends or the user picks a different preset to preview.
  let playingSlug = $state<string | null>(null);
  let audio: HTMLAudioElement | null = null;

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

  function toggleSample(slug: string, url: string, ev: Event) {
    // Don't let the click propagate to the row's preset-load button.
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
    a.addEventListener('ended', () => {
      if (audio === a) {
        audio = null;
        playingSlug = null;
      }
    });
    a.addEventListener('error', () => {
      if (audio === a) {
        audio = null;
        playingSlug = null;
      }
    });
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
          <li class="border-b border-slate-900">
            <!-- Row uses a div + role/tabindex rather than a real <button>
                 so the inline image link and sample-play button can nest
                 without hitting the "button-in-button" SSR/hydration
                 warning. Keyboard activation goes through onKey. -->
            <div
              role="button"
              tabindex="0"
              aria-pressed={activeSlug === entry.slug}
              class="flex w-full cursor-pointer flex-col gap-1 px-2 py-1.5 text-left hover:bg-slate-800/70"
              class:active={activeSlug === entry.slug}
              onclick={() => onPick?.(entry)}
              onkeydown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  onPick?.(entry);
                }
              }}
            >
              <span class="flex items-baseline justify-between gap-2">
                <span class="truncate font-medium text-slate-200">{entry.label}</span>
                <span class="shrink-0 font-mono text-[10px] text-slate-500">{entry.slug}</span>
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
