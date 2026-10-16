<script lang="ts">
  // The emitter list's SNR cutoff, shared by every viewer: detections
  // below it found no emitter, and unlabelled emitters whose best SNR is
  // below it are hidden. Commits on change (Enter / blur / arrows).
  import { emitters } from './store.svelte';

  let { label = 'SNR ≥' }: { label?: string } = $props();

  let draft = $state<number | null>(null);
  let value = $derived(draft ?? emitters.config.min_snr_db);
  let error = $state('');

  async function commit(v: number) {
    if (!Number.isFinite(v) || v === emitters.config.min_snr_db) {
      draft = null;
      return;
    }
    try {
      await emitters.setConfig({ min_snr_db: Math.min(80, Math.max(0, v)) });
      error = '';
    } catch (e) {
      error = String(e);
    }
    draft = null;
  }
</script>

<label
  class="flex items-center gap-1 text-[10px] text-[color:var(--color-muted)]"
  title={error ||
    'SNR cutoff: weaker detections found no emitter; unlabelled emitters whose best SNR is below it are hidden (shared, persisted)'}
>
  {label}
  <input
    type="number"
    min="0"
    max="80"
    step="1"
    class="w-10 rounded border bg-slate-900 px-1 py-0 text-right font-mono text-slate-200"
    class:border-red-600={!!error}
    class:border-slate-700={!error}
    {value}
    oninput={(e) => (draft = (e.currentTarget as HTMLInputElement).valueAsNumber)}
    onchange={(e) => void commit((e.currentTarget as HTMLInputElement).valueAsNumber)}
  />
  dB
</label>
