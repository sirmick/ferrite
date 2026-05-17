//! APRS parsing + wire record for the `ui:aprs` advanced view.
//!
//! `PacketDemod` runs multimon-ng, which prints each AX.25 UI frame
//! in TNC2 monitor form:
//!
//! ```text
//! APRS: KK6ABC-9>APRS,WIDE1-1,WIDE2-1:!3745.60N/12225.10W>hiking
//! ```
//!
//! multimon does *not* interpret the APRS info field — that's this
//! module. We resolve the high-value, well-specified formats:
//!
//! - uncompressed position (`!` `=` `/` `@`, optional timestamp)
//! - Base-91 compressed position
//! - object (`;`) and item (`)`)
//! - status (`>`) and message (`:`)
//!
//! Mic-E (`` ` `` / `'`) is emitted as `kind:"mic-e"` *without*
//! lat/lon — its position is bit-packed across the AX.25 destination
//! address, which multimon's printed TNC2 line mangles; decoding it
//! from the raw frame is a deliberate follow-up. Such stations still
//! show in the table/console, just not on the map. Anything
//! unrecognised is `kind:"other"` carrying the raw info so nothing is
//! silently dropped — same philosophy as `parse_ft8`.
//!
//! (APRS symbols are not surfaced yet — the map draws APRS stations
//! as plain markers, same as FT8; symbol glyphs are a later nicety.)
//!
//! One newline-terminated JSON object per frame on the `events` port,
//! same `PortType::Events` transport RDS / FT8 / ADS-B use, consumed
//! by `web/src/lib/aprs/store.svelte.ts`.

use std::io::Write;

/// Parsed APRS frame, ready to serialize. Borrows from the multimon
/// line — built and written within one drain.
pub struct AprsSpot<'a> {
    /// Source callsign-SSID (e.g. `KK6ABC-9`). Always present.
    pub call: &'a str,
    /// Digipeater path, comma-joined (`WIDE1-1,WIDE2-1`); may be "".
    pub path: &'a str,
    /// `position` | `object` | `item` | `status` | `message` |
    /// `mic-e` | `other`.
    pub kind: &'static str,
    /// Decoded position, when the format carried one.
    pub pos: Option<(f64, f64)>,
    /// Object/item name, or message addressee, when applicable.
    pub name: Option<&'a str>,
    /// Free text — comment (position), status text, or message body.
    pub text: &'a str,
    /// Raw APRS info field (always — the console shows this verbatim).
    pub raw: &'a str,
}

impl AprsSpot<'_> {
    pub fn write_json(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(br#"{"call":""#);
        write_str(out, self.call);
        out.extend_from_slice(br#"","kind":""#);
        out.extend_from_slice(self.kind.as_bytes());
        out.push(b'"');
        if !self.path.is_empty() {
            out.extend_from_slice(br#","path":""#);
            write_str(out, self.path);
            out.push(b'"');
        }
        if let Some((lat, lon)) = self.pos {
            let _ = write!(out, r#","lat":{lat:.5},"lon":{lon:.5}"#);
        }
        if let Some(name) = self.name {
            out.extend_from_slice(br#","name":""#);
            write_str(out, name);
            out.push(b'"');
        }
        if !self.text.is_empty() {
            out.extend_from_slice(br#","text":""#);
            write_str(out, self.text);
            out.push(b'"');
        }
        out.extend_from_slice(br#","raw":""#);
        write_str(out, self.raw);
        out.extend_from_slice(b"\"}\n");
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    for &b in s.as_bytes() {
        let ch = if (0x20..=0x7E).contains(&b) { b } else { b' ' };
        match ch {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            _ => out.push(ch),
        }
    }
}

/// Parsed info field. `pos` is `(lat, lon)`; `kind` is static; the
/// rest borrow the input.
pub struct Parsed<'a> {
    pub kind: &'static str,
    pub pos: Option<(f64, f64)>,
    pub name: Option<&'a str>,
    pub text: &'a str,
}

/// Split a multimon `APRS: src>dest,via,via:info` line into
/// `(source, path, info)`. `None` if it isn't an APRS line.
pub fn split_tnc2(line: &str) -> Option<(&str, &str, &str)> {
    let body = line.strip_prefix("APRS: ")?;
    let (header, info) = body.split_once(':')?;
    let (src, rest) = header.split_once('>')?;
    // rest = DEST[,VIA…]; path is everything after the first comma
    // (DEST is the generic tocall / Mic-E data, not a digi).
    let path = rest.split_once(',').map(|(_, p)| p).unwrap_or("");
    Some((src.trim(), path.trim(), info))
}

/// Decode an APRS info field. Tolerant: unrecognised → `other`.
pub fn parse_aprs(info: &str) -> Parsed<'_> {
    let other = |text| Parsed {
        kind: "other",
        pos: None,
        name: None,
        text,
    };
    let Some(dti) = info.chars().next() else {
        return other("");
    };
    let body = &info[1..];
    match dti {
        '!' | '=' => position(body, "position"),
        // Position with a 7-char timestamp prefix.
        '/' | '@' => position(body.get(7..).unwrap_or(""), "position"),
        '>' => Parsed {
            kind: "status",
            pos: None,
            name: None,
            text: body,
        },
        // Message: 9-char addressee, `:`, then text.
        ':' => Parsed {
            kind: "message",
            pos: None,
            name: body.get(0..9).map(str::trim),
            text: body.get(10..).unwrap_or(""),
        },
        ';' => named_position(body, "object"),
        ')' => item(body),
        // Mic-E position is in the (mangled) AX.25 dest addr — table
        // / console only until a raw-frame decoder lands.
        '`' | '\'' => Parsed {
            kind: "mic-e",
            pos: None,
            name: None,
            text: body,
        },
        _ => other(info),
    }
}

/// Uncompressed `DDMM.mmN{sym}DDDMM.mmW{sym}` or Base-91 compressed,
/// after any timestamp/name prefix has been stripped.
fn position<'a>(s: &'a str, kind: &'static str) -> Parsed<'a> {
    // Uncompressed: lat 0..8 (`.` at 4), table 8, lon 9..18, code 18,
    // comment 19..
    if s.len() >= 19 && s.as_bytes().get(4) == Some(&b'.') {
        if let Some(p) = parse_uncompressed(s) {
            return Parsed {
                kind,
                pos: Some(p),
                name: None,
                text: s.get(19..).unwrap_or(""),
            };
        }
    }
    // Compressed: table 0, lat 1..5, lon 5..9, code 9, cs/type 10..13,
    // comment 13..
    if s.len() >= 13 {
        if let Some(p) = parse_compressed(s.get(1..9).unwrap_or("")) {
            return Parsed {
                kind,
                pos: Some(p),
                name: None,
                text: s.get(13..).unwrap_or(""),
            };
        }
    }
    Parsed {
        kind,
        pos: None,
        name: None,
        text: s,
    }
}

fn parse_uncompressed(s: &str) -> Option<(f64, f64)> {
    let b = s.as_bytes();
    let lat_deg: f64 = s.get(0..2)?.parse().ok()?;
    let lat_min: f64 = s.get(2..7)?.parse().ok()?;
    let mut lat = lat_deg + lat_min / 60.0;
    match b.get(7)? {
        b'S' => lat = -lat,
        b'N' => {}
        _ => return None,
    }
    let lon_deg: f64 = s.get(9..12)?.parse().ok()?;
    let lon_min: f64 = s.get(12..17)?.parse().ok()?;
    let mut lon = lon_deg + lon_min / 60.0;
    match b.get(17)? {
        b'W' => lon = -lon,
        b'E' => {}
        _ => return None,
    }
    (lat.is_finite() && lon.is_finite()).then_some((lat, lon))
}

/// Base-91 compressed: 4 bytes lat then 4 bytes lon, each a radix-91
/// big number offset from `'!'` (33).
fn parse_compressed(cc: &str) -> Option<(f64, f64)> {
    let b = cc.as_bytes();
    if b.len() != 8 || b.iter().any(|&c| !(33..=124).contains(&c)) {
        return None;
    }
    let v = |r: &[u8]| r.iter().fold(0i64, |a, &c| a * 91 + i64::from(c - 33));
    let lat = 90.0 - v(&b[0..4]) as f64 / 380926.0;
    let lon = -180.0 + v(&b[4..8]) as f64 / 190463.0;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// Object: `;NAME_____{*|_}DDHHMMz<position>` — name is 9 chars, then
/// a live/killed flag, then 7-char timestamp, then a position.
fn named_position<'a>(body: &'a str, kind: &'static str) -> Parsed<'a> {
    let name = body.get(0..9).map(str::trim);
    let rest = body.get(9 + 1 + 7..).unwrap_or("");
    let mut p = position(rest, kind);
    p.name = name;
    p
}

/// Item: `)NAME{!|_}<position>` — name is 3–9 chars terminated by the
/// `!` (live) or `_` (killed) flag.
fn item(body: &str) -> Parsed<'_> {
    if let Some(i) = body.find(['!', '_']) {
        let mut p = position(body.get(i + 1..).unwrap_or(""), "item");
        p.name = Some(&body[..i]);
        p
    } else {
        Parsed {
            kind: "item",
            pos: None,
            name: None,
            text: body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_aprs, split_tnc2, AprsSpot};

    #[test]
    fn splits_tnc2_header() {
        let (src, path, info) =
            split_tnc2("APRS: KK6ABC-9>APRS,WIDE1-1,WIDE2-1:!3745.60N/12225.10W>hi").unwrap();
        assert_eq!(src, "KK6ABC-9");
        assert_eq!(path, "WIDE1-1,WIDE2-1");
        assert_eq!(info, "!3745.60N/12225.10W>hi");
        assert!(split_tnc2("AFSK1200: fm ... raw ax25").is_none());
    }

    #[test]
    fn uncompressed_position() {
        let p = parse_aprs("!3745.60N/12225.10W>on the road");
        assert_eq!(p.kind, "position");
        let (lat, lon) = p.pos.unwrap();
        assert!((lat - 37.76).abs() < 0.001, "lat {lat}");
        assert!((lon + 122.4183).abs() < 0.001, "lon {lon}");
        assert_eq!(p.text, "on the road");
    }

    #[test]
    fn timestamped_position_south_west() {
        let p = parse_aprs("@092345z3358.00S/15112.00W-comment");
        let (lat, lon) = p.pos.unwrap();
        assert!(lat < 0.0 && lon < 0.0, "S/W signs: {lat},{lon}");
        assert!((lat + 33.9667).abs() < 0.01, "lat {lat}");
    }

    #[test]
    fn status_and_message() {
        let s = parse_aprs(">Net tonight 8pm");
        assert_eq!(s.kind, "status");
        assert_eq!(s.text, "Net tonight 8pm");
        let m = parse_aprs(":WU2Z     :Testing{003");
        assert_eq!(m.kind, "message");
        assert_eq!(m.name, Some("WU2Z"));
        assert_eq!(m.text, "Testing{003");
    }

    #[test]
    fn mic_e_flagged_not_mapped() {
        let p = parse_aprs("`(_fn\"Oj/");
        assert_eq!(p.kind, "mic-e");
        assert!(p.pos.is_none());
    }

    #[test]
    fn serializes_contract_json() {
        let mut out = Vec::new();
        AprsSpot {
            call: "KK6ABC-9",
            path: "WIDE1-1",
            kind: "position",
            pos: Some((37.76, -122.418)),
            name: None,
            text: "hiking",
            raw: "!3745.60N/12225.10W>hiking",
        }
        .write_json(&mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with(r#"{"call":"KK6ABC-9","kind":"position""#));
        assert!(s.contains(r#""lat":37.76000,"lon":-122.41800"#), "got {s}");
        assert!(s.ends_with("}\n"));
    }
}
