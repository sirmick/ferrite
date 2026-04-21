<script lang="ts">
  // Generic block-params editor. Walks one PipelineBlock.spec.params and
  // renders the matching control for each kind (range → slider+number,
  // enum_* → select, toggle → checkbox, text → input). Every change
  // fires pipeline.setBlockParam(id, key, value); on success the store
  // refreshes the blocks map so the controls re-reflect the merged
  // server state.
  //
  // The component reads the current value straight off PipelineBlock.values.
  // That keeps it stateless across re-renders — when the upstream re-fetches
  // after a reconfigure, the new values flow in via the prop and the
  // controls follow. No local draft buffer: the UI treats each knob as
  // an immediate "commit on change" and accepts the server's echo.

  import type { PipelineBlock } from '$lib/api/pipelineBlocks';
  import type { ParamSchema } from '$lib/api/flowgraph';
  import { pipeline } from '$lib/pipeline.svelte';

  interface Props {
    block: PipelineBlock;
    /** Hide params whose reconfig_scope would tear down the source.
     *  The receiver panel uses this to keep the source-restart knobs
     *  (sample rate, device, antenna) out of the per-block editor and
     *  into the dedicated source controls. */
    hideSourceRestart?: boolean;
    /** Optional allow-list of param keys. When set, only params whose
     *  key appears here render. */
    only?: string[];
  }

  let { block, hideSourceRestart = false, only }: Props = $props();

  let params = $derived(filterParams(block.spec.params, hideSourceRestart, only));
  let values = $derived((block.values ?? {}) as Record<string, unknown>);
  let pending = $state<string | null>(null);

  function filterParams(all: ParamSchema[], hideRestart: boolean, allow: string[] | undefined) {
    return all.filter((p) => {
      if (hideRestart && p.reconfig_scope === 'sourceRestart') return false;
      if (allow && !allow.includes(p.key)) return false;
      return true;
    });
  }

  function coerceNumber(raw: string, fallback: number): number {
    const n = Number(raw);
    return Number.isFinite(n) ? n : fallback;
  }

  async function commit(key: string, value: unknown) {
    pending = key;
    try {
      await pipeline.setBlockParam(block.id, key, value);
    } finally {
      pending = null;
    }
  }

  function valueOrDefault(p: ParamSchema): unknown {
    const v = values[p.key];
    if (v !== undefined) return v;
    return p.kind === 'toggle' ? p.default : p.default;
  }
</script>

<div class="params">
  {#each params as p (p.key)}
    <label class="row" class:dim={pending === p.key}>
      <span class="label">{p.label}</span>

      {#if p.kind === 'range'}
        {@const current = Number(valueOrDefault(p))}
        <div class="range">
          <input
            type="range"
            min={p.min}
            max={p.max}
            step={p.step}
            value={current}
            disabled={pending === p.key}
            oninput={(e) => commit(p.key, coerceNumber(e.currentTarget.value, current))}
          />
          <input
            type="number"
            min={p.min}
            max={p.max}
            step={p.step}
            value={current}
            disabled={pending === p.key}
            onchange={(e) => commit(p.key, coerceNumber(e.currentTarget.value, current))}
          />
          {#if p.unit}<span class="unit">{p.unit}</span>{/if}
        </div>
      {:else if p.kind === 'enum_numeric'}
        {@const current = Number(valueOrDefault(p))}
        <select
          disabled={pending === p.key}
          onchange={(e) => commit(p.key, coerceNumber(e.currentTarget.value, current))}
        >
          {#each p.values as v (v)}
            <option value={v} selected={v === current}>{v}{p.unit ? ` ${p.unit}` : ''}</option>
          {/each}
        </select>
      {:else if p.kind === 'enum_string'}
        {@const current = String(valueOrDefault(p))}
        <select disabled={pending === p.key} onchange={(e) => commit(p.key, e.currentTarget.value)}>
          {#each p.values as v (v)}
            <option value={v} selected={v === current}>{v}</option>
          {/each}
        </select>
      {:else if p.kind === 'toggle'}
        {@const current = Boolean(valueOrDefault(p))}
        <input
          type="checkbox"
          checked={current}
          disabled={pending === p.key}
          onchange={(e) => commit(p.key, e.currentTarget.checked)}
        />
      {:else if p.kind === 'text'}
        {@const current = String(valueOrDefault(p))}
        <input
          type="text"
          value={current}
          disabled={pending === p.key}
          onchange={(e) => commit(p.key, e.currentTarget.value)}
        />
      {/if}
    </label>
  {/each}
</div>

<style>
  .params {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    font:
      12px ui-monospace,
      SFMono-Regular,
      Menlo,
      monospace;
  }
  .row {
    display: grid;
    grid-template-columns: 10rem 1fr;
    align-items: center;
    gap: 0.6rem;
  }
  .row.dim {
    opacity: 0.55;
  }
  .label {
    color: #aaa;
    user-select: none;
  }
  .range {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .range input[type='range'] {
    flex: 1 1 auto;
    min-width: 6rem;
  }
  .range input[type='number'] {
    width: 6rem;
  }
  .unit {
    color: #888;
  }
</style>
