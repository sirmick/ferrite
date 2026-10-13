<script lang="ts">
  // Advanced view for the fldigi family (RTTY / PSK31 / CW / MT63 /
  // Olivia / Contestia / DominoEX / Throb / Navtex / RSID auto-detect).
  // Mounted when the operator toggles "fldigi" and the active preset
  // advertises a `ui:fldigi` sink. Two panes over the same decoder
  // mirror (kind `fldigi`):
  //   * Rx text — the continuous running text (fldigi's Rx pane). The
  //     blocks emit one record per *character* (per word for CW), so
  //     this is where a NAVTEX forecast or an RTTY over is readable.
  //   * records — the per-record console (TextConsole), timestamped,
  //     for seeing exactly what arrived when.
  // The mode badge tracks live: FldigiAuto rewrites it on an RSID hit.

  import { fldigi } from '$lib/fldigi/store.svelte';
  import RunningText from '$lib/viz/RunningText.svelte';
  import TextConsole from '$lib/viz/TextConsole.svelte';

  let text = $derived(fldigi.text);
  let lines = $derived(fldigi.lines);
</script>

<div class="flex h-full w-full min-h-0 flex-col bg-[color:var(--color-bg)]">
  <header class="panel-head">
    <span>
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">Journal</span>
      <span class="ml-2 text-[color:var(--color-muted)]">
        {fldigi.mode || 'waiting'} · {lines.length} records · {text.length} chars
      </span>
    </span>
    <button
      type="button"
      class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none normal-case text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
      disabled={lines.length === 0}
      title="Clear the decoded text (otherwise persists across view toggles)"
      onclick={() => fldigi.reset()}
    >
      Reset
    </button>
  </header>

  <div class="min-h-0 flex-[3] border-b border-slate-800">
    <RunningText {text} placeholder="waiting for traffic…" />
  </div>

  <div class="flex min-h-0 flex-[2] flex-col">
    <div
      class="shrink-0 px-2 py-0.5 text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
    >
      records
    </div>
    <div class="min-h-0 flex-1">
      <TextConsole {lines} placeholder="" />
    </div>
  </div>
</div>
