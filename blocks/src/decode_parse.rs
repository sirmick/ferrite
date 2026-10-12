//! Structured fields for decoders whose engines print prose.
//!
//! multimon-ng (POCSAG, FLEX, EAS/SAME) and aisdecoder (AIVDM) emit text
//! lines. An agent wants one record per decode with the entity id, the
//! message, and position where there is one — so the emitting blocks run
//! these pure parsers and add the fields next to the raw `text` (kept
//! verbatim, so nothing that read it before breaks). A line a parser
//! doesn't recognise yields `None` and the record stays raw-only.

use serde_json::{json, Map, Value};

// ─── POCSAG / FLEX (multimon-ng) ────────────────────────────────────────

/// `POCSAG1200: Address: 1234567  Function: 0  Alpha:   text` (also
/// `Numeric:` / `Skyper:`; the address is `-` on a partial decode).
#[must_use]
pub fn parse_pocsag(line: &str) -> Option<Value> {
    let (head, rest) = line.split_once(':')?;
    let baud: u32 = head.strip_prefix("POCSAG")?.trim().parse().ok()?;
    let rest = rest.trim_start().strip_prefix("Address:")?.trim_start();
    let (addr, rest) = rest.split_once("Function:")?;
    let address = addr.trim().parse::<u64>().ok();
    let rest = rest.trim_start();
    let (func, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let function = func.trim().parse::<u8>().ok();
    let rest = rest.trim_start();
    let (format, message) = ["Alpha:", "Numeric:", "Skyper:"]
        .iter()
        .find_map(|tag| {
            rest.strip_prefix(tag).map(|m| {
                (
                    tag.trim_end_matches(':').to_ascii_lowercase(),
                    m.trim().to_string(),
                )
            })
        })
        .unwrap_or_else(|| ("tone".into(), String::new()));
    Some(json!({
        "protocol": "POCSAG",
        "baud": baud,
        "address": address,
        "function": function,
        "format": format,
        "message": message,
    }))
}

/// `FLEX_NEXT|1600/2|07.123.A|0001234567|SS|5|ALN|3.0.K|text` — the
/// decoder `PagerDemod` runs — and the classic
/// `FLEX|2026-10-10 12:00:00|1600/2/K/A|07.123|001234567|ALN|text` the
/// offline analyzer still prints. Pipe-delimited; the capcode is the
/// first long all-digit field, the page type one of
/// `ALN`/`NUM`/`TON`/`BIN`/`UNK`, the message everything after it.
///
/// FLEX_NEXT puts two things in front of an ALN message that are not
/// message text: a fragment tag `<frag>.<cont>.<K|C|F>` (K = complete,
/// C = the first part of a longer page, F = a continuation) and, when
/// the `LS`/`SG` flag field says `G` (a group page), the member
/// capcodes. Those come out as `frag` and `group`; `message` is the
/// text alone.
#[must_use]
pub fn parse_flex(line: &str) -> Option<Value> {
    let fields: Vec<&str> = line.split('|').collect();
    let protocol = *fields.first()?;
    if protocol != "FLEX" && protocol != "FLEX_NEXT" {
        return None;
    }
    let all_digits = |f: &str| f.len() >= 7 && f.bytes().all(|b| b.is_ascii_digit());
    let capcode = fields
        .iter()
        .skip(1)
        .find(|f| all_digits(f))
        .and_then(|f| f.parse::<u64>().ok());
    let type_at = fields
        .iter()
        .position(|f| matches!(*f, "ALN" | "NUM" | "TON" | "BIN" | "UNK"))?;
    let format = match fields[type_at] {
        "ALN" => "alpha",
        "NUM" => "numeric",
        "TON" => "tone",
        "BIN" => "binary",
        _ => "unknown",
    };
    let baud = fields
        .get(1..)
        .and_then(|fs| fs.iter().find(|f| f.contains('/')))
        .and_then(|f| f.split('/').next())
        .and_then(|b| b.parse::<u32>().ok());
    // FLEX_NEXT's address flags: `L`/`S` (long/short address) then
    // `G`/`S` (group/single page).
    let group_page = fields[..type_at]
        .iter()
        .any(|f| f.len() == 2 && matches!(f.as_bytes()[0], b'L' | b'S') && f.ends_with('G'));

    let mut rest = &fields[type_at + 1..];
    let mut frag = None;
    if format == "alpha" {
        if let Some(tag) = rest.first().filter(|f| is_frag_tag(f)) {
            frag = tag.chars().last().filter(|c| *c != '?').map(String::from);
            rest = &rest[1..];
        }
    }
    let mut group: Vec<u64> = Vec::new();
    if format == "alpha" && group_page {
        while let Some(member) = rest.first().filter(|f| all_digits(f)) {
            if let Ok(c) = member.parse::<u64>() {
                group.push(c);
            }
            rest = &rest[1..];
        }
    }
    let message = rest.join("|").trim().to_string();
    let mut out = json!({
        "protocol": protocol,
        "baud": baud,
        "capcode": capcode,
        "format": format,
        "message": message,
    });
    if let Some(obj) = out.as_object_mut() {
        if let Some(f) = frag {
            obj.insert("frag".into(), Value::from(f));
        }
        if !group.is_empty() {
            obj.insert("group".into(), Value::from(group));
        }
    }
    Some(out)
}

/// `3.0.K` — FLEX_NEXT's ALN fragment tag (`demod_flex_next.c`,
/// `parse_alphanumeric`): fragment number, continuation bit, flag.
fn is_frag_tag(f: &str) -> bool {
    let b = f.as_bytes();
    b.len() == 5
        && b[0].is_ascii_digit()
        && b[1] == b'.'
        && b[2].is_ascii_digit()
        && b[3] == b'.'
        && matches!(b[4], b'K' | b'C' | b'F' | b'?')
}

// ─── EAS / SAME ─────────────────────────────────────────────────────────

fn same_originator(code: &str) -> &'static str {
    match code {
        "WXR" => "National Weather Service",
        "EAS" => "EAS participant (broadcast station)",
        "PEP" => "Primary Entry Point (national)",
        "CIV" => "Civil authorities",
        _ => "unknown",
    }
}

/// The common SAME event codes (47 CFR 11.31 + NWS).
fn same_event(code: &str) -> &'static str {
    match code {
        "EAN" => "Emergency Action Notification",
        "NPT" => "National Periodic Test",
        "RMT" => "Required Monthly Test",
        "RWT" => "Required Weekly Test",
        "ADR" => "Administrative Message",
        "AVW" => "Avalanche Warning",
        "BZW" => "Blizzard Warning",
        "CAE" => "Child Abduction Emergency",
        "CDW" => "Civil Danger Warning",
        "CEM" => "Civil Emergency Message",
        "CFW" => "Coastal Flood Warning",
        "DSW" => "Dust Storm Warning",
        "EQW" => "Earthquake Warning",
        "EVI" => "Evacuation Immediate",
        "EWW" => "Extreme Wind Warning",
        "FFA" => "Flash Flood Watch",
        "FFW" => "Flash Flood Warning",
        "FFS" => "Flash Flood Statement",
        "FLA" => "Flood Watch",
        "FLW" => "Flood Warning",
        "FLS" => "Flood Statement",
        "FRW" => "Fire Warning",
        "HMW" => "Hazardous Materials Warning",
        "HUA" => "Hurricane Watch",
        "HUW" => "Hurricane Warning",
        "HWA" => "High Wind Watch",
        "HWW" => "High Wind Warning",
        "LAE" => "Local Area Emergency",
        "LEW" => "Law Enforcement Warning",
        "NUW" => "Nuclear Power Plant Warning",
        "RHW" => "Radiological Hazard Warning",
        "SMW" => "Special Marine Warning",
        "SPS" => "Special Weather Statement",
        "SPW" => "Shelter in Place Warning",
        "SQW" => "Snow Squall Warning",
        "SSA" => "Storm Surge Watch",
        "SSW" => "Storm Surge Warning",
        "SVA" => "Severe Thunderstorm Watch",
        "SVR" => "Severe Thunderstorm Warning",
        "SVS" => "Severe Weather Statement",
        "TOA" => "Tornado Watch",
        "TOR" => "Tornado Warning",
        "TRA" => "Tropical Storm Watch",
        "TRW" => "Tropical Storm Warning",
        "TSA" => "Tsunami Watch",
        "TSW" => "Tsunami Warning",
        "VOW" => "Volcano Warning",
        "WSA" => "Winter Storm Watch",
        "WSW" => "Winter Storm Warning",
        _ => "unknown",
    }
}

/// `EAS: ZCZC-WXR-RWT-012057-012081+0030-1231500-KTBW/NWS-` (header) or
/// `EAS: NNNN` (end of message).
#[must_use]
pub fn parse_same(line: &str) -> Option<Value> {
    let body = line.split_once(':').map_or(line, |(_, b)| b).trim();
    if body == "NNNN" {
        return Some(json!({ "type": "end_of_message" }));
    }
    let body = body.strip_prefix("ZCZC-")?;
    let (head, tail) = body.split_once('+')?;
    let mut parts = head.split('-');
    let org = parts.next()?;
    let event = parts.next()?;
    let locations: Vec<&str> = parts.filter(|p| !p.is_empty()).collect();
    let mut tail = tail.split('-');
    let purge = tail.next()?;
    let issued = tail.next().unwrap_or_default();
    let sender = tail.next().unwrap_or_default();
    let purge_minutes = (purge.len() == 4)
        .then(|| {
            let h: u32 = purge[..2].parse().ok()?;
            let m: u32 = purge[2..].parse().ok()?;
            Some(h * 60 + m)
        })
        .flatten();
    Some(json!({
        "type": "header",
        "originator": org,
        "originator_name": same_originator(org),
        "event": event,
        "event_name": same_event(event),
        "locations": locations,
        "purge_minutes": purge_minutes,
        "issued_jjjhhmm": issued,
        "sender": sender,
    }))
}

// ─── AIS (AIVDM) ────────────────────────────────────────────────────────

/// Reassembles multi-sentence AIVDM messages and decodes the common
/// types. One per AIS block (fragments are per channel + sequence id).
#[derive(Default)]
pub struct AisAssembler {
    /// (channel, seq id) → (expected count, payload so far, raw lines).
    pending: Vec<(String, String, usize, String, Vec<String>)>,
}

impl AisAssembler {
    /// Feed one `!AIVDM`/`!AIVDO` sentence. Returns the decoded record
    /// once a message is complete: the raw sentence(s) as `text` plus the
    /// parsed fields (or just `text` when the type isn't decoded).
    pub fn push(&mut self, line: &str) -> Option<Value> {
        let sentence = line.trim();
        let body = sentence
            .strip_prefix("!AIVDM,")
            .or_else(|| sentence.strip_prefix("!AIVDO,"))?;
        let f: Vec<&str> = body.split(',').collect();
        if f.len() < 6 {
            return None;
        }
        let count: usize = f[0].parse().ok()?;
        let num: usize = f[1].parse().ok()?;
        let seq = f[2].to_string();
        let channel = f[3].to_string();
        let payload = f[4];
        if count <= 1 {
            return Some(Self::finish(payload, &channel, &[sentence.to_string()]));
        }
        let pos = self
            .pending
            .iter()
            .position(|(c, s, _, _, _)| *c == channel && *s == seq);
        let idx = if num == 1 {
            if let Some(p) = pos {
                self.pending.remove(p);
            }
            self.pending
                .push((channel.clone(), seq, count, String::new(), Vec::new()));
            // Bound the buffer: a lost tail fragment must not pin memory.
            if self.pending.len() > 16 {
                self.pending.remove(0);
            }
            self.pending.len() - 1
        } else {
            pos?
        };
        let entry = &mut self.pending[idx];
        entry.3.push_str(payload);
        entry.4.push(sentence.to_string());
        if num == entry.2 {
            let (ch, _, _, full, raws) = self.pending.remove(idx);
            return Some(Self::finish(&full, &ch, &raws));
        }
        None
    }

    fn finish(payload: &str, channel: &str, raws: &[String]) -> Value {
        let mut rec = Map::new();
        rec.insert("text".into(), Value::from(raws.join("\n")));
        rec.insert("channel".into(), Value::from(channel));
        if let Some(Value::Object(fields)) = decode_ais_payload(payload) {
            rec.extend(fields);
        }
        Value::Object(rec)
    }
}

struct Bits(Vec<u8>);

impl Bits {
    fn from_armored(payload: &str) -> Self {
        let mut bits = Vec::with_capacity(payload.len() * 6);
        for c in payload.bytes() {
            let mut v = c.wrapping_sub(48);
            if v > 40 {
                v = v.wrapping_sub(8);
            }
            for i in (0..6).rev() {
                bits.push((v >> i) & 1);
            }
        }
        Self(bits)
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn u(&self, start: usize, len: usize) -> Option<u64> {
        if start + len > self.0.len() {
            return None;
        }
        Some(
            self.0[start..start + len]
                .iter()
                .fold(0u64, |acc, b| (acc << 1) | u64::from(*b)),
        )
    }
    fn i(&self, start: usize, len: usize) -> Option<i64> {
        let raw = self.u(start, len)?;
        let sign = 1u64 << (len - 1);
        #[allow(clippy::cast_possible_wrap)]
        Some(if raw & sign != 0 {
            raw as i64 - (1i64 << len)
        } else {
            raw as i64
        })
    }
    fn text(&self, start: usize, chars: usize) -> Option<String> {
        let mut s = String::with_capacity(chars);
        for k in 0..chars {
            let v = u8::try_from(self.u(start + 6 * k, 6)?).ok()?;
            s.push(char::from(if v < 32 { v + 64 } else { v }));
        }
        let t = s.trim_end_matches(['@', ' ']).trim().to_string();
        Some(t)
    }
}

fn pos(bits: &Bits, lon_at: usize, lat_at: usize) -> (Option<f64>, Option<f64>) {
    #[allow(clippy::cast_precision_loss)]
    let lon = bits
        .i(lon_at, 28)
        .map(|v| v as f64 / 600_000.0)
        .filter(|v| v.abs() <= 180.0);
    #[allow(clippy::cast_precision_loss)]
    let lat = bits
        .i(lat_at, 27)
        .map(|v| v as f64 / 600_000.0)
        .filter(|v| v.abs() <= 90.0);
    (lon, lat)
}

#[allow(clippy::cast_precision_loss)]
fn tenths(v: Option<u64>, invalid: u64) -> Option<f64> {
    v.filter(|x| *x != invalid).map(|x| x as f64 / 10.0)
}

/// Decode the 6-bit-armored payload of one complete AIS message. Types
/// 1–3 (class A position), 4 (base station), 5 (static/voyage), 18
/// (class B position), 24 (class B static). Others yield type + MMSI.
#[must_use]
pub fn decode_ais_payload(payload: &str) -> Option<Value> {
    let b = Bits::from_armored(payload);
    if b.len() < 38 {
        return None;
    }
    let msg_type = b.u(0, 6)?;
    let mmsi = b.u(8, 30)?;
    let mut out = Map::new();
    out.insert("msg_type".into(), Value::from(msg_type));
    out.insert("mmsi".into(), Value::from(mmsi));
    let put = |m: &mut Map<String, Value>, k: &str, v: Option<Value>| {
        if let Some(v) = v {
            m.insert(k.into(), v);
        }
    };
    match msg_type {
        1..=3 => {
            let (lon, lat) = pos(&b, 61, 89);
            put(&mut out, "lon", lon.map(Value::from));
            put(&mut out, "lat", lat.map(Value::from));
            put(&mut out, "nav_status", b.u(38, 4).map(Value::from));
            put(
                &mut out,
                "sog_kn",
                tenths(b.u(50, 10), 1023).map(Value::from),
            );
            put(
                &mut out,
                "cog_deg",
                tenths(b.u(116, 12), 3600).map(Value::from),
            );
            put(
                &mut out,
                "heading_deg",
                b.u(128, 9).filter(|h| *h != 511).map(Value::from),
            );
        }
        4 => {
            let (lon, lat) = pos(&b, 79, 107);
            put(&mut out, "lon", lon.map(Value::from));
            put(&mut out, "lat", lat.map(Value::from));
            out.insert("station".into(), Value::from("base"));
        }
        5 => {
            put(
                &mut out,
                "imo",
                b.u(40, 30).filter(|v| *v != 0).map(Value::from),
            );
            put(&mut out, "callsign", b.text(70, 7).map(Value::from));
            put(&mut out, "name", b.text(112, 20).map(Value::from));
            put(&mut out, "ship_type", b.u(232, 8).map(Value::from));
            put(&mut out, "destination", b.text(302, 20).map(Value::from));
        }
        18 => {
            let (lon, lat) = pos(&b, 57, 85);
            put(&mut out, "lon", lon.map(Value::from));
            put(&mut out, "lat", lat.map(Value::from));
            put(
                &mut out,
                "sog_kn",
                tenths(b.u(46, 10), 1023).map(Value::from),
            );
            put(
                &mut out,
                "cog_deg",
                tenths(b.u(112, 12), 3600).map(Value::from),
            );
            put(
                &mut out,
                "heading_deg",
                b.u(124, 9).filter(|h| *h != 511).map(Value::from),
            );
        }
        24 => match b.u(38, 2) {
            Some(0) => put(&mut out, "name", b.text(40, 20).map(Value::from)),
            Some(1) => {
                put(&mut out, "ship_type", b.u(40, 8).map(Value::from));
                put(&mut out, "callsign", b.text(90, 7).map(Value::from));
            }
            _ => {}
        },
        _ => {}
    }
    // Strip empty strings (unset text fields are all '@').
    out.retain(|_, v| v.as_str() != Some(""));
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pocsag_alpha_numeric_and_partial() {
        let v = parse_pocsag("POCSAG1200: Address: 1234567  Function: 3  Alpha:   HELLO WORLD")
            .unwrap();
        assert_eq!(v["baud"], 1200);
        assert_eq!(v["address"], 1_234_567);
        assert_eq!(v["function"], 3);
        assert_eq!(v["format"], "alpha");
        assert_eq!(v["message"], "HELLO WORLD");
        let v = parse_pocsag("POCSAG512: Address:   42  Function: 0  Numeric: 555-1234").unwrap();
        assert_eq!(v["format"], "numeric");
        assert_eq!(v["message"], "555-1234");
        let v = parse_pocsag("POCSAG2400: Address:       -  Function: -  Alpha:   X").unwrap();
        assert!(v["address"].is_null());
        assert!(parse_pocsag("FLEX|x").is_none());
    }

    #[test]
    fn flex_and_flex_next() {
        let v =
            parse_flex("FLEX|2026-10-10 12:00:00|1600/2/K/A|07.123|001234567|ALN|Call me | now")
                .unwrap();
        assert_eq!(v["protocol"], "FLEX");
        assert_eq!(v["baud"], 1600);
        assert_eq!(v["capcode"], 1_234_567);
        assert_eq!(v["format"], "alpha");
        assert_eq!(v["message"], "Call me | now");
        let v = parse_flex("FLEX_NEXT|3200/4|03.017.C|0002029569|SG|5|NUM|8675309").unwrap();
        assert_eq!(v["protocol"], "FLEX_NEXT");
        assert_eq!(v["capcode"], 2_029_569);
        assert_eq!(v["format"], "numeric");
        assert_eq!(v["message"], "8675309");
        assert!(v.get("frag").is_none());
    }

    #[test]
    fn flex_next_alpha_strips_fragment_tag_and_group_members() {
        // A complete single page: the `3.0.K` tag is metadata, not text.
        let v = parse_flex(
            "FLEX_NEXT|1600/2|07.123.A|0001234567|LS|5|ALN|3.0.K|From: EVS@kp.org | bed",
        )
        .unwrap();
        assert_eq!(v["capcode"], 1_234_567);
        assert_eq!(v["frag"], "K");
        assert_eq!(v["message"], "From: EVS@kp.org | bed");
        assert!(v.get("group").is_none());
        // A continuation fragment.
        let v =
            parse_flex("FLEX_NEXT|1600/2|07.123.A|0001234567|SS|5|ALN|1.1.F|more text").unwrap();
        assert_eq!(v["frag"], "F");
        assert_eq!(v["message"], "more text");
        // A group page lists the member capcodes before the text.
        let v = parse_flex(
            "FLEX_NEXT|1600/2|07.123.A|0002029569|SG|5|ALN|3.0.K|0001234567|0007654321|Code blue",
        )
        .unwrap();
        assert_eq!(v["capcode"], 2_029_569);
        assert_eq!(v["group"], json!([1_234_567, 7_654_321]));
        assert_eq!(v["message"], "Code blue");
        // A single page whose text happens to be digits keeps them.
        let v =
            parse_flex("FLEX_NEXT|1600/2|07.123.A|0001234567|SS|5|ALN|3.0.K|5551234567").unwrap();
        assert_eq!(v["message"], "5551234567");
        // Unknown page type: hex word dump, no tag.
        let v =
            parse_flex("FLEX_NEXT|1600/2|07.123.A|0001234567|SS|7|UNK|0001a2b3 000c4d5e").unwrap();
        assert_eq!(v["format"], "unknown");
        assert_eq!(v["message"], "0001a2b3 000c4d5e");
    }

    #[test]
    fn same_header_and_eom() {
        let v = parse_same("EAS: ZCZC-WXR-RWT-012057-012081+0030-1231500-KTBW/NWS-").unwrap();
        assert_eq!(v["originator"], "WXR");
        assert_eq!(v["event"], "RWT");
        assert_eq!(v["event_name"], "Required Weekly Test");
        assert_eq!(v["locations"], json!(["012057", "012081"]));
        assert_eq!(v["purge_minutes"], 30);
        assert_eq!(v["issued_jjjhhmm"], "1231500");
        assert_eq!(v["sender"], "KTBW/NWS");
        assert_eq!(parse_same("EAS: NNNN").unwrap()["type"], "end_of_message");
        assert!(parse_same("EAS: garbage").is_none());
    }

    #[test]
    fn ais_type1_position() {
        // Canonical example (gpsd AIVDM docs).
        let mut a = AisAssembler::default();
        let v = a
            .push("!AIVDM,1,1,,B,15M67FC000G?ufbE`FepT@3n00Sa,0*5C")
            .unwrap();
        assert_eq!(v["msg_type"], 1);
        assert_eq!(v["mmsi"], 366_053_209);
        assert_eq!(v["channel"], "B");
        let lat = v["lat"].as_f64().unwrap();
        let lon = v["lon"].as_f64().unwrap();
        assert!((lat - 37.802_118).abs() < 1e-3, "{lat}");
        assert!((lon - -122.341_618).abs() < 1e-3, "{lon}");
    }

    #[test]
    fn ais_type5_two_fragments() {
        let mut a = AisAssembler::default();
        assert!(a
            .push("!AIVDM,2,1,3,B,55P5TL01VIaAL@7WKO@mBplU@<PDhh000000001S;AJ::4A80?4i@E53,0*3E")
            .is_none());
        let v = a.push("!AIVDM,2,2,3,B,1@0000000000000,2*55").unwrap();
        assert_eq!(v["msg_type"], 5);
        assert_eq!(v["mmsi"], 369_190_000);
        assert_eq!(v["name"], "MT.MITCHELL");
        assert_eq!(v["callsign"], "WDA9674");
        assert!(
            v["text"].as_str().unwrap().contains('\n'),
            "both raw sentences kept"
        );
    }
}
