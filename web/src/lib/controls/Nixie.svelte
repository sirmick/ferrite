<script lang="ts">
  import { bumpDigit, digitsOfHz, NIXIE_DIGITS, parseFreq } from './freqParse';

  interface Props {
    /** Current frequency in Hz. */
    hz: number;
    /** Called with a new Hz value when the user commits a valid edit. */
    onCommit: (hz: number) => void;
    /** Disable interaction (no click, no wheel, no keyboard). */
    disabled?: boolean;
    /** Colour palette. `orange` for the VFO (the frequency you're
     *  listening to); `green` for the SDR centre (the RF tuner LO).
     *  Two distinct tones so the two tubes stay visually unambiguous
     *  when shown side by side. */
    tone?: 'orange' | 'green';
  }

  let { hz, onCommit, disabled = false, tone = 'orange' }: Props = $props();

  let digits = $derived(digitsOfHz(hz));
  // Leading-zero dimming stops at the first non-zero digit.
  let firstNonZero = $derived.by(() => {
    const idx = digits.findIndex((d) => d !== 0);
    return idx === -1 ? digits.length - 1 : idx;
  });

  let editing = $state(false);
  let draft = $state('');
  let error = $state<string | null>(null);
  let inputEl: HTMLInputElement | undefined = $state();
  const digitEls: (HTMLButtonElement | null)[] = $state(
    Array.from({ length: NIXIE_DIGITS }, () => null),
  );

  function startEdit() {
    if (disabled) return;
    draft = (hz / 1e6).toFixed(6).replace(/\.?0+$/, '');
    error = null;
    editing = true;
    queueMicrotask(() => inputEl?.select());
  }

  function commit() {
    const parsed = parseFreq(draft);
    if (!parsed) {
      error = 'invalid frequency';
      return;
    }
    onCommit(parsed.hz);
    editing = false;
  }

  function cancel() {
    editing = false;
    error = null;
  }

  function onEditKey(ev: KeyboardEvent) {
    if (ev.key === 'Enter') commit();
    else if (ev.key === 'Escape') cancel();
  }

  function bump(i: number, delta: number) {
    if (disabled) return;
    const next = bumpDigit(hz, i, delta);
    if (next !== hz) onCommit(next);
  }

  function focusDigit(i: number) {
    const clamped = Math.max(0, Math.min(NIXIE_DIGITS - 1, i));
    digitEls[clamped]?.focus();
  }

  function onDigitWheel(i: number, ev: WheelEvent) {
    if (disabled) return;
    ev.preventDefault();
    bump(i, ev.deltaY < 0 ? +1 : -1);
  }

  function onDigitKey(i: number, ev: KeyboardEvent) {
    if (disabled) return;
    switch (ev.key) {
      case 'ArrowUp':
        ev.preventDefault();
        bump(i, +1);
        return;
      case 'ArrowDown':
        ev.preventDefault();
        bump(i, -1);
        return;
      case 'ArrowLeft':
        ev.preventDefault();
        focusDigit(i - 1);
        return;
      case 'ArrowRight':
        ev.preventDefault();
        focusDigit(i + 1);
        return;
      case 'Enter':
      case 'F2':
        ev.preventDefault();
        startEdit();
        return;
    }
  }

  // Digit positions 0..9 = GHz..Hz. Group separators after positions 0,3,6.
  // Example: 1.234.567.890
  const GROUP_AFTER = new Set([0, 3, 6]);
</script>

<div class="nixie-wrap tone-{tone}">
  {#if editing}
    <input
      bind:this={inputEl}
      bind:value={draft}
      onkeydown={onEditKey}
      onblur={commit}
      class="nixie-input"
      placeholder="e.g. 1.25m, 2.54g, 446.0, 88100000"
      aria-label="frequency entry"
    />
    {#if error}
      <span class="nixie-error">{error}</span>
    {:else}
      <span class="nixie-hint">m=MHz · g=GHz · k=kHz · h=Hz</span>
    {/if}
  {:else}
    <div class="nixie-frame" role="group" aria-label="frequency" ondblclick={startEdit}>
      {#each digits as d, i (i)}
        <button
          type="button"
          bind:this={digitEls[i]}
          class="nixie-digit"
          class:dim={i < firstNonZero}
          onwheel={(ev) => onDigitWheel(i, ev)}
          onkeydown={(ev) => onDigitKey(i, ev)}
          aria-label={`digit ${i}`}
          tabindex={disabled ? -1 : 0}
          {disabled}
        >
          {d}
        </button>
        {#if GROUP_AFTER.has(i)}
          <span class="nixie-sep" class:dim={i < firstNonZero}>.</span>
        {/if}
      {/each}
      <span class="nixie-unit">Hz</span>
    </div>
  {/if}
</div>

<style>
  .nixie-wrap {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
  }
  /* Tone palettes — orange is the VFO (what you're listening to) and
     green is the SDR centre (the tuner LO). Keeping the two contrast
     strongly protects the user from confusing them when both are live. */
  .tone-orange {
    --nixie-bg-inner: #1a0d05;
    --nixie-bg-outer: #0a0504;
    --nixie-border: #2a1a10;
    --nixie-border-hover: #4a2a18;
    --nixie-glow: 255, 140, 40;
    --nixie-digit: #ff9d3a;
    --nixie-digit-dim: #3a1f10;
    --nixie-unit: #a15a2a;
    --nixie-unit-shadow: 180, 80, 20;
    --nixie-hint: rgba(255, 180, 120, 0.5);
  }
  .tone-green {
    --nixie-bg-inner: #051a0a;
    --nixie-bg-outer: #030a05;
    --nixie-border: #0f2e1a;
    --nixie-border-hover: #1b4a2c;
    --nixie-glow: 80, 240, 140;
    --nixie-digit: #58f0a0;
    --nixie-digit-dim: #0f2a18;
    --nixie-unit: #3fa070;
    --nixie-unit-shadow: 30, 160, 90;
    --nixie-hint: rgba(120, 255, 180, 0.5);
  }
  .nixie-frame {
    display: inline-flex;
    align-items: baseline;
    gap: 0.05em;
    padding: 0.2rem 0.5rem;
    border-radius: 4px;
    background: radial-gradient(
      ellipse at center,
      var(--nixie-bg-inner) 0%,
      var(--nixie-bg-outer) 100%
    );
    border: 1px solid var(--nixie-border);
    box-shadow:
      inset 0 0 12px rgba(0, 0, 0, 0.8),
      0 0 1px rgba(var(--nixie-glow), 0.2);
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 1.4rem;
    font-weight: 600;
    line-height: 1;
    letter-spacing: 0.02em;
    color: var(--nixie-digit);
  }
  .nixie-frame:hover {
    border-color: var(--nixie-border-hover);
  }
  .nixie-digit {
    appearance: none;
    background: transparent;
    border: 1px solid transparent;
    padding: 0 0.08em;
    margin: 0;
    font: inherit;
    cursor: ns-resize;
    color: var(--nixie-digit);
    text-shadow:
      0 0 2px var(--nixie-digit),
      0 0 6px rgba(var(--nixie-glow), 0.8),
      0 0 14px rgba(var(--nixie-glow), 0.5),
      0 0 22px rgba(var(--nixie-glow), 0.3);
    border-radius: 2px;
    line-height: 1;
  }
  .nixie-digit:hover:not(:disabled) {
    background: rgba(var(--nixie-glow), 0.08);
  }
  .nixie-digit:focus-visible {
    outline: none;
    border-color: var(--nixie-digit);
    background: rgba(var(--nixie-glow), 0.15);
  }
  .nixie-digit:disabled {
    cursor: default;
    opacity: 0.5;
  }
  .nixie-sep {
    color: var(--nixie-digit);
    text-shadow:
      0 0 3px rgba(var(--nixie-glow), 0.9),
      0 0 8px rgba(var(--nixie-glow), 0.5);
    transform: translateY(-0.15em);
    margin: 0 0.02em;
  }
  .dim {
    color: var(--nixie-digit-dim);
    text-shadow: none;
    opacity: 0.55;
  }
  .nixie-unit {
    font-size: 0.55em;
    margin-left: 0.4em;
    color: var(--nixie-unit);
    text-shadow: 0 0 2px rgba(var(--nixie-unit-shadow), 0.6);
    letter-spacing: 0.1em;
    font-weight: 500;
  }
  .nixie-input {
    background: var(--nixie-bg-outer);
    border: 1px solid var(--nixie-border-hover);
    color: var(--nixie-digit);
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 1rem;
    padding: 0.25rem 0.5rem;
    border-radius: 4px;
    width: 14rem;
    outline: none;
  }
  .nixie-input:focus {
    border-color: var(--nixie-digit);
    box-shadow: 0 0 0 2px rgba(var(--nixie-glow), 0.2);
  }
  .nixie-hint {
    font-size: 0.65rem;
    color: var(--nixie-hint);
  }
  .nixie-error {
    font-size: 0.7rem;
    color: #f87171;
  }
</style>
