//! Wire record for the `ui:adsb` advanced view.
//!
//! `AdsbDemod` polls `ferrite_dump1090::Dump1090::aircraft_snapshot`
//! on a timer and emits one newline-terminated JSON object per tracked
//! aircraft on its `events` port — same `PortType::Events` transport
//! RDS / FT8 use, consumed by the web `ui:adsb` store. Unlike FT8's
//! per-message stream this is a *full snapshot* each tick: the store
//! upserts by `icao` and ages rows out.
//!
//! Wire shape (optional fields omitted when absent):
//!
//! ```text
//! {"icao":"4CA1FA","flight":"RYR1234","lat":53.34,"lon":-6.25,
//!  "alt":37000,"gs":420,"trk":95,"msgs":312,"age":3}
//! ```
//!
//! `icao`/`alt`/`gs`/`trk`/`msgs`/`age` are always present; `flight`
//! is omitted until an identification frame; `lat`/`lon` are omitted
//! until a CPR pair decodes (an aircraft is heard before it's placed).

use std::io::Write;

/// One aircraft row, ready to serialize. Borrows the callsign from the
/// snapshot — built and written within a single drain, never stored.
pub struct AircraftSpot<'a> {
    /// ICAO 24-bit address — the stable per-airframe key the store
    /// upserts on. Serialized as 6 upper-hex digits.
    pub icao: u32,
    /// Callsign / flight number; empty until an identification frame.
    pub flight: &'a str,
    /// `(lat, lon)` once CPR-decoded; `None` until then.
    pub pos: Option<(f64, f64)>,
    /// Barometric altitude, feet.
    pub alt_ft: i32,
    /// Ground speed, knots.
    pub gs_kt: i32,
    /// Track, degrees 0–359.
    pub trk_deg: i32,
    /// Total Mode-S messages from this aircraft this session.
    pub msgs: i64,
    /// Seconds since the last message (the store ages rows on this).
    pub age_s: i32,
}

impl AircraftSpot<'_> {
    /// Append `{…}\n` to `out`. `flight` is escaped to the
    /// printable-ASCII subset (callsigns are AIS-charset: A–Z 0–9
    /// space — but be defensive, same as the RDS/FT8 serializers).
    pub fn write_json(&self, out: &mut Vec<u8>) {
        let _ = write!(out, r#"{{"icao":"{:06X}""#, self.icao);
        let flight = self.flight.trim();
        if !flight.is_empty() {
            out.extend_from_slice(br#","flight":""#);
            write_str(out, flight);
            out.push(b'"');
        }
        if let Some((lat, lon)) = self.pos {
            let _ = write!(out, r#","lat":{lat:.5},"lon":{lon:.5}"#);
        }
        let _ = write!(
            out,
            r#","alt":{},"gs":{},"trk":{},"msgs":{},"age":{}}}"#,
            self.alt_ft, self.gs_kt, self.trk_deg, self.msgs, self.age_s
        );
        out.push(b'\n');
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

#[cfg(test)]
mod tests {
    use super::AircraftSpot;

    #[test]
    fn positioned_aircraft_with_flight() {
        let mut out = Vec::new();
        AircraftSpot {
            icao: 0x4C_A1FA,
            flight: "RYR1234 ",
            pos: Some((53.34812, -6.24921)),
            alt_ft: 37_000,
            gs_kt: 420,
            trk_deg: 95,
            msgs: 312,
            age_s: 3,
        }
        .write_json(&mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"icao\":\"4CA1FA\",\"flight\":\"RYR1234\",\"lat\":53.34812,\
             \"lon\":-6.24921,\"alt\":37000,\"gs\":420,\"trk\":95,\
             \"msgs\":312,\"age\":3}\n"
        );
    }

    #[test]
    fn heard_but_not_yet_placed_omits_pos_and_flight() {
        let mut out = Vec::new();
        AircraftSpot {
            icao: 0xAB_CDEF,
            flight: "",
            pos: None,
            alt_ft: 0,
            gs_kt: 0,
            trk_deg: 0,
            msgs: 1,
            age_s: 0,
        }
        .write_json(&mut out);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            "{\"icao\":\"ABCDEF\",\"alt\":0,\"gs\":0,\"trk\":0,\"msgs\":1,\"age\":0}\n"
        );
        assert!(!s.contains("flight"));
        assert!(!s.contains("lat"));
    }
}
