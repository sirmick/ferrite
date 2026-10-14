import { describe, expect, it } from 'vitest';

import { buildFlow, flowToText, gapLabel, latestLabels, relabel, sepFor, type TLine } from './flow';

const sp = (id: string, group = '31068', colour = 1) => ({
  key: `DMR:${id}`,
  label: id,
  id,
  group,
  colour,
});
let n = 0;
const line = (atMs: number, text: string, extra: Partial<TLine> = {}): TLine => ({
  id: n++,
  atMs,
  text,
  gapMs: 0,
  ...extra,
});

describe('buildFlow', () => {
  it('flows speech on one line and breaks a paragraph only on a real pause', () => {
    expect(sepFor(200)).toBe('space');
    // A recogniser segment boundary (~1.2–2 s of silence) keeps flowing.
    expect(sepFor(1_400)).toBe('space');
    expect(sepFor(2_000)).toBe('space');
    expect(sepFor(3_500)).toBe('para');
    const f = buildFlow([
      line(1_000, 'one'),
      line(1_500, 'two', { gapMs: 200 }),
      line(3_000, 'three', { gapMs: 1_400 }),
      line(9_000, 'four', { gapMs: 4_000 }),
    ]);
    expect(f).toHaveLength(1);
    const b = f[0];
    expect(b.type).toBe('block');
    if (b.type === 'block')
      expect(b.lines.map((l) => l.sep)).toEqual(['space', 'space', 'space', 'para']);
  });

  it('merges a speaker into one block and starts a new one on a change', () => {
    const a = sp('3107998');
    const f = buildFlow([
      line(0, 'hi', { speaker: a }),
      line(2_000, 'there', { speaker: a, gapMs: 300 }),
      line(4_000, 'go ahead', { speaker: sp('3228520', '31068', 2) }),
    ]);
    expect(f.map((x) => x.type)).toEqual(['block', 'block']);
    if (f[0].type === 'block') expect(f[0].lines).toHaveLength(2);
  });

  it('splits the same radio across talkgroups', () => {
    const f = buildFlow([
      line(0, 'a', { speaker: sp('1', '93') }),
      line(1_000, 'b', { speaker: sp('1', '31068') }),
    ]);
    expect(f.filter((x) => x.type === 'block')).toHaveLength(2);
  });

  it('draws a neutral divider on an unknown speaker turn', () => {
    const f = buildFlow([line(0, 'a'), line(1_000, 'b', { speakerTurn: true })]);
    expect(f.map((x) => x.type)).toEqual(['block', 'turn', 'block']);
  });

  it('inserts a time separator after a long silence, from wall clock', () => {
    const f = buildFlow([line(0, 'before'), line(185_000, 'after', { dur: 2 })]);
    expect(f.map((x) => x.type)).toEqual(['block', 'gap', 'block']);
    if (f[1].type === 'gap') expect(f[1].label).toBe('— 3 min —');
    expect(gapLabel(45_000)).toBe('— 45 s —');
  });

  it('renders plain text with speaker labels and talkgroups', () => {
    const t = flowToText(
      buildFlow([
        line(0, 'cq cq', { speaker: { ...sp('3107998'), label: 'KR1JOE' } }),
        line(1_000, '(encrypted)', { kind: 'placeholder', speaker: sp('9') }),
      ]),
      () => '00:00',
    );
    expect(t).toContain('[KR1JOE TG31068] cq cq');
    expect(t).toContain('[9 TG31068] ((encrypted))');
  });
});

describe('latestLabels / relabel', () => {
  it('relabels lines spoken before the talker alias arrived', () => {
    const recs = [
      { data: { event: 'call', speaker: sp('4040379') } },
      { data: { event: 'call', speaker: sp('3107998') } },
      { data: { event: 'alias', speaker: { ...sp('4040379'), label: 'VU3ITI' } } },
      { data: { event: 'end' } },
      { data: null },
    ];
    const labels = latestLabels(recs);
    expect(labels.get('DMR:4040379')).toBe('VU3ITI');
    const early = sp('4040379');
    expect(relabel(early, labels)).toEqual({ ...early, label: 'VU3ITI' });
    const other = sp('3107998');
    expect(relabel(other, labels)).toBe(other);
    expect(relabel(undefined, labels)).toBeUndefined();
  });
});
