<script lang="ts">
  // Advanced view for the fldigi family (RTTY / PSK31 / CW / MT63 /
  // Olivia / Contestia / DominoEX / Throb / Navtex / RSID auto-detect).
  // Mounted when the operator toggles "fldigi" and the active preset
  // advertises a `ui:fldigi` sink. fldigi is a continuous keyboard-mode
  // text stream, so this is a single full-height streaming console —
  // the shared TextConsole primitive, same as the APRS packet console.
  // The mode badge tracks live: FldigiAuto rewrites it on an RSID hit.

  import { pipeline } from '$lib/pipeline.svelte';
  import { fldigi } from '$lib/fldigi/store.svelte';
  import TextConsole from '$lib/viz/TextConsole.svelte';

  let streamId = $derived(pipeline.uiSinks.fldigi?.stream_id);
  let client = $derived(pipeline.client);

  $effect(() => {
    if (client && streamId !== undefined) {
      fldigi.attach(client, streamId);
      return () => fldigi.detach();
    }
    fldigi.detach();
    return () => {};
  });
</script>

<div class="flex h-full w-full min-h-0 flex-col bg-[color:var(--color-bg)]">
  <header class="panel-head">
    <span>
      <span class="rounded-sm bg-sky-900/50 px-1 font-mono text-sky-300">fldigi</span>
      <span class="ml-2 text-[color:var(--color-muted)]">
        {fldigi.mode || 'waiting'} · {fldigi.lines.length} lines
      </span>
    </span>
    <button
      type="button"
      class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none normal-case text-slate-300 hover:border-slate-500 hover:text-slate-100 disabled:opacity-40"
      disabled={fldigi.lines.length === 0}
      title="Clear the decoded-text console (otherwise persists across view toggles)"
      onclick={() => fldigi.reset()}
    >
      Reset
    </button>
  </header>

  <div class="min-h-0 flex-1">
    <TextConsole lines={fldigi.lines} placeholder="waiting for traffic…" />
  </div>
</div>
