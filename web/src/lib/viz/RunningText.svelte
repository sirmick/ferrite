<script lang="ts">
  // RunningText — a continuous, wrapping text pane for keyboard-mode
  // decoders (fldigi's Rx pane): one string that grows at the end,
  // monospace, wraps horizontally, honours newlines. The companion of
  // TextConsole (one row per record): per-character decoders are
  // unreadable as a record list, so the Journal shows both. Same
  // follow-the-tail / "↓ live" scroll-lock behaviour as TextConsole.

  interface Props {
    text: string;
    placeholder?: string;
  }

  let { text, placeholder = 'waiting for traffic…' }: Props = $props();

  let scroller: HTMLDivElement | undefined;
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

  $effect(() => {
    void text.length;
    if (following && scroller) scroller.scrollTop = scroller.scrollHeight;
  });
</script>

<div class="relative flex h-full min-h-0 flex-col">
  <div
    bind:this={scroller}
    onscroll={onScroll}
    class="min-h-0 flex-1 overflow-auto px-2 py-1 font-mono text-xs leading-snug"
  >
    {#if text.length === 0}
      <div class="px-1 py-6 text-center text-[color:var(--color-muted)]">{placeholder}</div>
    {:else}
      <pre
        class="m-0 whitespace-pre-wrap break-words font-mono text-slate-200 select-text">{text}<span
          class="animate-pulse text-sky-400 select-none">▎</span
        ></pre>
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
