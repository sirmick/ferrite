<script lang="ts" module>
  /** One table row: a known emitter, a live detection, or both. */
  export interface EmitterRow {
    key: string | number;
    /** Emitter number, when the detection maps to one. */
    id?: number;
    freq_hz: number;
    width_hz: number;
    level_dbfs: number;
    snr_db?: number;
    band?: string | null;
    label?: string | null;
    status?: string;
    last_seen_ms?: number;
    /** Dim rows not seen recently (spectrum mode). */
    stale?: boolean;
  }
</script>

<script lang="ts">
  // The emitter table shared by the receiver's Strongest Signals pane and
  // the spectrum analyzer's emitter panel: same columns, same number and
  // label for a station in both. Click = pick (tune / marker), double-click
  // = listen, ✎ = label, ⊘ = ignore (spur / not interesting).
  import { emitters } from './store.svelte';
  import IconPencil from '@tabler/icons-svelte-runes/icons/pencil';
  import IconEyeOff from '@tabler/icons-svelte-runes/icons/eye-off';
  import IconEye from '@tabler/icons-svelte-runes/icons/eye';

  interface Props {
    rows: EmitterRow[];
    onPick?: (r: EmitterRow) => void;
    onListen?: (r: EmitterRow) => void;
    /** Highlighted row (e.g. the analyzer's marker). */
    activeHz?: number | null;
    empty?: string;
  }
  let { rows, onPick, onListen, activeHz = null, empty = 'no emitters yet' }: Props = $props();

  let editing = $state<number | null>(null);
  let draft = $state('');

  const fmtMhz = (hz: number) => (hz / 1e6).toFixed(4);
  function fmtBw(hz: number): string {
    if (hz >= 1e6) return `${(hz / 1e6).toFixed(2)}M`;
    if (hz >= 1e3) return `${(hz / 1e3).toFixed(hz >= 1e5 ? 0 : 1)}k`;
    return `${Math.round(hz)}`;
  }
  function ago(ms?: number): string {
    if (!ms) return '';
    const s = Math.max(0, (Date.now() - ms) / 1000);
    if (s < 60) return `${s.toFixed(0)}s`;
    if (s < 3600) return `${(s / 60).toFixed(0)}m`;
    if (s < 86400) return `${(s / 3600).toFixed(0)}h`;
    return `${(s / 86400).toFixed(0)}d`;
  }

  function startEdit(r: EmitterRow) {
    if (r.id === undefined) return;
    editing = r.id;
    draft = r.label ?? '';
  }
  async function saveEdit() {
    if (editing === null) return;
    const id = editing;
    editing = null;
    await emitters.edit(id, { label: draft });
  }
  async function toggleIgnore(r: EmitterRow) {
    if (r.id === undefined) return;
    await emitters.edit(r.id, { status: r.status === 'ignored' ? 'new' : 'ignored' });
  }
  const near = (r: EmitterRow) =>
    activeHz !== null && Math.abs(r.freq_hz - activeHz) <= Math.max(5_000, r.width_hz / 2);
</script>

{#if rows.length === 0}
  <p class="p-3 text-[11px] text-[color:var(--color-muted)]">{empty}</p>
{:else}
  <table class="w-full border-collapse text-[11px] tabular-nums">
    <thead
      class="sticky top-0 bg-[color:var(--color-bg)] text-left text-[color:var(--color-muted)]"
    >
      <tr class="border-b border-slate-800">
        <th class="px-1.5 py-1 font-medium">#</th>
        <th class="px-1.5 py-1 font-medium">MHz</th>
        <th class="px-1.5 py-1 text-right font-medium">dBFS</th>
        <th class="px-1.5 py-1 text-right font-medium">SNR</th>
        <th class="px-1.5 py-1 text-right font-medium">BW</th>
        <th class="px-1.5 py-1 font-medium">Band / label</th>
        <th class="px-1 py-1"></th>
      </tr>
    </thead>
    <tbody>
      {#each rows as r (r.key)}
        <tr
          class="group cursor-pointer border-b border-slate-900 hover:bg-sky-900/30"
          class:opacity-50={r.stale}
          class:row-active={near(r)}
          title="{fmtMhz(r.freq_hz)} MHz{r.band ? ` · ${r.band}` : ''}{r.last_seen_ms
            ? ` · seen ${ago(r.last_seen_ms)} ago`
            : ''} — click to pick, double-click to listen"
          onclick={() => onPick?.(r)}
          ondblclick={() => onListen?.(r)}
        >
          <td class="px-1.5 py-1 font-mono text-slate-500">{r.id ?? ''}</td>
          <td class="px-1.5 py-1 font-mono text-sky-300">{fmtMhz(r.freq_hz)}</td>
          <td class="px-1.5 py-1 text-right">{r.level_dbfs.toFixed(1)}</td>
          <td class="px-1.5 py-1 text-right text-emerald-300">{r.snr_db?.toFixed(0) ?? ''}</td>
          <td class="px-1.5 py-1 text-right text-[color:var(--color-muted)]">
            {fmtBw(r.width_hz)}
          </td>
          <td class="max-w-0 px-1.5 py-1">
            {#if editing !== null && editing === r.id}
              <!-- svelte-ignore a11y_autofocus -->
              <input
                class="w-full rounded border border-sky-600 bg-slate-900 px-1 text-slate-100"
                bind:value={draft}
                autofocus
                onclick={(e) => e.stopPropagation()}
                onkeydown={(e) => {
                  if (e.key === 'Enter') void saveEdit();
                  if (e.key === 'Escape') editing = null;
                }}
                onblur={() => void saveEdit()}
                aria-label="Emitter label"
              />
            {:else if r.label}
              <span class="block truncate text-amber-300">{r.label}</span>
            {:else}
              <span class="block truncate text-[color:var(--color-muted)]"
                >{r.band ?? ''}{r.status === 'new' ? '' : r.status ? ` · ${r.status}` : ''}</span
              >
            {/if}
          </td>
          <td class="whitespace-nowrap px-1 py-1 text-right">
            {#if r.id !== undefined}
              <button
                type="button"
                class="opacity-0 group-hover:opacity-100"
                title="Label emitter #{r.id}"
                aria-label="Label emitter {r.id}"
                onclick={(e) => {
                  e.stopPropagation();
                  startEdit(r);
                }}><IconPencil size={12} /></button
              >
              <button
                type="button"
                class="opacity-0 group-hover:opacity-100"
                title={r.status === 'ignored' ? 'Un-ignore' : 'Ignore (spur / not interesting)'}
                aria-label={r.status === 'ignored' ? 'Un-ignore' : 'Ignore'}
                onclick={(e) => {
                  e.stopPropagation();
                  void toggleIgnore(r);
                }}
                >{#if r.status === 'ignored'}<IconEye size={12} />{:else}<IconEyeOff
                    size={12}
                  />{/if}</button
              >
            {/if}
          </td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}

<style>
  .row-active {
    background: rgba(251, 146, 60, 0.12);
  }
</style>
