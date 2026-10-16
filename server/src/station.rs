//! The station profile: where the receiver is (`grid`) and how to set
//! its front end per band — which antenna port, what gain, AGC or not.
//! One persisted store value (kind `station`), so `listen`, the sweep,
//! spot maps, every tab and the AI all read the same table instead of
//! the operator (or Claude) re-deriving "HF wants the long-wire at 40 dB,
//! AGC off" from prose on every hop.
//!
//! `GET /api/station` reads it, `POST /api/station` merges top-level keys
//! (`bands` replaces the whole table), `GET /api/station/at?freq_hz=`
//! answers "what front end for this frequency".

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::decoder_store::DecoderStore;
use crate::error::AppError;

/// The store kind holding [`Station`] (persisted).
pub const KIND: &str = "station";

/// Station description shared by every surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Station {
    /// Maidenhead locator of the receiver (`CM87`, `CM87wj`): the origin
    /// for spot-map distances and bearings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    /// Front-end settings by frequency range. The narrowest range that
    /// contains a frequency wins, so a broad "HF" row can carry a
    /// narrower exception.
    pub bands: Vec<BandProfile>,
}

/// How to set the front end for one frequency range. Unset fields leave
/// the radio as it is.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BandProfile {
    pub lo_hz: f64,
    pub hi_hz: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Antenna port name as the driver lists it (`Antenna C`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub antenna: Option<String>,
    /// Gain dial, dB (manual gain; implies `agc: false` unless `agc` is
    /// set explicitly).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agc: Option<bool>,
}

impl BandProfile {
    /// The `src` params this row sets (`antenna`, `gain_db`, `agc`).
    #[must_use]
    pub fn src_params(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        if let Some(a) = &self.antenna {
            m.insert("antenna".into(), json!(a));
        }
        if let Some(g) = self.gain_db {
            m.insert("gain_db".into(), json!(g));
        }
        match (self.agc, self.gain_db) {
            (Some(agc), _) => {
                m.insert("agc".into(), json!(agc));
            }
            (None, Some(_)) => {
                m.insert("agc".into(), json!(false));
            }
            (None, None) => {}
        }
        m
    }

    fn contains(&self, hz: f64) -> bool {
        (self.lo_hz..self.hi_hz).contains(&hz)
    }
}

impl Station {
    /// The front-end row for `hz`: the narrowest range containing it.
    #[must_use]
    pub fn band_for(&self, hz: f64) -> Option<&BandProfile> {
        self.bands.iter().filter(|b| b.contains(hz)).min_by(|a, b| {
            (a.hi_hz - a.lo_hz)
                .partial_cmp(&(b.hi_hz - b.lo_hz))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(g) = &self.grid {
            if !valid_grid(g) {
                return Err(format!(
                    "grid {g:?} is not a Maidenhead locator (e.g. CM87, CM87wj)"
                ));
            }
        }
        for (i, b) in self.bands.iter().enumerate() {
            if !(b.lo_hz.is_finite() && b.hi_hz.is_finite() && b.lo_hz >= 0.0 && b.lo_hz < b.hi_hz)
            {
                return Err(format!("bands[{i}]: need 0 <= lo_hz < hi_hz"));
            }
            if let Some(g) = b.gain_db {
                if !(0.0..=100.0).contains(&g) {
                    return Err(format!("bands[{i}]: gain_db must be 0–100"));
                }
            }
        }
        Ok(())
    }
}

/// Four or six character Maidenhead locator.
fn valid_grid(g: &str) -> bool {
    let b = g.as_bytes();
    let field =
        |c: u8| c.to_ascii_uppercase().is_ascii_uppercase() && c.to_ascii_uppercase() <= b'R';
    let sub = |c: u8| c.to_ascii_lowercase().is_ascii_lowercase() && c.to_ascii_lowercase() <= b'x';
    match b.len() {
        4 => field(b[0]) && field(b[1]) && b[2].is_ascii_digit() && b[3].is_ascii_digit(),
        6 => {
            field(b[0])
                && field(b[1])
                && b[2].is_ascii_digit()
                && b[3].is_ascii_digit()
                && sub(b[4])
                && sub(b[5])
        }
        _ => false,
    }
}

/// The current station profile (default when none was ever set).
#[must_use]
pub fn load(store: &DecoderStore) -> Station {
    store
        .snapshot_kind(KIND)
        .and_then(|k| k.current.into_values().next())
        .and_then(|r| serde_json::from_value::<Station>(r.data).ok())
        .unwrap_or_default()
}

/// The receiver grid: the station profile's, else `FERRITE_RX_GRID`.
#[must_use]
pub fn grid(store: &DecoderStore) -> Option<String> {
    load(store)
        .grid
        .or_else(|| std::env::var("FERRITE_RX_GRID").ok())
        .filter(|g| !g.trim().is_empty())
}

/// `GET /api/station`.
pub async fn get_station(State(state): State<AppState>) -> Json<Station> {
    Json(load(&state.decoder_store()))
}

/// `POST /api/station {grid?, callsign?, bands?}` — merge top-level keys
/// (a `null` clears one; `bands` replaces the table). Persisted and
/// mirrored to every tab.
pub async fn post_station(
    State(state): State<AppState>,
    Json(patch): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Station>, AppError> {
    let store = state.decoder_store();
    let mut v = serde_json::to_value(load(&store)).unwrap_or_default();
    if let Some(m) = v.as_object_mut() {
        for (k, val) in patch {
            if val.is_null() {
                m.remove(&k);
            } else {
                m.insert(k, val);
            }
        }
    }
    let next: Station = serde_json::from_value(v)
        .map_err(|e| AppError::bad_request("STATION_REJECTED", format!("{e}")))?;
    next.validate()
        .map_err(|e| AppError::bad_request("STATION_REJECTED", e))?;
    store.apply(KIND, serde_json::to_value(&next).unwrap_or_default());
    Ok(Json(next))
}

#[derive(Debug, Deserialize)]
pub struct AtQuery {
    pub freq_hz: f64,
}

/// `GET /api/station/at?freq_hz=` — the front-end row for a frequency
/// (`band: null` when the table has none) plus the `src` params it sets.
pub async fn get_at(State(state): State<AppState>, Query(q): Query<AtQuery>) -> Json<Value> {
    let st = load(&state.decoder_store());
    let band = st.band_for(q.freq_hz);
    Json(json!({
        "freq_hz": q.freq_hz,
        "band": band,
        "src": band.map(BandProfile::src_params).unwrap_or_default(),
    }))
}

/// The sweep's `bands` param: one catch-all row for an explicit
/// antenna / gain override, else the station profile's rows (when
/// `use_station`), the override's fields winning on every row.
pub fn sweep_bands(
    station: &Station,
    use_station: bool,
    antenna: Option<&str>,
    gain_db: Option<f64>,
) -> Value {
    let mut rows: Vec<Value> = if use_station {
        station
            .bands
            .iter()
            .filter(|b| b.antenna.is_some() || b.gain_db.is_some())
            .map(|b| {
                json!({
                    "lo_hz": b.lo_hz, "hi_hz": b.hi_hz,
                    "antenna": antenna.map(str::to_string).or_else(|| b.antenna.clone()),
                    "gain_db": gain_db.or(b.gain_db),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    if antenna.is_some() || gain_db.is_some() {
        rows.push(json!({
            "lo_hz": 0.0, "hi_hz": 1e12, "antenna": antenna, "gain_db": gain_db,
        }));
    }
    Value::Array(rows)
}

/// The station profile's band table as sweep `bands` (no override).
pub fn station_bands(station: &Station) -> Value {
    sweep_bands(station, true, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(lo: f64, hi: f64, label: &str) -> BandProfile {
        BandProfile {
            lo_hz: lo,
            hi_hz: hi,
            label: Some(label.into()),
            ..BandProfile::default()
        }
    }

    #[test]
    fn narrowest_containing_band_wins() {
        let st = Station {
            bands: vec![
                row(0.0, 30e6, "HF"),
                row(9e6, 11e6, "31 m"),
                row(118e6, 150e6, "VHF low"),
            ],
            ..Station::default()
        };
        assert_eq!(
            st.band_for(7.074e6).and_then(|b| b.label.as_deref()),
            Some("HF")
        );
        assert_eq!(
            st.band_for(10e6).and_then(|b| b.label.as_deref()),
            Some("31 m")
        );
        assert_eq!(st.band_for(30e6), None, "hi_hz is exclusive");
        assert_eq!(st.band_for(460e6), None);
    }

    #[test]
    fn manual_gain_implies_agc_off_unless_stated() {
        let mut b = BandProfile {
            antenna: Some("Antenna C".into()),
            gain_db: Some(40.0),
            ..BandProfile::default()
        };
        assert_eq!(
            Value::Object(b.src_params()),
            json!({"antenna": "Antenna C", "gain_db": 40.0, "agc": false})
        );
        b.gain_db = None;
        b.agc = Some(true);
        assert_eq!(
            Value::Object(b.src_params()),
            json!({"antenna": "Antenna C", "agc": true})
        );
    }

    #[test]
    fn validates_grid_and_ranges() {
        assert!(valid_grid("CM87"));
        assert!(valid_grid("cm87wj"));
        assert!(!valid_grid("CM8"));
        assert!(!valid_grid("ZZ99"));
        let bad = Station {
            bands: vec![row(30e6, 1e6, "backwards")],
            ..Station::default()
        };
        assert!(bad.validate().is_err());
    }
}
