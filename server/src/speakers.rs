//! Speaker attribution for transcripts — who said what.
//!
//! Digital voice tells us who is talking: every `dsd` call record (from
//! `DmrDecode` or `DsdHelper`) names the timeslot, talkgroup and source
//! radio, and talker aliases arrive alongside. This book follows those
//! calls per slot and, when a `transcribe` record lands, stamps it with
//! the call that was on the air on its lane while the words were spoken.
//!
//! It runs inside [`DecoderStore::apply`](crate::decoder_store), so
//! every consumer — every browser tab, both panes, the AI's tool cards,
//! the MCP transcript text — sees the same `speaker` object:
//!
//! ```json
//! "speaker": {"key": "DMR:3107998", "label": "KR1JOE", "id": "3107998",
//!             "source": "radio", "group": "31068", "colour": 3}
//! ```
//!
//! **Colours are assigned here, not hashed in the browser.** A hash of
//! the radio ID into a 10-colour palette collides ~70 % of the time with
//! only five people talking; assigning least-recently-used slots keeps
//! everyone heard in the last [`ACTIVE_MS`] distinct, and one server
//! assignment keeps the colour identical across tabs and views.
//!
//! **Timing.** A transcript record is stored when the recogniser closes
//! an utterance — after its trailing-silence endpoint — so its audio sat
//! roughly `[at − latency − duration, at − latency]`. The record goes to
//! the call on its lane (`lane: "slot1"` / `"slot2"`) whose span overlaps
//! that window most; a record with no lane is matched across all slots.
//! Pure: the clock is passed in, so the policy is unit-tested.

use std::collections::{HashMap, VecDeque};

use serde_json::{json, Value};

/// Distinct speaker colours the UI's palette provides.
pub const PALETTE: u8 = 10;
/// A colour stays reserved for a speaker heard within this window.
pub const ACTIVE_MS: u64 = 10 * 60 * 1000;
/// Recogniser close latency: endpoint trailing silence plus processing.
pub const ASR_LATENCY_MS: u64 = 1_500;
/// Calls remembered per slot (for late transcripts after a handover).
const SPANS_PER_SLOT: usize = 8;

#[derive(Debug, Clone)]
struct Speaker {
    colour: u8,
    last_ms: u64,
}

#[derive(Debug, Clone)]
struct Span {
    key: String,
    id: String,
    group: Option<String>,
    start_ms: u64,
    end_ms: Option<u64>,
}

#[derive(Debug, Default)]
pub struct SpeakerBook {
    speakers: HashMap<String, Speaker>,
    aliases: HashMap<String, String>,
    slots: HashMap<u8, VecDeque<Span>>,
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl SpeakerBook {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The colour for `key`: its own if it has one, else the lowest
    /// palette slot no recently-heard speaker holds, else the colour of
    /// whoever was heard longest ago (who gets a new one next time).
    fn colour(&mut self, key: &str, now: u64) -> u8 {
        if let Some(s) = self.speakers.get_mut(key) {
            s.last_ms = now;
            return s.colour;
        }
        let busy: Vec<u8> = self
            .speakers
            .values()
            .filter(|s| now.saturating_sub(s.last_ms) <= ACTIVE_MS)
            .map(|s| s.colour)
            .collect();
        let colour = (0..PALETTE).find(|c| !busy.contains(c)).unwrap_or_else(|| {
            let (old_key, c) = self
                .speakers
                .iter()
                .min_by_key(|(_, s)| s.last_ms)
                .map(|(k, s)| (k.clone(), s.colour))
                .unwrap_or_default();
            self.speakers.remove(&old_key);
            c
        });
        self.speakers.insert(
            key.to_string(),
            Speaker {
                colour,
                last_ms: now,
            },
        );
        colour
    }

    fn speaker_json(&mut self, key: &str, id: &str, group: Option<&str>, now: u64) -> Value {
        let colour = self.colour(key, now);
        let label = self
            .aliases
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string());
        let mut v = json!({
            "key": key, "label": label, "id": id, "source": "radio", "colour": colour,
        });
        if let Some(g) = group {
            v["group"] = Value::from(g);
        }
        v
    }

    /// Enrich one record about to be stored under `kind` (in place).
    /// Only `dsd` and `transcribe` are touched.
    pub fn enrich(&mut self, kind: &str, data: &mut Value, now: u64) {
        match kind {
            "dsd" => self.on_dsd(data, now),
            "transcribe" => self.on_transcript(data, now),
            _ => {}
        }
    }

    fn on_dsd(&mut self, data: &mut Value, now: u64) {
        let event = data.get("event").and_then(Value::as_str).unwrap_or("");
        let proto = str_field(data, "protocol").unwrap_or_else(|| "DV".into());
        #[allow(clippy::cast_possible_truncation)]
        let slot = data.get("slot").and_then(Value::as_u64).unwrap_or(0) as u8;
        let src = str_field(data, "src");
        let tgt = str_field(data, "tgt");
        match (event, src) {
            ("call", Some(src)) => {
                let key = format!("{proto}:{src}");
                let spans = self.slots.entry(slot).or_default();
                // A new call on the slot closes whatever was open.
                if let Some(open) = spans.back_mut().filter(|s| s.end_ms.is_none()) {
                    open.end_ms = Some(now);
                }
                spans.push_back(Span {
                    key: key.clone(),
                    id: src.clone(),
                    group: tgt.clone(),
                    start_ms: now,
                    end_ms: None,
                });
                while spans.len() > SPANS_PER_SLOT {
                    spans.pop_front();
                }
                data["speaker"] = self.speaker_json(&key, &src, tgt.as_deref(), now);
            }
            ("end", Some(src)) => {
                let key = format!("{proto}:{src}");
                if let Some(open) = self.slots.get_mut(&slot).and_then(|s| {
                    s.iter_mut()
                        .rev()
                        .find(|s| s.key == key && s.end_ms.is_none())
                }) {
                    open.end_ms = Some(now);
                }
                data["speaker"] = self.speaker_json(&key, &src, tgt.as_deref(), now);
            }
            ("alias", Some(src)) => {
                if let Some(alias) = str_field(data, "text") {
                    self.aliases.insert(src.clone(), alias);
                }
                let key = format!("{proto}:{src}");
                data["speaker"] = self.speaker_json(&key, &src, None, now);
            }
            _ => {}
        }
    }

    fn on_transcript(&mut self, data: &mut Value, now: u64) {
        let lane_slot = data
            .get("lane")
            .and_then(Value::as_str)
            .and_then(|l| l.strip_prefix("slot"))
            .and_then(|n| n.parse::<u8>().ok());
        let t0 = data.get("t0").and_then(Value::as_f64).unwrap_or(0.0);
        let t1 = data.get("t1").and_then(Value::as_f64).unwrap_or(0.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let dur = ((t1 - t0).max(0.0) * 1000.0) as u64;
        let end = now.saturating_sub(ASR_LATENCY_MS);
        let start = end.saturating_sub(dur.max(500));
        // 1. The call that overlaps the utterance's estimated audio most.
        // 2. Otherwise the latest call on the lane that started before the
        //    line closed and ended no longer ago than the utterance could
        //    span: a lane only produces speech while its slot has a
        //    verified call (DmrDecode's voice gate), so a line with no
        //    overlap — the estimate is approximate, e.g. late entry just
        //    after the call record — still belongs to that slot's call.
        let recent = dur + ASR_LATENCY_MS + 2_000;
        let mut best: Option<(u64, Span)> = None;
        let mut latest: Option<Span> = None;
        for (slot, spans) in &self.slots {
            if lane_slot.is_some_and(|l| l != *slot) {
                continue;
            }
            for s in spans {
                let s_end = s.end_ms.unwrap_or(now);
                let overlap = s_end.min(end).saturating_sub(s.start_ms.max(start));
                if overlap > 0 && best.as_ref().is_none_or(|(o, _)| overlap > *o) {
                    best = Some((overlap, s.clone()));
                }
                let live = s.start_ms <= now && now.saturating_sub(s_end) <= recent;
                if live && latest.as_ref().is_none_or(|l| s.start_ms > l.start_ms) {
                    latest = Some(s.clone());
                }
            }
        }
        let best = best.or(latest.map(|s| (0, s)));
        if let Some((_, s)) = best {
            data["speaker"] = self.speaker_json(&s.key, &s.id, s.group.as_deref(), now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SpeakerBook, ACTIVE_MS, ASR_LATENCY_MS, PALETTE};
    use serde_json::{json, Value};

    fn call(slot: u64, tgt: &str, src: &str) -> Value {
        json!({"t":"dsd","event":"call","protocol":"DMR","slot":slot,"tgt":tgt,"src":src})
    }

    #[test]
    fn transcript_is_attributed_to_the_call_on_its_lane() {
        let mut b = SpeakerBook::new();
        let mut c1 = call(2, "31068", "3107998");
        b.enrich("dsd", &mut c1, 10_000);
        let mut c2 = call(1, "93", "3228520");
        b.enrich("dsd", &mut c2, 11_000);
        // A 4 s utterance on slot 2, closed at 20 s.
        let mut t = json!({"text":"hello","t0":0.0,"t1":4.0,"lane":"slot2"});
        b.enrich("transcribe", &mut t, 20_000);
        assert_eq!(t["speaker"]["id"], "3107998");
        assert_eq!(t["speaker"]["group"], "31068");
        assert_eq!(t["speaker"]["colour"], c1["speaker"]["colour"]);
        let mut t1 = json!({"text":"hi","t0":0.0,"t1":2.0,"lane":"slot1"});
        b.enrich("transcribe", &mut t1, 20_000);
        assert_eq!(t1["speaker"]["id"], "3228520");
        assert_ne!(t1["speaker"]["colour"], t["speaker"]["colour"]);
    }

    #[test]
    fn handover_on_one_slot_goes_to_the_right_talker() {
        let mut b = SpeakerBook::new();
        b.enrich("dsd", &mut call(2, "31068", "111"), 0);
        b.enrich(
            "dsd",
            &mut json!({"event":"end","protocol":"DMR","slot":2,"src":"111"}),
            9_000,
        );
        b.enrich("dsd", &mut call(2, "31068", "222"), 10_000);
        // Utterance spoken 4–8 s, transcript closed late (after 222 keyed up).
        let mut late = json!({"text":"over","t0":0.0,"t1":4.0,"lane":"slot2"});
        b.enrich("transcribe", &mut late, 8_000 + ASR_LATENCY_MS + 2_000);
        assert_eq!(late["speaker"]["id"], "111", "{late}");
        let mut next = json!({"text":"go ahead","t0":0.0,"t1":3.0,"lane":"slot2"});
        b.enrich("transcribe", &mut next, 15_000 + ASR_LATENCY_MS);
        assert_eq!(next["speaker"]["id"], "222", "{next}");
    }

    #[test]
    fn a_line_right_after_the_call_record_still_finds_it() {
        // Late entry: the call record and the first transcript close
        // together, so the audio-window estimate predates the call.
        let mut b = SpeakerBook::new();
        b.enrich("dsd", &mut call(2, "31068", "3107998"), 50_000);
        let mut t = json!({"text":"go ahead","t0":0.0,"t1":1.0,"lane":"slot2"});
        b.enrich("transcribe", &mut t, 50_000);
        assert_eq!(t["speaker"]["id"], "3107998", "{t}");
        // …but a line long after that call ended is left unattributed.
        let mut b2 = SpeakerBook::new();
        b2.enrich("dsd", &mut call(2, "31068", "1"), 0);
        b2.enrich(
            "dsd",
            &mut json!({"event":"end","protocol":"DMR","slot":2,"src":"1"}),
            1_000,
        );
        let mut stale = json!({"text":"?","t0":0.0,"t1":1.0,"lane":"slot2"});
        b2.enrich("transcribe", &mut stale, 60_000);
        assert!(stale.get("speaker").is_none(), "{stale}");
    }

    #[test]
    fn alias_becomes_the_label_everywhere() {
        let mut b = SpeakerBook::new();
        b.enrich("dsd", &mut call(2, "31068", "3107998"), 0);
        let mut a = json!({"event":"alias","protocol":"DMR","slot":2,"src":"3107998","text":"KR1JOE Joseph"});
        b.enrich("dsd", &mut a, 1_000);
        assert_eq!(a["speaker"]["label"], "KR1JOE Joseph");
        let mut t = json!({"text":"x","t0":0.0,"t1":1.0,"lane":"slot2"});
        b.enrich("transcribe", &mut t, 4_000);
        assert_eq!(t["speaker"]["label"], "KR1JOE Joseph");
    }

    #[test]
    fn colours_stay_distinct_for_recent_speakers_and_recycle_the_oldest() {
        let mut b = SpeakerBook::new();
        let mut seen = Vec::new();
        for i in 0..u64::from(PALETTE) {
            let mut c = call(1, "9", &format!("{i}"));
            b.enrich("dsd", &mut c, i * 1_000);
            seen.push(c["speaker"]["colour"].as_u64().unwrap());
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), usize::from(PALETTE), "10 speakers, 10 colours");
        // An 11th recycles speaker 0's colour (heard longest ago).
        let mut c = call(1, "9", "new");
        b.enrich("dsd", &mut c, 20_000);
        assert_eq!(c["speaker"]["colour"], 0);
        // After the active window, colours free up again.
        let mut late = call(1, "9", "later");
        b.enrich("dsd", &mut late, 20_000 + ACTIVE_MS + 1);
        assert!(late["speaker"]["colour"].as_u64().unwrap() < u64::from(PALETTE));
    }

    #[test]
    fn records_without_calls_are_left_alone() {
        let mut b = SpeakerBook::new();
        let mut t = json!({"text":"analog voice","t0":0.0,"t1":2.0});
        b.enrich("transcribe", &mut t, 5_000);
        assert!(t.get("speaker").is_none());
        let mut other = json!({"msg":"CQ"});
        b.enrich("ft8", &mut other, 5_000);
        assert!(other.get("speaker").is_none());
    }
}
