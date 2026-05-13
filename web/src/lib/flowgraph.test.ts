// Phase-4 enforcement: every shipped flowgraph carries non-empty
// `ai_notes` prose that the ferrite-ai sidecar injects into the
// system prompt when the preset is active. See
// `docs/13-ai-notes-migration.md`.

import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

// `web/vitest.config.ts` runs from the web/ workspace, so the
// flowgraphs/ directory sits one level up.
const FLOWGRAPHS_DIR = join(__dirname, '..', '..', '..', 'flowgraphs');

const presetFiles = readdirSync(FLOWGRAPHS_DIR)
  .filter((n) => n.endsWith('.json'))
  .sort();

describe('flowgraphs ai_notes coverage', () => {
  it('discovers at least one preset', () => {
    expect(presetFiles.length).toBeGreaterThan(0);
  });

  for (const file of presetFiles) {
    it(`${file} has a non-empty ai_notes prose`, () => {
      const doc = JSON.parse(readFileSync(join(FLOWGRAPHS_DIR, file), 'utf-8')) as {
        name?: string;
        ai_notes?: string;
      };
      expect(typeof doc.ai_notes, `${file}: missing ai_notes field`).toBe('string');
      const notes = (doc.ai_notes ?? '').trim();
      expect(notes.length, `${file}: ai_notes is empty`).toBeGreaterThan(0);
      // 3+ sentences ≈ 20+ words. The migration doc proposes 3–6
      // sentences; this is the floor that rejects placeholder stubs.
      const words = notes.split(/\s+/).length;
      expect(
        words,
        `${file}: ai_notes is too terse (${words} words; need ≥20)`,
      ).toBeGreaterThanOrEqual(20);
    });
  }
});
