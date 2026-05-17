<script lang="ts">
  // TextConsole — the reusable append-only streaming-text primitive
  // for advanced views (APRS messages/bulletins first; RTTY/PSK/CW
  // later). Terminal-style: monospace, newest at the bottom,
  // auto-follows the tail UNLESS the operator has scrolled up (then
  // it holds position and shows a "jump to live" affordance — same
  // scroll-lock behaviour as a log tail / fldigi's Rx pane).
  //
  // Append-only and capped: the caller pushes lines via the `lines`
  // prop (a bounded ring it owns); we never mutate it. No
  // virtualization — the cap keeps the DOM small enough that a plain
  // scroll container is the right amount of machinery, same call as
  // DecodeTable.

  export interface ConsoleLine {
    /** Stable unique key (monotonic seq from the caller's store). */
    id: string;
    /** Optional UTC epoch seconds — rendered as a dim HH:MM:SS gutter. */
    ts?: number;
    text: string;
    /** Optional emphasis (e.g. APRS messages addressed to you). */
    tone?: 'normal' | 'accent' | 'muted';
  }

  interface Props {
    lines: ConsoleLine[];
    placeholder?: string;
  }

  let { lines, placeholder = 'waiting for traffic…' }: Props = $props();

  let scroller: HTMLDivElement | undefined;
  // Auto-follow the tail until the operator scrolls away from the
  // bottom; resume when they scroll back down.
  let following = $state(true);

  function onScroll(): void {
    if (!scroller) return;
    const slack = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    following = slack < 24;
  }

  function jumpToLive(): void {
    following = true;
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  }

  // After each render where new lines arrived, stick to the bottom if
  // we're in follow mode. Keyed on length so it re-runs on append.
  $effect(() => {
    void lines.length;
    if (following && scroller) scroller.scrollTop = scroller.scrollHeight;
  });

  const hms = (ts?: number) => (ts ? new Date(ts * 1000).toISOString().slice(11, 19) : '');
</script>

<div class="relative flex h-full min-h-0 flex-col">
  <div
    bind:this={scroller}
    onscroll={onScroll}
    class="min-h-0 flex-1 overflow-auto px-2 py-1 font-mono text-xs leading-snug"
  >
    {#each lines as line (line.id)}
      <div
        class="flex gap-2 whitespace-pre-wrap break-words {line.tone === 'accent'
          ? 'text-sky-300'
          : line.tone === 'muted'
            ? 'text-[color:var(--color-muted)]'
            : 'text-slate-300'}"
      >
        {#if line.ts}
          <span class="shrink-0 select-none text-[color:var(--color-muted)]">{hms(line.ts)}</span>
        {/if}
        <span class="min-w-0">{line.text}</span>
      </div>
    {/each}
    {#if lines.length === 0}
      <div class="px-1 py-6 text-center text-[color:var(--color-muted)]">{placeholder}</div>
    {/if}
  </div>

  {#if !following}
    <button
      type="button"
      class="absolute bottom-2 right-3 rounded border border-sky-700 bg-[color:var(--color-bg)] px-2 py-0.5 text-[10px] text-sky-300 hover:border-sky-500"
      onclick={jumpToLive}
    >
      ↓ live
    </button>
  {/if}
</div>
