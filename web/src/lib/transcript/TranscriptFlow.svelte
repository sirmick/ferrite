<script lang="ts">
  // TranscriptFlow — the one transcript renderer, reused wherever speech
  // shows up as text: the analog-voice Transcript, the digital-voice
  // Transcript (speakers from the radio), and the AI panel's compact
  // cards. Layout decisions live in `flow.ts` (pure, tested); this only
  // paints them.
  //
  //   ┃ KR1JOE  TG 31068   12:04:31 │ cq cq this is …
  //   ┃                            │ (new paragraph after a real pause)
  //   ──────── speaker change ────────   ← unknown-speaker turn
  //               — 3 min —               ← long silence
  //
  // A coloured left rule + label per speaker block (colour assigned by
  // the server, so every view agrees), graded pause spacing, dimmed
  // low-confidence words, follow-the-tail with "↓ live" (wide mode).

  import { buildFlow, type Block, type FlowItem, type TLine } from './flow';
  import { groupColour, speakerColour, NEUTRAL } from './palette';

  interface Props {
    lines: ReadonlyArray<TLine>;
    /** Colour blocks by speaker (default) or by talkgroup. */
    colourBy?: 'speaker' | 'group';
    /** Small, non-scrolling rendering of the last `maxLines` lines. */
    compact?: boolean;
    maxLines?: number;
    /** Per-line time · frequency · confidence (the old log view). */
    details?: boolean;
    showRaw?: boolean;
    /** Highlight one speaker / group key; others dim. */
    focusKey?: string;
    placeholder?: string;
  }

  let {
    lines,
    colourBy = 'speaker',
    compact = false,
    maxLines,
    details = false,
    showRaw = false,
    focusKey,
    placeholder = 'Nothing yet.',
  }: Props = $props();

  let shown = $derived(maxLines !== undefined ? lines.slice(-maxLines) : lines);
  let items: FlowItem[] = $derived(buildFlow(shown));

  function colourOf(b: Block): string {
    if (!b.speaker) return NEUTRAL;
    return colourBy === 'group' ? groupColour(b.speaker.group) : speakerColour(b.speaker.colour);
  }
  function focusOf(b: Block): string | undefined {
    if (!b.speaker) return undefined;
    return colourBy === 'group' ? b.speaker.group : b.speaker.key;
  }
  /** Split a block's lines into paragraphs at `para` separators. */
  function paragraphs(b: Block): Array<Array<{ line: TLine; sep: string }>> {
    const out: Array<Array<{ line: TLine; sep: string }>> = [];
    for (const l of b.lines) {
      if (out.length === 0 || l.sep === 'para' || details) out.push([]);
      out[out.length - 1].push(l);
    }
    return out;
  }
  const hms = (ms: number) => new Date(ms).toLocaleTimeString([], { hour12: false });
  const mhz = (hz: number | null | undefined) =>
    hz === null || hz === undefined ? '' : `${(hz / 1e6).toFixed(4)}`;
  function tokClass(p: number): string {
    if (p >= 0.7) return 'text-slate-200';
    if (p >= 0.45) return 'text-slate-400';
    return 'text-amber-500/70 underline decoration-dotted';
  }

  let scroller: HTMLDivElement | undefined = $state();
  let following = $state(true);
  function onScroll(): void {
    if (!scroller) return;
    following = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 40;
  }
  function jumpToLive(): void {
    following = true;
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  }
  $effect(() => {
    void items.length;
    void shown.length;
    if (!compact && following && scroller) scroller.scrollTop = scroller.scrollHeight;
  });
</script>

{#snippet lineText(line: TLine)}
  {#if line.kind === 'placeholder'}
    <span class="italic text-slate-500">({line.text})</span>
  {:else if showRaw && line.rawText}
    <span class="italic text-slate-400">{line.rawText}</span>
  {:else if line.tokens && line.tokens.length > 0}
    {#each line.tokens as tk, i (i)}<span class={tokClass(tk.p)}>{tk.text}</span>{/each}
  {:else}
    <span>{line.text}</span>
  {/if}
{/snippet}

{#snippet body()}
  {#if items.length === 0}
    <p class="px-1 py-4 text-slate-600">{placeholder}</p>
  {/if}
  {#each items as it (it.key)}
    {#if it.type === 'gap'}
      <div class="my-2 text-center text-[10px] tracking-wide text-slate-600">{it.label}</div>
    {:else if it.type === 'turn'}
      <div class="my-1 flex items-center gap-2 text-[9px] uppercase tracking-wide text-slate-600">
        <span class="h-px flex-1 bg-slate-800"></span><span>speaker change</span><span
          class="h-px flex-1 bg-slate-800"
        ></span>
      </div>
    {:else}
      {@const colour = colourOf(it)}
      {@const fk = focusOf(it)}
      <div
        class="flex gap-2 border-l-[3px] py-1 pl-2 transition-opacity"
        class:opacity-35={focusKey !== undefined && fk !== focusKey}
        style:border-color={colour}
      >
        {#if it.speaker || !compact}
          <div
            class="shrink-0 font-mono leading-tight {compact
              ? 'w-24 text-[10px]'
              : 'w-36 text-[11px]'}"
          >
            {#if it.speaker}
              <div class="truncate font-semibold" style:color={colour} title={it.speaker.label}>
                {it.speaker.label}
              </div>
              {#if it.speaker.group}
                <div class="truncate text-[10px] text-slate-500">TG {it.speaker.group}</div>
              {/if}
            {/if}
            {#if !compact}
              <div class="text-[10px] text-slate-600">{hms(it.startMs)}</div>
            {/if}
          </div>
        {/if}
        <div
          class="min-w-0 max-w-[90ch] flex-1 {compact
            ? 'text-[11px]'
            : 'text-xs'} leading-relaxed text-slate-300"
        >
          {#each paragraphs(it) as para, pi (pi)}
            <p class:mt-2={pi > 0 && !details}>
              {#if details}
                {@const l = para[0].line}
                <span class="mr-2 font-mono text-[10px] text-slate-600"
                  >{hms(l.atMs)}{l.vfoHz ? ` · ${mhz(l.vfoHz)}` : ''}{l.confidence !== undefined
                    ? ` · ${Math.round(l.confidence * 100)}%`
                    : ''}</span
                >
              {/if}
              {#each para as { line }, li (line.id)}{#if li > 0}{' '}{/if}{@render lineText(
                  line,
                )}{/each}
            </p>
          {/each}
        </div>
      </div>
    {/if}
  {/each}
{/snippet}

{#if compact}
  <div class="flex flex-col gap-0.5">{@render body()}</div>
{:else}
  <div class="relative flex h-full min-h-0 flex-col">
    <div bind:this={scroller} onscroll={onScroll} class="min-h-0 flex-1 overflow-y-auto px-2 py-1">
      {@render body()}
    </div>
    {#if !following}
      <button
        type="button"
        class="absolute bottom-2 right-3 rounded border border-sky-700 bg-[color:var(--color-bg)] px-2 py-0.5 text-[10px] text-sky-300 hover:border-sky-500"
        onclick={jumpToLive}>↓ live</button
      >
    {/if}
  </div>
{/if}
