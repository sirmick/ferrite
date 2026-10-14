<script lang="ts">
  // Digital-voice call log — the narrow right-pane companion to the
  // Transcript. One compact line per event from the `dsd` decoder kind
  // (DmrDecode / DsdHelper):
  //
  //   ┃ 12:04:31 S2 TG31068 ← KR1JOE          call
  //   ┃ 12:04:58 S2 TG31068 ← KR1JOE  27.1 s  end
  //
  // The left rule uses the speaker's server-assigned colour, the same one
  // the Transcript uses for that radio, so the two panes line up by eye.

  import { decoders } from '$lib/decoders/store.svelte';
  import { resetJournal } from '$lib/decoders/journal';
  import { speakerColour, NEUTRAL } from '$lib/transcript/palette';
  import { latestLabels } from '$lib/transcript/flow';

  interface Props {
    kind?: string;
    label?: string;
  }
  let { kind = 'dsd', label = 'Calls' }: Props = $props();

  interface Rec {
    event?: string;
    protocol?: string;
    slot?: number;
    tgt?: string;
    src?: string;
    enc?: boolean;
    text?: string;
    speaker?: { key?: string; label?: string; colour?: number };
  }
  interface Row {
    id: number;
    at: number;
    event: string;
    colour: string;
    main: string;
    note: string;
    tone: 'call' | 'end' | 'muted';
  }

  function row(seq: number, atMs: number, d: Rec, labels: ReadonlyMap<string, string>): Row {
    const event = d.event ?? '';
    // Calls logged before the talker alias arrived show the newest label.
    const who = (d.speaker?.key && labels.get(d.speaker.key)) ?? d.speaker?.label ?? d.src ?? '';
    const colour = d.speaker ? speakerColour(d.speaker.colour) : NEUTRAL;
    const head = `${d.slot ? `S${d.slot} ` : ''}${d.tgt ? `TG${d.tgt} ` : ''}`;
    if (event === 'call') {
      return {
        id: seq,
        at: atMs,
        event,
        colour,
        main: `${head}← ${who}`,
        note: d.enc ? 'ENC' : (d.protocol ?? ''),
        tone: 'call',
      };
    }
    if (event === 'end') {
      const secs = /\(([\d.]+) s\)/.exec(d.text ?? '')?.[1];
      return {
        id: seq,
        at: atMs,
        event,
        colour,
        main: `${head}← ${who}`,
        note: secs ? `${secs} s` : 'end',
        tone: 'end',
      };
    }
    if (event === 'alias') {
      return {
        id: seq,
        at: atMs,
        event,
        colour,
        main: `alias ${d.text ?? ''}`,
        note: '',
        tone: 'muted',
      };
    }
    return {
      id: seq,
      at: atMs,
      event,
      colour: NEUTRAL,
      main: d.text ?? event,
      note: '',
      tone: 'muted',
    };
  }

  let rows = $derived.by(() => {
    const recent = decoders.kind(kind).recent;
    const labels = latestLabels(recent);
    return recent.map((r) => row(r.seq, r.at_ms, (r.data ?? {}) as Rec, labels));
  });
  let calls = $derived(rows.filter((r) => r.event === 'call').length);
  const hms = (ms: number) => new Date(ms).toLocaleTimeString([], { hour12: false });

  let scroller: HTMLDivElement | undefined = $state();
  let following = $state(true);
  function onScroll(): void {
    if (!scroller) return;
    following = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 24;
  }
  $effect(() => {
    void rows.length;
    if (following && scroller) scroller.scrollTop = scroller.scrollHeight;
  });
</script>

<div
  class="flex h-full w-full min-h-0 flex-col border-l border-slate-800 bg-[color:var(--color-bg)]"
>
  <header class="panel-head">
    <span>
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">{label}</span>
      <span class="ml-2 text-[color:var(--color-muted)]">{calls} calls</span>
    </span>
    <button
      type="button"
      class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none normal-case text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
      disabled={rows.length === 0}
      title="Clear the call log"
      onclick={() => resetJournal(kind)}>Reset</button
    >
  </header>
  <div
    bind:this={scroller}
    onscroll={onScroll}
    class="min-h-0 flex-1 overflow-y-auto py-1 font-mono text-[11px] leading-snug"
  >
    {#each rows as r (r.id)}
      <div
        class="flex gap-2 border-l-[3px] px-2 py-px"
        style:border-color={r.tone === 'muted' ? 'transparent' : r.colour}
        class:text-slate-200={r.tone === 'call'}
        class:text-slate-400={r.tone === 'end'}
        class:text-slate-600={r.tone === 'muted'}
      >
        <span class="shrink-0 text-slate-600">{hms(r.at)}</span>
        <span
          class="min-w-0 flex-1 truncate"
          style:color={r.tone === 'call' ? r.colour : undefined}
          title={r.main}>{r.main}</span
        >
        <span class="shrink-0">{r.note}</span>
      </div>
    {/each}
    {#if rows.length === 0}
      <div class="px-2 py-6 text-center text-[color:var(--color-muted)]">waiting for calls…</div>
    {/if}
  </div>
</div>
