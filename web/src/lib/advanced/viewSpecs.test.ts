import { describe, expect, it } from 'vitest';

import {
  VIEW_SPECS,
  defaultRightOf,
  paneViewsOf,
  resolveOf,
  type AdvancedCtx,
  type Pane,
} from './viewSpecs';

const paneViews = (c: AdvancedCtx, p: Pane) => paneViewsOf(VIEW_SPECS, c, p);
const resolvePaneView = (c: AdvancedCtx, p: Pane, v: string) => resolveOf(VIEW_SPECS, c, p, v);
const defaultRightView = (c: AdvancedCtx) => defaultRightOf(VIEW_SPECS, c);

const ctx = (sinks: string[], hasAudioSink: boolean): AdvancedCtx => ({
  uiSinks: Object.fromEntries(sinks.map((s) => [s, {}])),
  voiceTranscribeIds: [],
  hasAudioSink,
});
const ids = (c: AdvancedCtx, pane: 'left' | 'right') => paneViews(c, pane).map((v) => v.id);

describe('pane registry', () => {
  it('DMR: transcript on the left, call log on the right (default)', () => {
    const dmr = ctx(['fft', 'dsd'], true);
    expect(ids(dmr, 'left')).toEqual(['transcript']);
    expect(ids(dmr, 'right')).toEqual(['calls:dsd']);
    expect(defaultRightView(dmr)?.id).toBe('calls:dsd');
  });

  it('analog voice: transcript left, nothing extra right', () => {
    const nbfm = ctx(['fft'], true);
    expect(ids(nbfm, 'left')).toEqual(['transcript']);
    expect(ids(nbfm, 'right')).toEqual([]);
    expect(defaultRightView(nbfm)).toBeNull();
  });

  it('a text decoder with audio keeps its journal, not the transcript', () => {
    expect(ids(ctx(['fft', 'pager'], true), 'left')).toEqual(['journal:pager']);
    expect(ids(ctx(['fft', 'fldigi'], true), 'left')).toEqual(['fldigi']);
  });

  it('resolves ids, the legacy `advanced` value, and unknown ids', () => {
    const dmr = ctx(['fft', 'dsd'], true);
    expect(resolvePaneView(dmr, 'left', 'transcript')?.id).toBe('transcript');
    expect(resolvePaneView(dmr, 'left', 'advanced')?.id).toBe('transcript');
    expect(resolvePaneView(dmr, 'left', 'wide')).toBeNull();
    expect(resolvePaneView(dmr, 'left', 'ft8')).toBeNull();
    expect(resolvePaneView(dmr, 'right', 'calls:dsd')?.id).toBe('calls:dsd');
    // A right view is never offered on the left, and vice versa.
    expect(resolvePaneView(dmr, 'left', 'calls:dsd')).toBeNull();
    expect(resolvePaneView(dmr, 'right', 'transcript')).toBeNull();
  });
});
