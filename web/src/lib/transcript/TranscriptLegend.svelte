<script lang="ts">
  // Who is (or was recently) on the air in this transcript: one chip per
  // speaker — or per talkgroup in "by talkgroup" mode — with its colour,
  // label, talkgroup and how long ago it was heard. Clicking a chip
  // highlights that speaker in the transcript (and again to clear).
  // Colours match TranscriptFlow and the call log: they come from the
  // server's assignment (`speaker.colour`) or the tab-shared group order.

  import type { TLine } from './flow';
  import { groupColour, speakerColour } from './palette';

  interface Props {
    lines: ReadonlyArray<TLine>;
    colourBy: 'speaker' | 'group';
    focusKey?: string;
    /** Chips shown before "+N more". */
    max?: number;
  }

  let { lines, colourBy = $bindable(), focusKey = $bindable(), max = 8 }: Props = $props();

  interface Entry {
    key: string;
    label: string;
    sub?: string;
    colour: string;
    lastMs: number;
    count: number;
  }

  let entries: Entry[] = $derived.by(() => {
    const m = new Map<string, Entry>();
    for (const l of lines) {
      const s = l.speaker;
      if (!s) continue;
      const key = colourBy === 'group' ? (s.group ?? '') : s.key;
      if (!key) continue;
      const e = m.get(key);
      if (e) {
        e.count += 1;
        e.lastMs = Math.max(e.lastMs, l.atMs);
        if (colourBy === 'speaker') e.label = s.label;
      } else {
        m.set(key, {
          key,
          label: colourBy === 'group' ? `TG ${s.group}` : s.label,
          sub: colourBy === 'group' ? undefined : s.group ? `TG ${s.group}` : undefined,
          colour: colourBy === 'group' ? groupColour(s.group) : speakerColour(s.colour),
          lastMs: l.atMs,
          count: 1,
        });
      }
    }
    return [...m.values()].sort((a, b) => b.lastMs - a.lastMs);
  });
  let hasGroups = $derived(lines.some((l) => l.speaker?.group));

  let now = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (now = Date.now()), 5_000);
    return () => clearInterval(t);
  });
  function ago(ms: number): string {
    const s = Math.max(0, Math.round((now - ms) / 1000));
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.round(s / 60)}m`;
    return `${Math.round(s / 3600)}h`;
  }
</script>

{#if entries.length > 0}
  <div class="flex flex-wrap items-center gap-1.5 border-b border-slate-800 px-2 py-1 text-[10px]">
    {#if hasGroups}
      <span class="mr-1 flex overflow-hidden rounded border border-slate-700">
        <button
          type="button"
          class="px-1.5 py-0.5"
          class:bg-slate-700={colourBy === 'speaker'}
          class:text-slate-100={colourBy === 'speaker'}
          class:text-slate-400={colourBy !== 'speaker'}
          onclick={() => {
            colourBy = 'speaker';
            focusKey = undefined;
          }}>by speaker</button
        ><button
          type="button"
          class="px-1.5 py-0.5"
          class:bg-slate-700={colourBy === 'group'}
          class:text-slate-100={colourBy === 'group'}
          class:text-slate-400={colourBy !== 'group'}
          onclick={() => {
            colourBy = 'group';
            focusKey = undefined;
          }}>by talkgroup</button
        >
      </span>
    {/if}
    {#each entries.slice(0, max) as e (e.key)}
      <button
        type="button"
        class="flex items-center gap-1 rounded border px-1.5 py-0.5 hover:border-slate-500"
        class:border-slate-400={focusKey === e.key}
        class:border-slate-800={focusKey !== e.key}
        title="{e.label}{e.sub ? ` · ${e.sub}` : ''} · {e.count} line{e.count === 1
          ? ''
          : 's'} · click to highlight"
        onclick={() => (focusKey = focusKey === e.key ? undefined : e.key)}
      >
        <span class="inline-block h-2 w-2 rounded-full" style:background={e.colour}></span>
        <span class="font-mono" style:color={e.colour}>{e.label}</span>
        {#if e.sub}<span class="text-slate-500">{e.sub}</span>{/if}
        <span class="text-slate-600">{ago(e.lastMs)}</span>
      </button>
    {/each}
    {#if entries.length > max}
      <span class="text-slate-600">+{entries.length - max} more</span>
    {/if}
  </div>
{/if}
