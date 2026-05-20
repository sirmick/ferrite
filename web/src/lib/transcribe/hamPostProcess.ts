// Ham post-processor — the deterministic pass that turns Whisper's
// best-effort phonetic transcription into structured ham text.
//
// Whisper transcribes *spoken* callsigns phonetically ("whiskey one
// alpha bravo") far more reliably than it would ever guess "W1AB"
// directly. Structured ham knowledge — not the audio model — does the
// spelling. This module:
//
//   1. collapses runs of NATO phonetic words into a callsign when the
//      run validates against an ITU callsign pattern,
//   2. folds number-words into digits for signal reports ("five nine"
//      → "59", "five by nine" → "5 by 9"),
//   3. normalises common prosigns / Q-codes ("seventy three" → "73").
//
// Conservative by design: a phonetic run is only collapsed when it
// actually looks like a callsign, so ordinary speech containing
// "...an alpha test..." is left alone. The recovered callsigns are
// also fed back into the Worker's rolling `initial_prompt`, so the
// model biases toward them for the rest of the QSO.

/** NATO phonetic alphabet + the common ham deviations heard on air. */
const PHONETIC: Record<string, string> = {
  alpha: 'A',
  alfa: 'A',
  bravo: 'B',
  charlie: 'C',
  delta: 'D',
  echo: 'E',
  foxtrot: 'F',
  golf: 'G',
  hotel: 'H',
  india: 'I',
  juliet: 'J',
  juliett: 'J',
  kilo: 'K',
  lima: 'L',
  mike: 'M',
  november: 'N',
  oscar: 'O',
  papa: 'P',
  quebec: 'Q',
  romeo: 'R',
  sierra: 'S',
  tango: 'T',
  uniform: 'U',
  victor: 'V',
  whiskey: 'W',
  whisky: 'W',
  xray: 'X',
  'x-ray': 'X',
  yankee: 'Y',
  zulu: 'Z',
};

/** Spoken digits (incl. ham "niner") → digit chars. */
const DIGIT_WORD: Record<string, string> = {
  zero: '0',
  oh: '0',
  one: '1',
  two: '2',
  three: '3',
  four: '4',
  five: '5',
  six: '6',
  seven: '7',
  eight: '8',
  nine: '9',
  niner: '9',
};

/** ITU-ish amateur callsign: 1–2 char prefix (letter, or letter+digit,
 *  or 2 letters), one digit, 1–4 letter suffix. Optional /P /M /MM …
 *  appendix. Deliberately loose — false negatives lose a collapse,
 *  false positives mangle text, so we err toward not collapsing. */
const CALLSIGN_RE = /^[A-Z]{1,2}[0-9][A-Z]{1,4}(?:\/[A-Z0-9]{1,3})?$/;

function isPhoneticOrDigit(w: string): boolean {
  return w in PHONETIC || w in DIGIT_WORD;
}

function letterFor(w: string): string {
  return PHONETIC[w] ?? DIGIT_WORD[w] ?? '';
}

/** Decide whether a maximal run of phonetic/digit words should collapse
 *  into a single compact token. Returns the compact string when it
 *  validates as a callsign (or is a long all-phonetic spell-out), or
 *  `null` to leave the run as separate words — the caller then re-emits
 *  the original tokens individually so a later number-fold pass can
 *  still see them ("five nine" → "59"). */
function collapseRun(words: string[]): string | null {
  const compact = words.map(letterFor).join('');
  if (compact.length >= 3 && CALLSIGN_RE.test(compact)) return compact;
  // Not a callsign — but a long all-phonetic spell-out (e.g. a name or
  // grid square being spelled) is still better shown compact-upper.
  if (words.length >= 4 && words.every((w) => w in PHONETIC)) return compact;
  return null;
}

/** "five nine", "five by nine", "five nine plus" → signal-report digits.
 *  Runs of bare digit-words of length ≥2 collapse to digits; the
 *  connector "by" is preserved (RST-with-tone style: "5 by 9"). */
function foldNumbers(tokens: string[]): string[] {
  const out: string[] = [];
  let i = 0;
  while (i < tokens.length) {
    const w = tokens[i].toLowerCase();
    if (w in DIGIT_WORD) {
      let digits = DIGIT_WORD[w];
      let j = i + 1;
      while (j < tokens.length && tokens[j].toLowerCase() in DIGIT_WORD) {
        digits += DIGIT_WORD[tokens[j].toLowerCase()];
        j += 1;
      }
      out.push(digits);
      i = j;
      continue;
    }
    out.push(tokens[i]);
    i += 1;
  }
  return out;
}

/** Multi-word prosign / Q-code phrases, applied on the lowercased
 *  string before tokenisation. Order matters (longest first). */
const PHRASES: ReadonlyArray<readonly [RegExp, string]> = [
  [/\bseventy[\s-]?three\b/g, '73'],
  [/\beighty[\s-]?eight\b/g, '88'],
  [/\bsee you later\b/g, 'CUL'],
  [/\bbest regards\b/g, '73'],
];

const WORD_FIXUPS: Record<string, string> = {
  cq: 'CQ',
  qsl: 'QSL',
  qrz: 'QRZ',
  qso: 'QSO',
  qth: 'QTH',
  qrm: 'QRM',
  qrn: 'QRN',
  qsy: 'QSY',
  rst: 'RST',
  roger: 'roger',
  over: 'over',
  break: 'break',
};

/**
 * Run the full ham post-processing pipeline on one segment of raw
 * Whisper text. Pure + deterministic. Returns the cleaned string.
 */
export function applyHamPostProcess(raw: string): string {
  if (!raw.trim()) return raw.trim();

  let s = ` ${raw.toLowerCase()} `;
  for (const [re, rep] of PHRASES) s = s.replace(re, ` ${rep} `);

  const tokens = s
    .split(/\s+/)
    .map((t) => t.trim())
    .filter(Boolean);

  // Pass 1: collapse phonetic runs (callsign recovery).
  const afterPhonetic: string[] = [];
  let i = 0;
  while (i < tokens.length) {
    const bare = tokens[i].replace(/[.,!?;:]+$/, '');
    if (isPhoneticOrDigit(bare.toLowerCase())) {
      const run: string[] = [];
      const orig: string[] = [];
      while (i < tokens.length) {
        const b = tokens[i].replace(/[.,!?;:]+$/, '').toLowerCase();
        if (!isPhoneticOrDigit(b)) break;
        run.push(b);
        orig.push(tokens[i]);
        i += 1;
      }
      const collapsed = collapseRun(run);
      if (collapsed !== null) afterPhonetic.push(collapsed);
      else afterPhonetic.push(...orig);
      continue;
    }
    afterPhonetic.push(tokens[i]);
    i += 1;
  }

  // Pass 2: fold any remaining bare number-words ("five nine" reports).
  const afterNumbers = foldNumbers(afterPhonetic);

  // Pass 3: per-word Q-code / prosign casing.
  const finalTokens = afterNumbers.map((t) => {
    const key = t.toLowerCase().replace(/[.,!?;:]+$/, '');
    if (key in WORD_FIXUPS) {
      const punct = t.slice(key.length);
      return WORD_FIXUPS[key] + punct;
    }
    return t;
  });

  return stripTailRepeats(
    finalTokens
      .join(' ')
      .replace(/\s+([.,!?;:])/g, '$1')
      .trim(),
  );
}

/**
 * Trim whisper's classic short-clip tail-loop artefacts: a single
 * character repeated 4+ times at the end ("okayyyyy", "hello....")
 * collapses to one; an identical word repeated 3+ times in a row at
 * the end ("the the the the") collapses to one. Whisper hallucinates
 * these when the VAD-gated clip has trailing silence — entropy_thold
 * + no_speech_thold in the glue catch most cases; this is the
 * belt-and-braces cleanup on whatever still slips through.
 */
function stripTailRepeats(s: string): string {
  if (!s) return s;
  let out = s;
  // Trailing word-repeat: ≥3 occurrences of the same case-insensitive
  // word at the end, optionally followed by terminal punctuation.
  // Keep the first; drop the rest.
  out = out.replace(/(\b\w+\b)(?:\s+\1\b){2,}(\s*[.,!?;:]*)\s*$/i, '$1$2');
  // Trailing single-character run: 4+ of the same character at the
  // end (any character — letters, dots, dashes). Keep one.
  out = out.replace(/(.)\1{3,}\s*$/u, '$1');
  return out.trimEnd();
}

/** Extract callsign-shaped tokens from a cleaned segment. The Worker
 *  feeds these back into the rolling `initial_prompt` so the model
 *  biases toward stations already heard in the QSO. */
export function extractCallsigns(cleaned: string): string[] {
  const seen = new Set<string>();
  for (const tok of cleaned.split(/\s+/)) {
    const base = tok.replace(/[.,!?;:]+$/, '');
    if (CALLSIGN_RE.test(base)) seen.add(base);
  }
  return [...seen];
}

/** Exposed for unit tests. */
export const __test = { collapseRun, foldNumbers, CALLSIGN_RE, stripTailRepeats };
