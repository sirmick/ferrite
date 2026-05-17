<script lang="ts" generics="Row extends { id: string }">
  // DecodeTable — the reusable sortable-table primitive for advanced
  // mode views. Zero deps, config-driven columns. Selection is
  // two-way (`selectedId` / `onselect`) so it stays coupled to a
  // sibling map: click a row → highlight the station, click the
  // station → the row scrolls into view and highlights.
  //
  // No virtualization: the views that use this (FT8/FT4/WSPR decodes,
  // later ADS-B) keep a bounded, pruned row set by design, so a plain
  // sticky-header scroll table is the right amount of machinery.

  interface Column<R> {
    key: keyof R & string;
    label: string;
    /** Right-align + monospace for numeric columns. */
    numeric?: boolean;
    sortable?: boolean;
    /** Cell renderer; defaults to String(value). */
    format?: (row: R) => string;
  }

  interface Props {
    columns: Column<Row>[];
    rows: Row[];
    selectedId?: string | null;
    onselect?: (id: string | null) => void;
    /** Initial sort; user clicks override it. */
    defaultSort?: { key: keyof Row & string; dir: 'asc' | 'desc' };
    /** Empty-state line. */
    placeholder?: string;
  }

  let {
    columns,
    rows,
    selectedId = $bindable(null),
    onselect,
    defaultSort,
    placeholder = 'waiting for decodes…',
  }: Props = $props();

  // `defaultSort` is an initial seed only — user header clicks own it
  // thereafter, so capturing just the initial value is intended.
  // svelte-ignore state_referenced_locally
  let sortKey = $state<(keyof Row & string) | null>(defaultSort?.key ?? null);
  // svelte-ignore state_referenced_locally
  let sortDir = $state<'asc' | 'desc'>(defaultSort?.dir ?? 'desc');

  function toggleSort(col: Column<Row>): void {
    if (col.sortable === false) return;
    if (sortKey === col.key) {
      sortDir = sortDir === 'asc' ? 'desc' : 'asc';
    } else {
      sortKey = col.key;
      sortDir = 'asc';
    }
  }

  let sorted = $derived.by(() => {
    if (sortKey === null) return rows;
    const k = sortKey;
    const dir = sortDir === 'asc' ? 1 : -1;
    // Copy before sort — never mutate the caller's array.
    return [...rows].sort((a, b) => {
      const av = a[k];
      const bv = b[k];
      if (av === bv) return 0;
      if (av == null) return 1;
      if (bv == null) return -1;
      if (typeof av === 'number' && typeof bv === 'number') return (av - bv) * dir;
      return String(av).localeCompare(String(bv)) * dir;
    });
  });

  function selectRow(id: string): void {
    const next = selectedId === id ? null : id;
    selectedId = next;
    onselect?.(next);
  }

  // When selection arrives from outside (map click), bring the row
  // into view.
  let bodyEl: HTMLDivElement | undefined;
  $effect(() => {
    if (!selectedId || !bodyEl) return;
    bodyEl
      .querySelector(`[data-row-id="${CSS.escape(selectedId)}"]`)
      ?.scrollIntoView({ block: 'nearest' });
  });
</script>

<div class="flex h-full flex-col text-xs">
  <div bind:this={bodyEl} class="min-h-0 flex-1 overflow-auto">
    <table class="w-full border-collapse">
      <thead class="sticky top-0 z-10 bg-[color:var(--color-bg)]">
        <tr class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]">
          {#each columns as col (col.key)}
            <th
              class="select-none border-b border-[color:var(--color-border,#2c3647)] px-2 py-1 {col.numeric
                ? 'text-right'
                : 'text-left'} {col.sortable === false
                ? ''
                : 'cursor-pointer hover:text-slate-200'}"
              onclick={() => toggleSort(col)}
            >
              {col.label}{#if sortKey === col.key}<span class="ml-0.5 text-slate-400"
                  >{sortDir === 'asc' ? '▲' : '▼'}</span
                >{/if}
            </th>
          {/each}
        </tr>
      </thead>
      <tbody>
        {#each sorted as row (row.id)}
          <tr
            data-row-id={row.id}
            class="cursor-pointer border-b border-[color:var(--color-border,#1b2230)] {row.id ===
            selectedId
              ? 'bg-sky-900/40 text-slate-100'
              : 'text-slate-300 hover:bg-slate-800/40'}"
            onclick={() => selectRow(row.id)}
          >
            {#each columns as col (col.key)}
              <td
                class="px-2 py-1 {col.numeric ? 'text-right font-mono tabular-nums' : 'font-mono'}"
              >
                {col.format ? col.format(row) : String(row[col.key] ?? '')}
              </td>
            {/each}
          </tr>
        {/each}
      </tbody>
    </table>

    {#if sorted.length === 0}
      <div class="px-2 py-6 text-center text-[color:var(--color-muted)]">
        {placeholder}
      </div>
    {/if}
  </div>
</div>
