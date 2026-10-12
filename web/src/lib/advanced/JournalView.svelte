<script lang="ts">
  // Decode Journal for a text-producing decoder kind (pager, EAS,
  // rtl_433, …). Mounted as the main-pane "advanced" view when the
  // active preset wires `ui:<kind>` — the registry passes `kind` and
  // `label` as props. Same shape as the fldigi Journal: one full-height
  // streaming console fed by the always-connected decoder mirror.

  import { journalLines, resetJournal } from '$lib/decoders/journal';
  import TextConsole from '$lib/viz/TextConsole.svelte';

  interface Props {
    kind: string;
    label?: string;
  }

  let { kind, label = 'Journal' }: Props = $props();
  let lines = $derived(journalLines(kind));
</script>

<div class="flex h-full w-full min-h-0 flex-col bg-[color:var(--color-bg)]">
  <header class="panel-head">
    <span>
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">{label}</span>
      <span class="ml-2 text-[color:var(--color-muted)]">{lines.length} decodes</span>
    </span>
    <button
      type="button"
      class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none normal-case text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
      disabled={lines.length === 0}
      title="Clear the decode log (otherwise persists across view toggles)"
      onclick={() => resetJournal(kind)}
    >
      Reset
    </button>
  </header>

  <div class="min-h-0 flex-1">
    <TextConsole {lines} placeholder="waiting for traffic…" />
  </div>
</div>
