//! Shared wire record for weak-signal "advanced view" event sinks.
//!
//! `Ft8Demod` (FT8/FT4) and `WsprDemod` each emit one
//! newline-terminated JSON object per decoded message on their
//! `events` output port, folded into the server-side store via the
//! `EventStore` sink (kind `ft8`) that `env_split` inserts for the
//! `ui:ft8` wire, then mirrored to the web `ft8` store. Keeping the
//! serializer here means the two producers cannot drift in field
//! names or shape.
//!
//! Wire shape (optional fields are omitted entirely when absent, so
//! the consumer treats missing == null):
//!
//! ```text
//! {"t":"ft8","utc":1747400000,"de":"K1ABC","dx":"W9XYZ",
//!  "grid":"FN42","snr":-15,"dt":0.2,"freq":1074,"msg":"W9XYZ K1ABC FN42"}
//! {"t":"wspr","utc":1747400040,"de":"K1ABC","grid":"FN42",
//!  "snr":-22,"dt":0.5,"freq":1493,"pwr":37,"drift":0,"msg":"K1ABC FN42 37"}
//! ```

use std::io::Write;

/// One decoded weak-signal spot, ready to serialize. Borrows its
/// string fields from the decoder's message buffers — built and
/// written within a single drain, never stored.
pub struct DigitalSpot<'a> {
    /// Mode tag — `"ft8"`, `"ft4"`, or `"wspr"`. Drives the badge and
    /// which optional columns the view shows.
    pub mode: &'a str,
    /// Slot epoch seconds (UTC-aligned slot start).
    pub utc: u64,
    /// Transmitting station callsign (the sender).
    pub de: &'a str,
    /// Addressed station, if any. `None` for CQ calls and WSPR
    /// beacons.
    pub dx: Option<&'a str>,
    /// 4/6-char Maidenhead locator of `de`, when the message carried
    /// one. `None` for report/RR73/73/free-text messages.
    pub grid: Option<&'a str>,
    /// SNR estimate, dB (conventionally integer for these modes).
    pub snr: f32,
    /// Time offset of the decode within the slot, seconds.
    pub dt: f32,
    /// Audio-band frequency offset, Hz.
    pub freq: f32,
    /// Raw decoded message text.
    pub msg: &'a str,
    /// Reported TX power, dBm (WSPR only).
    pub pwr_dbm: Option<i32>,
    /// Carrier drift, Hz (WSPR only).
    pub drift_hz: Option<i32>,
}

impl DigitalSpot<'_> {
    /// Append `{…}\n` to `out`. Strings are escaped for the
    /// printable-ASCII subset callsigns/grids/messages live in;
    /// stray control bytes become spaces — the same defensive
    /// approach the RDS PS serializer uses, keeping the consumer's
    /// `JSON.parse` happy without a separate transform.
    pub fn write_json(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(br#"{"t":""#);
        write_str(out, self.mode);
        let _ = write!(out, r#"","utc":{}"#, self.utc);
        out.extend_from_slice(br#","de":""#);
        write_str(out, self.de);
        out.push(b'"');
        if let Some(dx) = self.dx {
            out.extend_from_slice(br#","dx":""#);
            write_str(out, dx);
            out.push(b'"');
        }
        if let Some(grid) = self.grid {
            out.extend_from_slice(br#","grid":""#);
            write_str(out, grid);
            out.push(b'"');
        }
        #[allow(clippy::cast_possible_truncation)]
        let snr = self.snr.round() as i32;
        let _ = write!(
            out,
            r#","snr":{},"dt":{:.1},"freq":{:.0}"#,
            snr, self.dt, self.freq
        );
        if let Some(p) = self.pwr_dbm {
            let _ = write!(out, r#","pwr":{}"#, p);
        }
        if let Some(d) = self.drift_hz {
            let _ = write!(out, r#","drift":{}"#, d);
        }
        out.extend_from_slice(br#","msg":""#);
        write_str(out, self.msg);
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

#[cfg(test)]
mod tests {
    use super::DigitalSpot;

    #[test]
    fn ft8_directed_with_grid() {
        let mut out = Vec::new();
        DigitalSpot {
            mode: "ft8",
            utc: 1_747_400_000,
            de: "K1ABC",
            dx: Some("W9XYZ"),
            grid: Some("FN42"),
            snr: -15.0,
            dt: 0.2,
            freq: 1074.0,
            msg: "W9XYZ K1ABC FN42",
            pwr_dbm: None,
            drift_hz: None,
        }
        .write_json(&mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"t\":\"ft8\",\"utc\":1747400000,\"de\":\"K1ABC\",\"dx\":\"W9XYZ\",\
             \"grid\":\"FN42\",\"snr\":-15,\"dt\":0.2,\"freq\":1074,\
             \"msg\":\"W9XYZ K1ABC FN42\"}\n"
        );
    }

    #[test]
    fn wspr_beacon_omits_dx_includes_power() {
        let mut out = Vec::new();
        DigitalSpot {
            mode: "wspr",
            utc: 1_747_400_040,
            de: "K1ABC",
            dx: None,
            grid: Some("FN42"),
            snr: -22.4,
            dt: 0.5,
            freq: 1493.0,
            msg: "K1ABC FN42 37",
            pwr_dbm: Some(37),
            drift_hz: Some(0),
        }
        .write_json(&mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("\"dx\""));
        assert!(s.contains("\"pwr\":37"));
        assert!(s.contains("\"snr\":-22")); // rounded
        assert!(s.ends_with("}\n"));
    }
}
