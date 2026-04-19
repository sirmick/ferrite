<script lang="ts">
  import { Dialog } from 'bits-ui';
  import { session } from '$lib/session.svelte';
  import type { PatchSettingsRequest } from '$lib/api/device';

  interface Props {
    open: boolean;
    onClose: () => void;
  }

  let { open = $bindable(), onClose }: Props = $props();

  // Read directly off the store. The store mutates `session.state` in
  // place after each PATCH so the form reflects whatever the server
  // (and ultimately the device) actually accepted.
  let s = $derived(session.state);

  function patch(req: PatchSettingsRequest) {
    void session.patch(req);
  }

  function fmtMHz(hz: number): string {
    return `${(hz / 1e6).toFixed(3)} MHz`;
  }
</script>

<Dialog.Root bind:open onOpenChange={(o) => !o && onClose()}>
  <Dialog.Portal>
    <Dialog.Overlay class="fixed inset-0 z-40 bg-black/50" />
    <Dialog.Content
      class="fixed left-1/2 top-1/2 z-50 w-[28rem] -translate-x-1/2 -translate-y-1/2 rounded-md border border-slate-800 bg-[color:var(--color-bg)] text-[color:var(--color-fg)] shadow-xl"
    >
      <div class="flex flex-col gap-4 p-4">
        <div class="flex items-baseline justify-between">
          <Dialog.Title class="text-sm font-semibold">Session settings</Dialog.Title>
          <Dialog.Description
            class="text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
          >
            {s ? s.source_kind : 'no session'}
          </Dialog.Description>
        </div>

        {#if !s}
          <p class="text-xs text-[color:var(--color-muted)]">
            No active session. Open a device first.
          </p>
        {:else}
          {#if s.source_kind === 'sine' && s.tone_freq_abs_hz !== null}
            <label class="grid gap-1 text-xs">
              <div class="flex justify-between">
                <span class="text-[color:var(--color-muted)]">Tone frequency</span>
                <span class="font-mono">{fmtMHz(s.tone_freq_abs_hz)}</span>
              </div>
              <input
                type="number"
                step="100"
                value={s.tone_freq_abs_hz}
                onchange={(e) =>
                  patch({ tone_freq_abs_hz: Number((e.target as HTMLInputElement).value) })}
                class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
              />
            </label>

            {#if s.amplitude !== null}
              <label class="grid gap-1 text-xs">
                <div class="flex justify-between">
                  <span class="text-[color:var(--color-muted)]">Amplitude</span>
                  <span class="font-mono">{s.amplitude.toFixed(2)}</span>
                </div>
                <input
                  type="range"
                  min="0"
                  max="1"
                  step="0.01"
                  value={s.amplitude}
                  onchange={(e) =>
                    patch({ amplitude: Number((e.target as HTMLInputElement).value) })}
                />
              </label>
            {/if}
          {/if}

          <label class="grid gap-1 text-xs">
            <div class="flex justify-between">
              <span class="text-[color:var(--color-muted)]">Centre frequency</span>
              <span class="font-mono">{fmtMHz(s.center_freq_hz)}</span>
            </div>
            <input
              type="number"
              step="1000"
              value={s.center_freq_hz}
              onchange={(e) =>
                patch({ center_freq_hz: Number((e.target as HTMLInputElement).value) })}
              class="rounded border border-slate-800 bg-slate-900 px-2 py-1"
            />
          </label>

          {#if s.source_kind === 'soapy'}
            {#if s.gain_db !== null}
              <label class="grid gap-1 text-xs">
                <div class="flex justify-between">
                  <span class="text-[color:var(--color-muted)]">Gain</span>
                  <span class="font-mono">{s.gain_db.toFixed(1)} dB</span>
                </div>
                <input
                  type="range"
                  min="0"
                  max="60"
                  step="0.5"
                  value={s.gain_db}
                  disabled={s.agc === true}
                  onchange={(e) => patch({ gain_db: Number((e.target as HTMLInputElement).value) })}
                />
              </label>
            {/if}

            {#if s.agc !== null}
              <label class="flex items-center justify-between gap-2 text-xs">
                <span class="text-[color:var(--color-muted)]">AGC</span>
                <input
                  type="checkbox"
                  checked={s.agc}
                  onchange={(e) => patch({ agc: (e.target as HTMLInputElement).checked })}
                />
              </label>
            {/if}
          {/if}

          <fieldset class="grid gap-2 rounded border border-slate-800 p-2 text-xs">
            <legend
              class="px-1 text-[10px] uppercase tracking-wide text-[color:var(--color-muted)]"
            >
              Display
            </legend>
            <label class="grid gap-1">
              <div class="flex justify-between">
                <span class="text-[color:var(--color-muted)]">Floor</span>
                <span class="font-mono">{s.floor_dbfs.toFixed(0)} dBFS</span>
              </div>
              <input
                type="range"
                min="-150"
                max="-20"
                step="1"
                value={s.floor_dbfs}
                onchange={(e) =>
                  patch({ floor_dbfs: Number((e.target as HTMLInputElement).value) })}
              />
            </label>
            <label class="grid gap-1">
              <div class="flex justify-between">
                <span class="text-[color:var(--color-muted)]">Ceiling</span>
                <span class="font-mono">{s.ceil_dbfs.toFixed(0)} dBFS</span>
              </div>
              <input
                type="range"
                min="-50"
                max="20"
                step="1"
                value={s.ceil_dbfs}
                onchange={(e) => patch({ ceil_dbfs: Number((e.target as HTMLInputElement).value) })}
              />
            </label>
            <label class="grid gap-1">
              <div class="flex justify-between">
                <span class="text-[color:var(--color-muted)]">Smoothing α</span>
                <span class="font-mono">{s.alpha.toFixed(2)}</span>
              </div>
              <input
                type="range"
                min="0"
                max="1"
                step="0.05"
                value={s.alpha}
                onchange={(e) => patch({ alpha: Number((e.target as HTMLInputElement).value) })}
              />
            </label>
          </fieldset>
        {/if}

        <div class="flex justify-end gap-2 pt-2">
          <button
            type="button"
            class="rounded border border-slate-700 px-3 py-1 text-sm"
            onclick={() => {
              open = false;
              onClose();
            }}
          >
            Close
          </button>
        </div>
      </div>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
