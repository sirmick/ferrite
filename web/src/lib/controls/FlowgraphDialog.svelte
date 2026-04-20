<script lang="ts">
  import { Dialog } from 'bits-ui';
  import PresetJsonView from './PresetJsonView.svelte';
  import type { FlowgraphDoc } from '$lib/flowgraph';
  import {
    fetchBlockSchemas,
    fetchFlowgraph,
    patchFlowgraph,
    type BlockSchema,
    type ParamSchema,
    type ReconfigurePlan,
  } from '$lib/api/flowgraph';

  interface Props {
    open: boolean;
    onClose: () => void;
  }

  let { open = $bindable(), onClose }: Props = $props();

  let doc = $state<FlowgraphDoc | null>(null);
  let schemas = $state<BlockSchema[]>([]);
  let status = $state<'idle' | 'loading' | 'ready' | 'applying' | 'error'>('idle');
  let errorMessage = $state<string | null>(null);
  let lastPlan = $state<ReconfigurePlan | null>(null);
  let formValues = $state<Record<string, Record<string, unknown>>>({});

  type Tab = 'form' | 'json';
  let tab = $state<Tab>('form');

  $effect(() => {
    if (open) {
      void load();
    } else {
      lastPlan = null;
      errorMessage = null;
    }
  });

  async function load() {
    status = 'loading';
    errorMessage = null;
    try {
      const [d, s] = await Promise.all([fetchFlowgraph(), fetchBlockSchemas()]);
      if (d === null) {
        status = 'error';
        errorMessage = 'ferrited is not in preset mode; flowgraph is unavailable';
        return;
      }
      doc = d;
      schemas = s;
      formValues = seedFormValues(d, s);
      status = 'ready';
    } catch (err) {
      status = 'error';
      errorMessage = err instanceof Error ? err.message : String(err);
    }
  }

  function seedFormValues(
    d: FlowgraphDoc,
    s: BlockSchema[],
  ): Record<string, Record<string, unknown>> {
    const schemaByType = new Map(s.map((x) => [x.type_name, x]));
    const out: Record<string, Record<string, unknown>> = {};
    for (const [id, inst] of Object.entries(d.blocks ?? {})) {
      const spec = schemaByType.get(inst.type);
      if (!spec) continue;
      const live = (inst.params ?? {}) as Record<string, unknown>;
      const perBlock: Record<string, unknown> = {};
      for (const p of spec.params) {
        perBlock[p.key] = live[p.key] ?? defaultFor(p);
      }
      out[id] = perBlock;
    }
    return out;
  }

  function defaultFor(p: ParamSchema): unknown {
    switch (p.kind) {
      case 'range':
      case 'enum_numeric':
      case 'toggle':
      case 'text':
      case 'enum_string':
        return p.default;
    }
  }

  type Entry = { id: string; type: string; spec: BlockSchema };

  let entries = $derived.by<Entry[]>(() => {
    if (!doc) return [];
    const byType = new Map(schemas.map((s) => [s.type_name, s]));
    const out: Entry[] = [];
    for (const [id, inst] of Object.entries(doc.blocks ?? {})) {
      const spec = byType.get(inst.type);
      if (!spec) continue;
      // Source-like blocks are configured via the Source dialog; this
      // dialog covers the rest.
      if (spec.placement === 'native') continue;
      out.push({ id, type: inst.type, spec });
    }
    out.sort((a, b) => a.id.localeCompare(b.id));
    return out;
  });

  function buildPatchedDoc(): FlowgraphDoc {
    if (!doc) throw new Error('no doc loaded');
    const blocks: Record<string, { type: string; placement?: string; params?: unknown }> = {};
    for (const [id, inst] of Object.entries(doc.blocks ?? {})) {
      const merged = { ...(inst.params ?? {}), ...(formValues[id] ?? {}) };
      blocks[id] = {
        type: inst.type,
        ...(inst.placement ? { placement: inst.placement } : {}),
        params: merged,
      };
    }
    return { ...doc, blocks } as FlowgraphDoc;
  }

  async function apply() {
    status = 'applying';
    errorMessage = null;
    try {
      const plan = await patchFlowgraph(buildPatchedDoc());
      lastPlan = plan;
      // Re-seed from the newly-applied doc so the form reflects what
      // the server actually accepted.
      await load();
    } catch (err) {
      status = 'error';
      errorMessage = err instanceof Error ? err.message : String(err);
    }
  }

  function close() {
    open = false;
    onClose();
  }
</script>

<Dialog.Root bind:open onOpenChange={(o) => !o && onClose()}>
  <Dialog.Portal>
    <Dialog.Overlay class="fixed inset-0 z-40 bg-black/50" />
    <Dialog.Content
      class="fixed left-1/2 top-1/2 z-50 w-[30rem] max-h-[85dvh] overflow-y-auto -translate-x-1/2 -translate-y-1/2 rounded-md border border-slate-800 bg-[color:var(--color-bg)] text-[color:var(--color-fg)] shadow-xl"
    >
      <div class="flex flex-col gap-4 p-4">
        <div class="flex items-baseline justify-between">
          <Dialog.Title class="text-sm font-semibold">Flowgraph</Dialog.Title>
          <Dialog.Description
            class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
          >
            {doc?.name ?? ''}
          </Dialog.Description>
        </div>

        <div class="flex border-b border-slate-800 text-xs">
          <button
            type="button"
            class="px-3 py-1"
            class:tab-active={tab === 'form'}
            class:tab-idle={tab !== 'form'}
            onclick={() => (tab = 'form')}>Form</button
          >
          <button
            type="button"
            class="px-3 py-1"
            class:tab-active={tab === 'json'}
            class:tab-idle={tab !== 'json'}
            onclick={() => (tab = 'json')}>JSON</button
          >
        </div>

        {#if tab === 'json'}
          <PresetJsonView refreshKey={open} />
        {:else if status === 'loading'}
          <p class="text-xs text-[color:var(--color-muted)]">Loading…</p>
        {:else if status === 'error'}
          <p class="text-xs text-rose-400">{errorMessage}</p>
        {:else if entries.length === 0}
          <p class="text-xs text-[color:var(--color-muted)]">
            No configurable blocks in this preset.
          </p>
        {:else}
          {#each entries as entry (entry.id)}
            <section
              class="flex flex-col gap-3 border-t border-slate-800 pt-3 first:border-t-0 first:pt-0"
            >
              <header class="flex items-baseline justify-between">
                <span class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
                  {entry.id}
                </span>
                <span class="font-mono text-[10px] text-[color:var(--color-muted)]">
                  {entry.type}
                </span>
              </header>

              {#each entry.spec.params as param (param.key)}
                {@const value = formValues[entry.id]?.[param.key]}
                <label class="grid gap-1 text-xs">
                  <div class="flex justify-between">
                    <span class="text-[color:var(--color-muted)]">{param.label}</span>
                    <span class="font-mono text-[10px] text-[color:var(--color-muted)]">
                      {param.reconfig_scope}
                    </span>
                  </div>

                  {#if param.kind === 'range'}
                    <input
                      type="number"
                      min={param.min}
                      max={param.max}
                      step={param.step}
                      class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                      value={value as number}
                      oninput={(e) => {
                        const n = (e.currentTarget as HTMLInputElement).valueAsNumber;
                        if (Number.isFinite(n)) formValues[entry.id][param.key] = n;
                      }}
                    />
                    <div class="flex justify-between text-[10px] text-[color:var(--color-muted)]">
                      <span>{param.min}{param.unit ? ` ${param.unit}` : ''}</span>
                      <span>{param.max}{param.unit ? ` ${param.unit}` : ''}</span>
                    </div>
                  {:else if param.kind === 'enum_numeric'}
                    <select
                      class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                      value={value as number}
                      onchange={(e) => {
                        const n = Number((e.currentTarget as HTMLSelectElement).value);
                        formValues[entry.id][param.key] = n;
                      }}
                    >
                      {#each param.values as v (v)}
                        <option value={v}>{v}{param.unit ? ` ${param.unit}` : ''}</option>
                      {/each}
                    </select>
                  {:else if param.kind === 'enum_string'}
                    <select
                      class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                      value={value as string}
                      onchange={(e) => {
                        formValues[entry.id][param.key] = (
                          e.currentTarget as HTMLSelectElement
                        ).value;
                      }}
                    >
                      {#each param.values as v (v)}
                        <option value={v}>{v}</option>
                      {/each}
                    </select>
                  {:else if param.kind === 'toggle'}
                    <input
                      type="checkbox"
                      checked={value as boolean}
                      onchange={(e) => {
                        formValues[entry.id][param.key] = (
                          e.currentTarget as HTMLInputElement
                        ).checked;
                      }}
                    />
                  {:else if param.kind === 'text'}
                    <input
                      type="text"
                      class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
                      value={value as string}
                      oninput={(e) => {
                        formValues[entry.id][param.key] = (
                          e.currentTarget as HTMLInputElement
                        ).value;
                      }}
                    />
                  {/if}
                </label>
              {/each}
            </section>
          {/each}

          {#if lastPlan}
            <p class="text-[11px] text-emerald-400">
              Applied: {lastPlan.noop ? 'no-op' : lastPlan.overall} — {lastPlan.changes.length} param
              change{lastPlan.changes.length === 1 ? '' : 's'}, {lastPlan.structural_count} structural
            </p>
          {/if}
        {/if}

        <div class="flex justify-end gap-2 pt-2">
          <button
            type="button"
            class="rounded border border-slate-700 px-3 py-1 text-sm"
            onclick={close}
          >
            Close
          </button>
          {#if tab === 'form'}
            <button
              type="button"
              class="rounded bg-[color:var(--color-accent)] px-3 py-1 text-sm font-semibold text-slate-900 disabled:opacity-50"
              disabled={status !== 'ready' || entries.length === 0}
              onclick={apply}
            >
              {status === 'applying' ? 'Applying…' : 'Apply'}
            </button>
          {/if}
        </div>
      </div>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>

<style>
  .tab-active {
    border-bottom: 2px solid var(--color-accent);
    color: var(--color-fg);
    font-weight: 600;
  }
  .tab-idle {
    color: var(--color-muted);
    border-bottom: 2px solid transparent;
  }
  .tab-idle:hover {
    color: var(--color-fg);
  }
</style>
