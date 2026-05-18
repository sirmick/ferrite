import { describe, expect, test } from 'vitest';
import { injectVoiceTranscribe, type FlowgraphDoc } from '$lib/flowgraph';

// Browser-side mirror of `runtime/src/inject_voice_transcribe.rs`. These
// cases mirror that crate's tests (`splices_before_audio_sink`,
// `idempotent_and_respects_hand_authored`,
// `no_audio_sink_means_no_injection`) so the two halves can't diverge —
// a divergence is exactly the bug that left transcription dead in the
// browser (the block was injected node-side only).

const audioPreset = (): FlowgraphDoc => ({
  name: 'am',
  blocks: {
    src: { type: 'SoapySource' },
    demod: { type: 'AmDemod' },
    audio: { type: 'AudioSink' },
  },
  wires: [
    ['src.out', 'demod.in'],
    ['demod.out', 'audio.in'],
  ],
});

describe('injectVoiceTranscribe', () => {
  test('splices a VoiceTranscribe tap before every AudioSink', () => {
    const out = injectVoiceTranscribe(audioPreset());

    const vtId = '__voice_transcribe_audio';
    expect(out.blocks?.[vtId]).toEqual({
      type: 'VoiceTranscribe',
      placement: 'browser',
      params: { mode: 'on' },
    });
    // Producer re-pointed through the tap; tap feeds the sink.
    expect(out.wires).toContainEqual(['demod.out', `${vtId}.in`]);
    expect(out.wires).toContainEqual([`${vtId}.out`, 'audio.in']);
    // The original demod→audio wire is re-pointed, not duplicated.
    expect(out.wires).not.toContainEqual(['demod.out', 'audio.in']);
    // Input doc untouched (returns a fresh doc).
    expect(audioPreset().wires).toContainEqual(['demod.out', 'audio.in']);
  });

  test('is idempotent — re-running does not double-inject', () => {
    const once = injectVoiceTranscribe(audioPreset());
    const twice = injectVoiceTranscribe(once);
    expect(twice).toBe(once); // already-present short-circuit
    const vtBlocks = Object.values(twice.blocks ?? {}).filter((b) => b.type === 'VoiceTranscribe');
    expect(vtBlocks).toHaveLength(1);
  });

  test('respects a hand-authored VoiceTranscribe (leaves wiring alone)', () => {
    const hand: FlowgraphDoc = {
      blocks: {
        demod: { type: 'AmDemod' },
        vt: { type: 'VoiceTranscribe', params: { mode: 'on' } },
        audio: { type: 'AudioSink' },
      },
      wires: [
        ['demod.out', 'vt.in'],
        ['vt.out', 'audio.in'],
      ],
    };
    expect(injectVoiceTranscribe(hand)).toBe(hand);
  });

  test('no-op on a preset with no AudioSink (pure decoder)', () => {
    const decoder: FlowgraphDoc = {
      blocks: { src: { type: 'SoapySource' }, ft8: { type: 'Ft8Decode' } },
      wires: [['src.out', 'ft8.in']],
    };
    expect(injectVoiceTranscribe(decoder)).toBe(decoder);
  });

  test('skips an AudioSink with no producer wire', () => {
    const dangling: FlowgraphDoc = {
      blocks: { audio: { type: 'AudioSink' } },
      wires: [],
    };
    const out = injectVoiceTranscribe(dangling);
    expect(Object.keys(out.blocks ?? {})).toEqual(['audio']);
    expect(out.wires).toEqual([]);
  });
});
