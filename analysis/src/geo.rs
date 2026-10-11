//! Maidenhead grid squares → positions, distances and bearings — for
//! mapping where FT8 / WSPR spots came from.

use serde::Serialize;

/// Mean Earth radius, km.
const EARTH_KM: f64 = 6371.0;

/// Centre of a 4- or 6-character Maidenhead locator (`CM87`, `cm87vx`),
/// as `(lat, lon)` degrees. `None` for anything else.
#[must_use]
pub fn grid_to_latlon(grid: &str) -> Option<(f64, f64)> {
    let g = grid.trim().as_bytes();
    if g.len() != 4 && g.len() != 6 {
        return None;
    }
    let up = |c: u8| c.to_ascii_uppercase();
    let field = |c: u8| {
        let c = up(c);
        (b'A'..=b'R').contains(&c).then(|| f64::from(c - b'A'))
    };
    let digit = |c: u8| c.is_ascii_digit().then(|| f64::from(c - b'0'));
    let mut lon = field(g[0])? * 20.0 - 180.0 + digit(g[2])? * 2.0;
    let mut lat = field(g[1])? * 10.0 - 90.0 + digit(g[3])?;
    if g.len() == 6 {
        let sub = |c: u8| {
            let c = up(c);
            (b'A'..=b'X').contains(&c).then(|| f64::from(c - b'A'))
        };
        lon += sub(g[4])? * (2.0 / 24.0) + 1.0 / 24.0;
        lat += sub(g[5])? * (1.0 / 24.0) + 0.5 / 24.0;
    } else {
        lon += 1.0;
        lat += 0.5;
    }
    Some((lat, lon))
}

/// Great-circle distance in km.
#[must_use]
pub fn distance_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, lo1) = (a.0.to_radians(), a.1.to_radians());
    let (la2, lo2) = (b.0.to_radians(), b.1.to_radians());
    let h = ((la2 - la1) / 2.0).sin().powi(2)
        + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
    2.0 * EARTH_KM * h.sqrt().min(1.0).asin()
}

/// Initial great-circle bearing from `a` to `b`, degrees 0..360.
#[must_use]
pub fn bearing_deg(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, lo1) = (a.0.to_radians(), a.1.to_radians());
    let (la2, lo2) = (b.0.to_radians(), b.1.to_radians());
    let y = (lo2 - lo1).sin() * la2.cos();
    let x = la1.cos() * la2.sin() - la1.sin() * la2.cos() * (lo2 - lo1).cos();
    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

/// One heard station, placed.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Spot {
    pub call: String,
    pub grid: String,
    pub lat: f64,
    pub lon: f64,
    /// From the receive grid, when one is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub km: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearing: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr: Option<f64>,
    /// Unix ms of the latest decode.
    pub t: u64,
    /// Decodes heard from this station in the window.
    pub count: u32,
}

/// Place spots: one per callsign (its latest decode with a grid),
/// farthest first when the receive grid is known, else newest first.
/// `heard` is `(call, grid, snr, t_ms)`.
#[must_use]
pub fn place(heard: &[(String, String, Option<f64>, u64)], rx: Option<(f64, f64)>) -> Vec<Spot> {
    let mut by_call: std::collections::BTreeMap<&str, Spot> = std::collections::BTreeMap::new();
    for (call, grid, snr, t) in heard {
        let Some(pos) = grid_to_latlon(grid) else {
            continue;
        };
        let e = by_call.entry(call.as_str()).or_insert_with(|| Spot {
            call: call.clone(),
            grid: grid.to_ascii_uppercase(),
            lat: pos.0,
            lon: pos.1,
            km: rx.map(|r| distance_km(r, pos)),
            bearing: rx.map(|r| bearing_deg(r, pos)),
            snr: *snr,
            t: *t,
            count: 0,
        });
        e.count += 1;
        if *t >= e.t {
            e.t = *t;
            e.snr = *snr;
        }
    }
    let mut out: Vec<Spot> = by_call.into_values().collect();
    if rx.is_some() {
        out.sort_by(|a, b| b.km.unwrap_or(0.0).total_cmp(&a.km.unwrap_or(0.0)));
    } else {
        out.sort_by_key(|s| std::cmp::Reverse(s.t));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_centres() {
        let (lat, lon) = grid_to_latlon("CM87").unwrap();
        assert!((lat - 37.5).abs() < 1e-9 && (lon + 123.0).abs() < 1e-9);
        let (lat, lon) = grid_to_latlon("fn42").unwrap();
        assert!((lat - 42.5).abs() < 1e-9 && (lon + 71.0).abs() < 1e-9);
        let (lat, lon) = grid_to_latlon("CM87vx").unwrap();
        assert!((37.0..38.0).contains(&lat) && (-124.0..-122.0).contains(&lon));
        assert!(grid_to_latlon("ZZ99").is_none());
        assert!(grid_to_latlon("-15").is_none());
    }

    #[test]
    fn distance_and_bearing_bay_area_to_boston() {
        let sf = grid_to_latlon("CM87").unwrap();
        let bos = grid_to_latlon("FN42").unwrap();
        let km = distance_km(sf, bos);
        assert!((4200.0..4500.0).contains(&km), "{km}");
        let b = bearing_deg(sf, bos);
        assert!((55.0..75.0).contains(&b), "{b}");
    }

    #[test]
    fn one_spot_per_call_farthest_first() {
        let heard = vec![
            ("K1ABC".to_string(), "FN42".to_string(), Some(-15.0), 1_000),
            ("W6XYZ".to_string(), "CM97".to_string(), Some(-3.0), 2_000),
            ("K1ABC".to_string(), "FN42".to_string(), Some(-9.0), 3_000),
        ];
        let s = place(&heard, grid_to_latlon("CM87"));
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].call, "K1ABC");
        assert_eq!(s[0].count, 2);
        assert_eq!(s[0].snr, Some(-9.0));
        assert!(s[0].km.unwrap() > s[1].km.unwrap());
    }
}
