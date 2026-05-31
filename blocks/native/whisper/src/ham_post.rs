//! Ham post-processor — Rust port of
//! `web/src/lib/transcribe/hamPostProcess.ts`.
//!
//! Turns whisper's best-effort *phonetic* transcription into structured
//! ham text deterministically (no model involved):
//!
//!   1. collapse NATO phonetic runs into a callsign when the run validates
//!      against an ITU callsign pattern,
//!   2. fold number-words into digits for signal reports ("five nine" → "59"),
//!   3. normalise common prosigns / Q-codes ("seventy three" → "73"),
//!   4. strip whisper's short-clip tail-loop artefacts.
//!
//! Conservative: a phonetic run only collapses when it actually looks
//! like a callsign, so ordinary speech ("an alpha test") is left alone.
//! Recovered callsigns are returned separately so the orchestrator can
//! feed them back into the rolling `initial_prompt`.
//!
//! Behaviour-preserving port: the same input must yield the same output
//! as the TS so node and browser transcripts match. Pure + no deps —
//! wasm-clean, which is what lets Stage B run this exact code in the
//! browser and delete `hamPostProcess.ts`.

/// NATO phonetic alphabet + common ham deviations. Lowercase keys.
fn phonetic(w: &str) -> Option<char> {
    Some(match w {
        "alpha" | "alfa" => 'A',
        "bravo" => 'B',
        "charlie" => 'C',
        "delta" => 'D',
        "echo" => 'E',
        "foxtrot" => 'F',
        "golf" => 'G',
        "hotel" => 'H',
        "india" => 'I',
        "juliet" | "juliett" => 'J',
        "kilo" => 'K',
        "lima" => 'L',
        "mike" => 'M',
        "november" => 'N',
        "oscar" => 'O',
        "papa" => 'P',
        "quebec" => 'Q',
        "romeo" => 'R',
        "sierra" => 'S',
        "tango" => 'T',
        "uniform" => 'U',
        "victor" => 'V',
        "whiskey" | "whisky" => 'W',
        "xray" | "x-ray" => 'X',
        "yankee" => 'Y',
        "zulu" => 'Z',
        _ => return None,
    })
}

/// Spoken digits incl. ham "niner".
fn digit_word(w: &str) -> Option<char> {
    Some(match w {
        "zero" | "oh" => '0',
        "one" => '1',
        "two" => '2',
        "three" => '3',
        "four" => '4',
        "five" => '5',
        "six" => '6',
        "seven" => '7',
        "eight" => '8',
        "nine" | "niner" => '9',
        _ => return None,
    })
}

fn is_phonetic_or_digit(w: &str) -> bool {
    phonetic(w).is_some() || digit_word(w).is_some()
}

fn letter_for(w: &str) -> Option<char> {
    phonetic(w).or_else(|| digit_word(w))
}

/// ITU-ish amateur callsign: 1–2 char prefix (letter, letter+digit, or
/// 2 letters), one digit, 1–4 letter suffix, optional `/P` `/MM` …
/// appendix. Deliberately loose. Hand-rolled (no regex dep): mirrors
/// `^[A-Z]{1,2}[0-9][A-Z]{1,4}(?:/[A-Z0-9]{1,3})?$`.
fn is_callsign(s: &str) -> bool {
    let (core, suffix) = match s.split_once('/') {
        Some((c, x)) => (c, Some(x)),
        None => (s, None),
    };
    if let Some(x) = suffix {
        if x.is_empty()
            || x.len() > 3
            || !x
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return false;
        }
    }
    let b = core.as_bytes();
    let mut i = 0;
    // prefix: 1–2 of [A-Z], with at most… actually the TS allows
    // [A-Z]{1,2} then [0-9] then [A-Z]{1,4}. (A leading digit prefix
    // like 2E0 isn't matched by the TS regex either, so we don't add it.)
    let mut pfx = 0;
    while i < b.len() && b[i].is_ascii_uppercase() && pfx < 2 {
        i += 1;
        pfx += 1;
    }
    if pfx == 0 {
        return false;
    }
    // exactly one digit
    if i >= b.len() || !b[i].is_ascii_digit() {
        return false;
    }
    i += 1;
    // 1–4 letter suffix, consuming the rest
    let mut sfx = 0;
    while i < b.len() && b[i].is_ascii_uppercase() {
        i += 1;
        sfx += 1;
    }
    i == b.len() && (1..=4).contains(&sfx)
}

/// Collapse a maximal run of phonetic/digit words into a compact token,
/// or `None` to leave them as separate words (caller re-emits originals).
fn collapse_run(words: &[String]) -> Option<String> {
    let compact: String = words.iter().filter_map(|w| letter_for(w)).collect();
    if compact.len() >= 3 && is_callsign(&compact) {
        return Some(compact);
    }
    // Long all-phonetic spell-out (name / grid) → still show compact.
    if words.len() >= 4 && words.iter().all(|w| phonetic(w).is_some()) {
        return Some(compact);
    }
    None
}

fn strip_trailing_punct(w: &str) -> &str {
    w.trim_end_matches([',', '.', '!', '?', ';', ':'])
}

/// Fold runs of bare digit-words (≥1) into digit strings. "five nine" →
/// "59"; the connector "by" is preserved by virtue of not being a digit.
fn fold_numbers(tokens: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let lw = tokens[i].to_lowercase();
        if digit_word(&lw).is_some() {
            let mut digits = String::new();
            digits.push(digit_word(&lw).unwrap());
            let mut j = i + 1;
            while j < tokens.len() {
                let lj = tokens[j].to_lowercase();
                match digit_word(&lj) {
                    Some(d) => {
                        digits.push(d);
                        j += 1;
                    }
                    None => break,
                }
            }
            out.push(digits);
            i = j;
        } else {
            out.push(tokens[i].clone());
            i += 1;
        }
    }
    out
}

/// Per-word Q-code / prosign casing.
fn word_fixup(key: &str) -> Option<&'static str> {
    Some(match key {
        "cq" => "CQ",
        "qsl" => "QSL",
        "qrz" => "QRZ",
        "qso" => "QSO",
        "qth" => "QTH",
        "qrm" => "QRM",
        "qrn" => "QRN",
        "qsy" => "QSY",
        "rst" => "RST",
        "roger" => "roger",
        "over" => "over",
        "break" => "break",
        _ => return None,
    })
}

/// Multi-word prosign / Q-code phrases applied on the lowercased string
/// before tokenisation. (Hand-applied; no regex dep.)
fn apply_phrases(s: &str) -> String {
    // longest-first; whitespace/hyphen tolerant between the two words.
    let mut out = s.to_string();
    for (a, b, rep) in [("seventy", "three", "73"), ("eighty", "eight", "88")] {
        out = replace_two_word(&out, a, b, rep);
    }
    out = replace_phrase(&out, "see you later", "CUL");
    out = replace_phrase(&out, "best regards", "73");
    out
}

/// Replace "`a`[\s-]`b`" (word-boundaried) with `rep`, case-insensitive.
fn replace_two_word(s: &str, a: &str, b: &str, rep: &str) -> String {
    let lower = s.to_lowercase();
    let mut result = String::with_capacity(s.len());
    let bytes = lower.as_bytes();
    let mut i = 0;
    'outer: while i < s.len() {
        // try match at i
        for sep in ["-", " "] {
            let pat = format!("{a}{sep}{b}");
            if lower[i..].starts_with(&pat)
                && is_boundary(bytes, i)
                && is_boundary(bytes, i + pat.len())
            {
                result.push_str(rep);
                i += pat.len();
                continue 'outer;
            }
        }
        // copy original byte (UTF-8 safe: ASCII inputs in practice; fall
        // back to char copy at this position).
        let ch = s[i..].chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }
    result
}

fn replace_phrase(s: &str, phrase: &str, rep: &str) -> String {
    let lower = s.to_lowercase();
    let bytes = lower.as_bytes();
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if lower[i..].starts_with(phrase)
            && is_boundary(bytes, i)
            && is_boundary(bytes, i + phrase.len())
        {
            result.push_str(rep);
            i += phrase.len();
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }
    result
}

fn is_boundary(bytes: &[u8], pos: usize) -> bool {
    if pos == 0 || pos >= bytes.len() {
        return true;
    }
    !bytes[pos].is_ascii_alphanumeric() || !bytes[pos - 1].is_ascii_alphanumeric()
}

/// Run the full ham post-processing pipeline on one raw whisper segment.
/// Pure + deterministic; returns the cleaned string.
#[must_use]
pub fn apply(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let padded = format!(" {} ", raw.to_lowercase());
    let s = apply_phrases(&padded);

    let tokens: Vec<String> = s.split_whitespace().map(str::to_string).collect();

    // Pass 1: collapse phonetic runs (callsign recovery).
    let mut after_phonetic: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let bare = strip_trailing_punct(&tokens[i]).to_lowercase();
        if is_phonetic_or_digit(&bare) {
            let mut run = Vec::new();
            let mut orig = Vec::new();
            while i < tokens.len() {
                let b = strip_trailing_punct(&tokens[i]).to_lowercase();
                if !is_phonetic_or_digit(&b) {
                    break;
                }
                run.push(b);
                orig.push(tokens[i].clone());
                i += 1;
            }
            match collapse_run(&run) {
                Some(c) => after_phonetic.push(c),
                None => after_phonetic.extend(orig),
            }
        } else {
            after_phonetic.push(tokens[i].clone());
            i += 1;
        }
    }

    // Pass 2: fold remaining bare number-words.
    let after_numbers = fold_numbers(after_phonetic);

    // Pass 3: per-word Q-code / prosign casing (preserve trailing punct).
    let final_tokens: Vec<String> = after_numbers
        .iter()
        .map(|t| {
            let key = strip_trailing_punct(t).to_lowercase();
            if let Some(fixed) = word_fixup(&key) {
                let punct = &t[strip_trailing_punct(t).len()..];
                format!("{fixed}{punct}")
            } else {
                t.clone()
            }
        })
        .collect();

    let joined = final_tokens.join(" ");
    let tidied = tidy_punct_spacing(&joined);
    strip_tail_repeats(tidied.trim())
}

/// Drop the space before terminal punctuation (`word ,` → `word,`).
fn tidy_punct_spacing(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' {
            if let Some(&n) = chars.peek() {
                if matches!(n, '.' | ',' | '!' | '?' | ';' | ':') {
                    continue; // skip the space
                }
            }
        }
        out.push(c);
    }
    out
}

/// Trim whisper's classic short-clip tail-loop artefacts:
///   * the same word repeated ≥3× at the end → keep one;
///   * the same character run ≥4× at the end → keep one.
fn strip_tail_repeats(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut out = s.to_string();

    // Trailing word-repeat: ≥3 occurrences of the same (case-insensitive)
    // word at the end, optional terminal punctuation. Keep the first.
    let words: Vec<&str> = out.split(' ').collect();
    if words.len() >= 3 {
        let last = strip_trailing_punct(words[words.len() - 1]).to_lowercase();
        if !last.is_empty() {
            let mut rep = 1;
            for w in words.iter().rev().skip(1) {
                if strip_trailing_punct(w).to_lowercase() == last {
                    rep += 1;
                } else {
                    break;
                }
            }
            if rep >= 3 {
                // Keep everything up to and including the first of the run.
                let keep = words.len() - rep;
                let mut kept: Vec<&str> = words[..keep].to_vec();
                kept.push(words[keep]); // the first occurrence (+ its punct if any)
                out = kept.join(" ");
            }
        }
    }

    // Trailing single-character run: 4+ of the same char at the very end.
    let chars: Vec<char> = out.chars().collect();
    if chars.len() >= 4 {
        let last = chars[chars.len() - 1];
        let mut run = 1;
        for &c in chars.iter().rev().skip(1) {
            if c == last {
                run += 1;
            } else {
                break;
            }
        }
        if run >= 4 {
            let keep = chars.len() - run + 1;
            out = chars[..keep].iter().collect();
        }
    }

    out.trim_end().to_string()
}

/// Extract callsign-shaped tokens from a cleaned segment, for the rolling
/// `initial_prompt` bias.
#[must_use]
pub fn extract_callsigns(cleaned: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for tok in cleaned.split_whitespace() {
        let base = strip_trailing_punct(tok);
        if is_callsign(base) && !seen.iter().any(|s: &String| s == base) {
            seen.push(base.to_string());
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_phonetic_callsign() {
        // "whiskey one alpha bravo" → W1AB
        assert_eq!(apply("whiskey one alpha bravo"), "W1AB");
    }

    #[test]
    fn leaves_ordinary_speech_with_a_phonetic_word_alone() {
        // A lone "alpha" amid prose must NOT collapse.
        let out = apply("running an alpha test today");
        assert!(out.contains("alpha"), "got: {out}");
        assert!(!out.contains('A') || out.contains("alpha"));
    }

    #[test]
    fn folds_signal_report() {
        assert_eq!(apply("you are five nine"), "you are 59");
    }

    #[test]
    fn normalises_seventy_three() {
        assert_eq!(apply("seventy three"), "73");
        assert_eq!(apply("seventy-three"), "73");
    }

    #[test]
    fn q_code_casing() {
        assert_eq!(apply("calling cq cq"), "calling CQ CQ");
    }

    #[test]
    fn strips_tail_word_loop() {
        assert_eq!(apply("hello the the the the"), "hello the");
    }

    #[test]
    fn strips_tail_char_loop() {
        // 4+ trailing same char collapses to one.
        assert_eq!(apply("okayyyyy"), "okay");
    }

    #[test]
    fn extracts_callsigns() {
        let calls = extract_callsigns("thanks W1AB and K2XY/P over");
        assert!(calls.contains(&"W1AB".to_string()));
        assert!(calls.contains(&"K2XY/P".to_string()));
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn callsign_validator_matches_ts_regex() {
        assert!(is_callsign("W1AB"));
        assert!(is_callsign("K2XY"));
        assert!(is_callsign("AB1CDE")); // 2-letter pfx, 4-letter sfx
        assert!(is_callsign("W1AB/P"));
        assert!(!is_callsign("HELLO")); // no digit
        assert!(!is_callsign("W1")); // no suffix
        assert!(!is_callsign("W1ABCDE")); // 5-letter suffix
        assert!(!is_callsign("1ABC")); // leading digit (TS regex rejects)
    }

    #[test]
    fn empty_and_whitespace() {
        assert_eq!(apply(""), "");
        assert_eq!(apply("   "), "");
    }
}
