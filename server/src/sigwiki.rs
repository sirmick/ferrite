//! The Signal Identification Guide (sigidwiki.com) as a lookup service —
//! "what could this be?" for a frequency + bandwidth, search by name,
//! and a signal's page (description, waterfall picture, sample audio).
//!
//! sigidwiki is Semantic MediaWiki: every signal page carries typed
//! properties (`Frequency_min/max`, `Bandwidth_min/max`, `Mode`,
//! `Modulation`, `Picture`, `Signal_file`, …). The whole catalogue (~600
//! signals) is fetched once through the `ask` API, cached on disk for
//! [`INDEX_MAX_AGE`], and every query runs against that index locally.
//! Pictures are fetched on demand and cached too.
//!
//! - `GET /api/sigwiki/identify?freq_hz=&bw_hz=&mode=&modulation=&limit=`
//! - `GET /api/sigwiki/search?q=&limit=`
//! - `GET /api/sigwiki/page?title=`
//! - `GET /api/sigwiki/image?file=&width=` (the picture bytes, cached)
//!
//! `identify` and `page` also land as `analysis` records (`tool:
//! "sigwiki"`), so the AI panel shows the candidates / the page.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::app_state::AppState;
use crate::error::AppError;

const API: &str = "https://www.sigidwiki.com/api.php";
const WIKI: &str = "https://www.sigidwiki.com/wiki/";
const USER_AGENT: &str = "ferrite-sdr/0.9 (signal identification lookups; cached)";
/// Refetch the catalogue after this long.
const INDEX_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);
const PAGE: usize = 500;

/// One signal in the catalogue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Signal {
    pub title: String,
    pub url: String,
    /// Specific frequencies (Hz) the page lists — a range's two ends, or
    /// a set of channels.
    #[serde(default)]
    pub frequencies: Vec<f64>,
    pub freq_min: Option<f64>,
    pub freq_max: Option<f64>,
    pub bw_min: Option<f64>,
    pub bw_max: Option<f64>,
    #[serde(default)]
    pub mode: Vec<String>,
    #[serde(default)]
    pub modulation: Vec<String>,
    pub picture: Option<String>,
    pub audio: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    fetched_ms: u64,
    signals: Vec<Signal>,
}

static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where the index + pictures are cached (`<state-dir>/sigwiki`, else
/// the temp dir). Set once at startup.
pub fn set_cache_dir(dir: PathBuf) {
    let _ = CACHE_DIR.set(dir);
}

fn cache_dir() -> PathBuf {
    CACHE_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("ferrite-sigwiki"))
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default()
    })
}

fn index_slot() -> &'static Mutex<Option<std::sync::Arc<Vec<Signal>>>> {
    static I: OnceLock<Mutex<Option<std::sync::Arc<Vec<Signal>>>>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(None))
}

fn now_ms() -> u64 {
    u64::try_from(crate::capture::now_ms()).unwrap_or(0)
}

/// The catalogue: memory, else a fresh-enough disk cache, else fetched.
/// A stale cache still serves when the fetch fails (offline).
async fn index() -> Result<std::sync::Arc<Vec<Signal>>, AppError> {
    let mut slot = index_slot().lock().await;
    if let Some(i) = slot.as_ref() {
        return Ok(i.clone());
    }
    let path = cache_dir().join("index.json");
    let cached: Option<IndexFile> = tokio::fs::read(&path)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let fresh = cached.as_ref().is_some_and(|c| {
        now_ms().saturating_sub(c.fetched_ms)
            < u64::try_from(INDEX_MAX_AGE.as_millis()).unwrap_or(u64::MAX)
    });
    let signals = if fresh {
        cached.map(|c| c.signals).unwrap_or_default()
    } else {
        match fetch_index().await {
            Ok(signals) => {
                let file = IndexFile {
                    fetched_ms: now_ms(),
                    signals,
                };
                let _ = tokio::fs::create_dir_all(cache_dir()).await;
                if let Ok(b) = serde_json::to_vec(&file) {
                    let _ = tokio::fs::write(&path, b).await;
                }
                tracing::info!(n = file.signals.len(), "sigwiki: catalogue fetched");
                file.signals
            }
            Err(e) => match cached {
                Some(c) => {
                    tracing::warn!(error = %e, "sigwiki: refresh failed; serving the stale cache");
                    c.signals
                }
                None => {
                    return Err(AppError::unavailable(
                        "SIGWIKI_UNREACHABLE",
                        format!("sigidwiki.com unreachable and no cached catalogue: {e}"),
                    ))
                }
            },
        }
    };
    let arc = std::sync::Arc::new(signals);
    *slot = Some(arc.clone());
    Ok(arc)
}

async fn fetch_index() -> anyhow::Result<Vec<Signal>> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    loop {
        let query = format!(
            "[[Category:Signal]]|?Frequencies|?Frequency_min|?Frequency_max|?Bandwidth_min|?Bandwidth_max\
             |?Mode|?Modulation|?Picture|?Signal_file|?Signal_description|?Location|?Additional_categories\
             |limit={PAGE}|offset={offset}"
        );
        let v: Value = client()
            .get(API)
            .query(&[("action", "ask"), ("format", "json"), ("query", &query)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let results = v
            .pointer("/query/results")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        out.extend(results.iter().map(|(title, r)| parse_signal(title, r)));
        match v.get("query-continue-offset").and_then(Value::as_u64) {
            Some(next) if !results.is_empty() => offset = usize::try_from(next)?,
            _ => break,
        }
        if offset > 20_000 {
            break;
        }
    }
    Ok(out)
}

fn parse_signal(title: &str, r: &Value) -> Signal {
    let p = r.get("printouts").cloned().unwrap_or(Value::Null);
    let nums = |k: &str| -> Vec<f64> {
        p.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_f64().or_else(|| x.as_str()?.parse().ok()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let strs = |k: &str| -> Vec<String> {
        p.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        x.as_str()
                            .map(str::to_string)
                            .or_else(|| x.get("fulltext")?.as_str().map(str::to_string))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let first = |k: &str| strs(k).into_iter().next();
    let file = |k: &str| first(k).map(|f| f.trim_start_matches("File:").to_string());
    Signal {
        title: title.to_string(),
        url: r.get("fullurl").and_then(Value::as_str).map_or_else(
            || format!("{WIKI}{}", title.replace(' ', "_")),
            str::to_string,
        ),
        frequencies: nums("Frequencies"),
        freq_min: nums("Frequency min").first().copied(),
        freq_max: nums("Frequency max").first().copied(),
        bw_min: nums("Bandwidth min").first().copied(),
        bw_max: nums("Bandwidth max").first().copied(),
        mode: strs("Mode"),
        modulation: strs("Modulation"),
        picture: file("Picture"),
        audio: file("Signal file"),
        description: first("Signal description"),
        location: first("Location"),
        categories: strs("Additional categories")
            .into_iter()
            .map(|c| c.trim_start_matches("Category:").to_string())
            .collect(),
    }
}

/// Direct URLs for wiki files (pictures scaled to `width`), via the
/// `imageinfo` API — the wiki's `Special:FilePath` redirect refuses
/// non-browser clients, and its images refuse a foreign `Referer`, so
/// the panel loads pictures through `/api/sigwiki/image` (audio plays
/// direct). One request for up to 50 files; best effort
/// (missing entries just have no URL).
async fn file_urls(files: &[String], width: u32) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for chunk in files.chunks(50) {
        let titles = chunk
            .iter()
            .map(|f| format!("File:{f}"))
            .collect::<Vec<_>>()
            .join("|");
        let w = width.to_string();
        let resp = client()
            .get(API)
            .query(&[
                ("action", "query"),
                ("format", "json"),
                ("prop", "imageinfo"),
                ("iiprop", "url|mime"),
                ("iiurlwidth", w.as_str()),
                ("titles", titles.as_str()),
            ])
            .send()
            .await;
        let Ok(v) = (match resp {
            Ok(r) => r.json::<Value>().await,
            Err(e) => Err(e),
        }) else {
            continue;
        };
        // Titles come back normalised (`_` → space); key by the bare name.
        for page in v
            .pointer("/query/pages")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.values())
        {
            let Some(title) = page.get("title").and_then(Value::as_str) else {
                continue;
            };
            let Some(info) = page.pointer("/imageinfo/0") else {
                continue;
            };
            let mime = info.get("mime").and_then(Value::as_str).unwrap_or("");
            // Non-images (audio) get a file *icon* as their "thumbnail".
            let thumb = if mime.starts_with("image/") {
                info.get("thumburl")
            } else {
                None
            };
            let url = thumb.or_else(|| info.get("url")).and_then(Value::as_str);
            if let Some(url) = url {
                out.insert(
                    file_key(title.trim_start_matches("File:")),
                    (url.to_string(), mime.to_string()),
                );
            }
        }
    }
    out
}

/// Wiki file names compare with `_` and ` ` as the same character.
fn file_key(f: &str) -> String {
    f.trim().replace(' ', "_")
}

/// A candidate for "what's at this frequency?" and why it ranked.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    #[serde(flatten)]
    pub signal: Signal,
    pub score: f64,
    pub why: Vec<String>,
}

/// Rank the catalogue against an observation. Frequency is the gate (in
/// the page's range, or within a channel's width of a listed frequency
/// — digital modes sit a few kHz above their dial); a narrow range, a
/// matching bandwidth and — strongest — a matching mode / modulation
/// raise the score; a bandwidth far off, or a stated mode the page
/// doesn't list, lowers it. Pages like CW span 3 kHz–250 GHz, so
/// frequency alone can't favour them: pass what you observed.
#[must_use]
pub fn identify(
    all: &[Signal],
    freq_hz: f64,
    bw_hz: Option<f64>,
    mode: Option<&str>,
    modulation: Option<&str>,
    ham_band: bool,
) -> Vec<Candidate> {
    let lc = |s: &str| s.to_ascii_lowercase();
    let mut out: Vec<Candidate> = Vec::new();
    for s in all {
        let mut why = Vec::new();
        let tol = s.bw_max.unwrap_or(0.0).max(3_000.0);
        let near_channel = s.frequencies.iter().any(|f| (f - freq_hz).abs() <= tol);
        let in_range = match (s.freq_min, s.freq_max) {
            (Some(lo), Some(hi)) if hi >= lo => freq_hz >= lo && freq_hz <= hi,
            _ => false,
        };
        if !in_range && !near_channel {
            continue;
        }
        // Specificity: a page spanning 3–30 MHz says less than one at
        // 7.0–7.1 MHz. log-scaled so a 10× narrower range is +1.
        let span = match (s.freq_min, s.freq_max) {
            (Some(lo), Some(hi)) if hi > lo => hi - lo,
            _ => 1_000.0,
        };
        let mut score = 1.0 + ((1e9 / span.max(1_000.0)).log10() * 0.4).clamp(0.0, 3.0);
        if near_channel {
            score += 2.0;
            why.push("on a listed frequency".into());
        } else {
            why.push("in its frequency range".into());
        }
        if let Some(bw) = bw_hz.filter(|b| *b > 0.0) {
            let (lo, hi) = (
                s.bw_min.or(s.bw_max).unwrap_or(0.0),
                s.bw_max.or(s.bw_min).unwrap_or(0.0),
            );
            if hi > 0.0 {
                if bw >= lo * 0.5 && bw <= hi * 2.0 {
                    score += 1.5;
                    why.push("bandwidth fits".into());
                } else {
                    let off = if bw < lo { lo / bw } else { bw / hi };
                    score -= off.log10().clamp(0.0, 3.0);
                    why.push("bandwidth off".into());
                }
            }
        }
        if let Some(m) = mode.filter(|m| !m.is_empty()) {
            // How the observer received it (USB) vs how the page files it
            // (SSB) is the same evidence.
            if s.mode.iter().any(|x| mode_family(x) == mode_family(m)) {
                score += 2.5;
                why.push(format!("mode {m}"));
            } else if !s.mode.is_empty() {
                score -= 1.0;
            }
        }
        if let Some(m) = modulation.filter(|m| !m.is_empty()) {
            if s.modulation.iter().any(|x| lc(x).contains(&lc(m))) {
                score += 2.0;
                why.push(format!("modulation {m}"));
            } else if !s.modulation.is_empty() {
                score -= 0.5;
            }
        }
        if s.categories.iter().any(|c| c == "Active") {
            score += 0.3;
        }
        if ham_band && s.categories.iter().any(|c| c == "Amateur Radio") {
            score += 1.5;
            why.push("amateur band".into());
        }
        out.push(Candidate {
            signal: s.clone(),
            score,
            why,
        });
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// Name/description search: every term must appear somewhere in the
/// title, description, mode, modulation, location or categories; title
/// hits rank first.
#[must_use]
pub fn search<'a>(all: &'a [Signal], q: &str) -> Vec<&'a Signal> {
    let terms: Vec<String> = q.split_whitespace().map(str::to_ascii_lowercase).collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u32, &Signal)> = all
        .iter()
        .filter_map(|s| {
            let title = s.title.to_ascii_lowercase();
            let hay = format!(
                "{title} {} {} {} {} {}",
                s.description.as_deref().unwrap_or(""),
                s.mode.join(" "),
                s.modulation.join(" "),
                s.location.as_deref().unwrap_or(""),
                s.categories.join(" ")
            )
            .to_ascii_lowercase();
            terms.iter().all(|t| hay.contains(t)).then(|| {
                let rank = u32::from(title == q.to_ascii_lowercase()) * 4
                    + u32::try_from(terms.iter().filter(|t| title.contains(t.as_str())).count())
                        .unwrap_or(0);
                (rank, s)
            })
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.cmp(&b.1.title)));
    hits.into_iter().map(|(_, s)| s).collect()
}

/// Receive modes that are the same thing to an observer: a USB
/// signal matches a page filed as SSB; NFM matches FM.
fn mode_family(m: &str) -> String {
    match m.to_ascii_lowercase().as_str() {
        "usb" | "lsb" | "ssb" => "ssb".into(),
        "fm" | "nfm" | "wfm" => "fm".into(),
        "cw" | "ook" => "cw".into(),
        other => other.into(),
    }
}

/// Band-plan rows for amateur allocations: `20m`, `70cm`, `40m Ham Band`.
fn is_ham_band(name: &str) -> bool {
    let n = name.trim();
    n.contains("Ham") || n.contains("Amateur") || {
        let head = n.split_whitespace().next().unwrap_or("");
        let digits = head.trim_end_matches("cm").trim_end_matches('m');
        head.ends_with('m')
            && !digits.is_empty()
            && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
    }
}

fn mhz_range(s: &Signal) -> String {
    match (s.freq_min, s.freq_max) {
        (Some(lo), Some(hi)) if (hi - lo).abs() > 1.0 => {
            format!("{:.4}–{:.4} MHz", lo / 1e6, hi / 1e6)
        }
        (Some(f), _) | (_, Some(f)) => format!("{:.4} MHz", f / 1e6),
        _ => "?".into(),
    }
}

fn bw_text(s: &Signal) -> String {
    let f = |b: f64| {
        if b >= 1e6 {
            format!("{:.1} MHz", b / 1e6)
        } else if b >= 1e3 {
            format!("{:.1} kHz", b / 1e3)
        } else {
            format!("{b:.0} Hz")
        }
    };
    match (s.bw_min, s.bw_max) {
        (Some(lo), Some(hi)) if (hi - lo).abs() > 0.5 => format!("{}–{}", f(lo), f(hi)),
        (Some(b), _) | (_, Some(b)) => f(b),
        _ => "?".into(),
    }
}

fn line(s: &Signal) -> String {
    format!(
        "{} — {} · bw {} · {} {}",
        s.title,
        mhz_range(s),
        bw_text(s),
        s.mode.join("/"),
        s.modulation.join("/")
    )
}

fn caller(headers: &HeaderMap) -> String {
    headers
        .get("x-ferrite-via")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("ui")
        .to_string()
}

fn publish(state: &AppState, by: &str, body: &Value) -> String {
    let id = format!("sw-{}", now_ms());
    let mut rec = json!({ "id": id, "tool": "sigwiki", "by": by });
    if let (Some(r), Some(b)) = (rec.as_object_mut(), body.as_object()) {
        r.extend(b.clone());
    }
    state.decoder_store().apply("analysis", rec);
    id
}

#[derive(Debug, Deserialize)]
pub struct IdentifyQ {
    pub freq_hz: f64,
    #[serde(default)]
    pub bw_hz: Option<f64>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub modulation: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/sigwiki/identify`.
pub async fn get_identify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<IdentifyQ>,
) -> Result<Json<Value>, AppError> {
    let all = index().await?;
    let limit = q.limit.unwrap_or(10).clamp(1, 50);
    // The band plan is context: inside a ham band, amateur modes are the
    // likely answer.
    let bands = crate::band_plan::at(q.freq_hz);
    let ham_band = bands.iter().any(|b| is_ham_band(&b.name));
    let c = identify(
        &all,
        q.freq_hz,
        q.bw_hz,
        q.mode.as_deref(),
        q.modulation.as_deref(),
        ham_band,
    );
    let total = c.len();
    let top: Vec<Candidate> = c.into_iter().take(limit).collect();
    let mut text = format!(
        "{total} sigidwiki signals cover {:.4} MHz{}; best {}:\n",
        q.freq_hz / 1e6,
        q.bw_hz
            .map(|b| format!(" (bw {b:.0} Hz)"))
            .unwrap_or_default(),
        top.len()
    );
    for c in &top {
        text.push_str(&format!("{:>5.1}  {}\n", c.score, line(&c.signal)));
    }
    let body = json!({
        "params": { "freq_hz": q.freq_hz, "bw_hz": q.bw_hz, "mode": q.mode, "modulation": q.modulation },
        "bands": bands.iter().map(|b| b.name.trim_end_matches(['|', ' '])).collect::<Vec<_>>(),
        "result": { "total": total, "candidates": top },
        "text": text,
    });
    let id = publish(&state, &caller(&headers), &body);
    let mut out = body;
    out["analysis_id"] = json!(id);
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct SearchQ {
    pub q: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/sigwiki/search` — not drawn (a list of names); the agent
/// follows up with `page`.
pub async fn get_search(Query(q): Query<SearchQ>) -> Result<Json<Value>, AppError> {
    let all = index().await?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let hits = search(&all, &q.q);
    let total = hits.len();
    let rows: Vec<Value> = hits
        .into_iter()
        .take(limit)
        .map(|s| {
            json!({
                "title": s.title, "url": s.url, "freq": mhz_range(s), "bw": bw_text(s),
                "mode": s.mode, "modulation": s.modulation,
                "description": s.description.as_deref().map(|d| d.chars().take(160).collect::<String>()),
            })
        })
        .collect();
    Ok(Json(
        json!({ "query": q.q, "total": total, "results": rows }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct PageQ {
    pub title: String,
}

/// `GET /api/sigwiki/page` — one signal's entry (case-insensitive title
/// match; else the best search hit).
pub async fn get_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PageQ>,
) -> Result<Json<Value>, AppError> {
    let all = index().await?;
    let want = q.title.trim().replace('_', " ").to_ascii_lowercase();
    let s = all
        .iter()
        .find(|s| s.title.to_ascii_lowercase() == want)
        .or_else(|| search(&all, &want).into_iter().next())
        .ok_or_else(|| {
            AppError::not_found(
                "SIGWIKI_NO_PAGE",
                format!("no sigidwiki signal matches {:?}", q.title),
            )
        })?
        .clone();
    let text = format!(
        "{}\n{}\n{}\nlocation: {}\npicture: {}\naudio: {}\n{}",
        line(&s),
        s.url,
        s.description.as_deref().unwrap_or(""),
        s.location.as_deref().unwrap_or("?"),
        s.picture.as_deref().unwrap_or("-"),
        s.audio.as_deref().unwrap_or("-"),
        if s.categories.is_empty() {
            String::new()
        } else {
            format!("categories: {}", s.categories.join(", "))
        }
    );
    let files: Vec<String> = s.picture.iter().chain(s.audio.iter()).cloned().collect();
    let urls = file_urls(&files, 640).await;
    let url_of = |f: &Option<String>| {
        f.as_deref()
            .and_then(|f| urls.get(&file_key(f)))
            .map(|(u, _)| u.clone())
    };
    let body = json!({
        "params": { "title": s.title },
        "result": { "signal": s, "picture_url": url_of(&s.picture), "audio_url": url_of(&s.audio) },
        "text": text,
    });
    let id = publish(&state, &caller(&headers), &body);
    let mut out = body;
    out["analysis_id"] = json!(id);
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct ImageQ {
    pub file: String,
    #[serde(default)]
    pub width: Option<u32>,
}

/// `GET /api/sigwiki/image` — a wiki file (picture), scaled by the wiki
/// to `width` (default 640), cached on disk.
pub async fn get_image(Query(q): Query<ImageQ>) -> Result<axum::response::Response, AppError> {
    let file = q.file.trim().trim_start_matches("File:").replace(' ', "_");
    if file.is_empty() || file.contains('/') || file.contains("..") {
        return Err(AppError::bad_request(
            "BAD_FILE",
            "file must be a bare wiki file name",
        ));
    }
    let width = q.width.unwrap_or(640).clamp(64, 2048);
    let dir = cache_dir().join("images");
    let path = dir.join(format!("{width}-{file}"));
    let mime = |name: &str| match name
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => "image/jpeg",
    };
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return Ok(([(http::header::CONTENT_TYPE, mime(&file))], bytes).into_response());
    }
    let urls = file_urls(std::slice::from_ref(&file), width).await;
    let (url, wiki_mime) = urls
        .get(&file_key(&file))
        .cloned()
        .ok_or_else(|| AppError::not_found("SIGWIKI_NO_FILE", format!("no wiki file {file:?}")))?;
    let resp = client()
        .get(&url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| AppError::unavailable("SIGWIKI_UNREACHABLE", format!("{url}: {e}")))?;
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .filter(|c| c.starts_with("image/"))
        .unwrap_or_else(|| {
            if wiki_mime.is_empty() {
                mime(&file).to_string()
            } else {
                wiki_mime
            }
        });
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AppError::unavailable("SIGWIKI_UNREACHABLE", format!("{url}: {e}")))?;
    let _ = tokio::fs::create_dir_all(&dir).await;
    let _ = tokio::fs::write(&path, &bytes).await;
    Ok(([(http::header::CONTENT_TYPE, ct)], bytes.to_vec()).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(title: &str, lo: f64, hi: f64, bw: (f64, f64), mode: &str, modulation: &str) -> Signal {
        Signal {
            title: title.into(),
            url: String::new(),
            frequencies: vec![lo, hi],
            freq_min: Some(lo),
            freq_max: Some(hi),
            bw_min: Some(bw.0),
            bw_max: Some(bw.1),
            mode: vec![mode.into()],
            modulation: vec![modulation.into()],
            picture: None,
            audio: None,
            description: Some(format!("{title} test signal")),
            location: Some("Worldwide".into()),
            categories: vec!["Active".into()],
        }
    }

    fn corpus() -> Vec<Signal> {
        vec![
            sig("Olivia", 3e6, 30e6, (125.0, 2000.0), "USB", "MFSK"),
            sig("FT8", 1.84e6, 50.313e6, (50.0, 50.0), "USB", "8-FSK"),
            sig("OTH Radar", 6e6, 30e6, (40_000.0, 80_000.0), "USB", "FMCW"),
            sig("Morse Code (CW)", 0.1e6, 30e6, (10.0, 150.0), "CW", "OOK"),
            sig(
                "FM Broadcast",
                87.5e6,
                108e6,
                (200_000.0, 200_000.0),
                "WFM",
                "FM",
            ),
        ]
    }

    #[test]
    fn frequency_gates_and_bandwidth_ranks() {
        let all = corpus();
        let c = identify(&all, 7.03e6, Some(60.0), None, None, false);
        assert!(c.iter().all(|c| c.signal.title != "FM Broadcast"));
        // A 60 Hz-wide signal at 7.03 MHz: CW / FT8 fit; OTH radar doesn't.
        let pos = |t: &str| c.iter().position(|c| c.signal.title == t).unwrap();
        assert!(pos("Morse Code (CW)") < pos("OTH Radar"));
        assert!(pos("FT8") < pos("OTH Radar"));
    }

    #[test]
    fn mode_breaks_ties() {
        let all = corpus();
        let c = identify(&all, 7.03e6, Some(60.0), Some("CW"), None, false);
        assert_eq!(c[0].signal.title, "Morse Code (CW)");
    }

    #[test]
    fn listed_channel_matches_outside_the_range_ends() {
        let mut s = sig("WWV", 2.5e6, 20e6, (10_000.0, 10_000.0), "AM", "AM");
        s.frequencies = vec![2.5e6, 5e6, 10e6, 15e6, 20e6];
        let c = identify(&[s], 10.0005e6, None, None, None, false);
        assert_eq!(c.len(), 1);
        assert!(c[0].why.iter().any(|w| w.contains("listed")));
    }

    #[test]
    fn mode_families_match_loosely() {
        let mut ssb = sig(
            "Single Sideband Voice",
            3e6,
            30e6,
            (1900.0, 1900.0),
            "SSB",
            "AM",
        );
        ssb.categories.push("Amateur Radio".into());
        let other = sig("Some Modem", 14.2e6, 14.3e6, (2500.0, 2500.0), "FM", "FSK");
        let c = identify(
            &[ssb, other],
            14.25e6,
            Some(2700.0),
            Some("USB"),
            None,
            true,
        );
        assert_eq!(c[0].signal.title, "Single Sideband Voice");
    }

    #[test]
    fn ham_band_names() {
        for n in ["20m", "70cm", "40m Ham Band|  ", "2.2m"] {
            assert!(is_ham_band(n), "{n}");
        }
        for n in ["WWV NIST", "NOAA Radio", "Maritime"] {
            assert!(!is_ham_band(n), "{n}");
        }
    }

    #[test]
    fn search_needs_every_term_and_ranks_titles() {
        let all = corpus();
        assert_eq!(search(&all, "olivia")[0].title, "Olivia");
        assert!(search(&all, "radar usb")[0].title.contains("OTH"));
        assert!(search(&all, "nonexistent").is_empty());
    }

    #[test]
    fn parses_an_ask_result() {
        let r = json!({
            "fullurl": "https://www.sigidwiki.com/wiki/Olivia",
            "printouts": {
                "Frequencies": [3000000, 30000000],
                "Frequency min": [3000000], "Frequency max": [30000000],
                "Bandwidth min": [125], "Bandwidth max": [2000],
                "Mode": ["USB"], "Modulation": ["MFSK"],
                "Picture": [{"fulltext": "File:Olivia 8-250y.png"}],
                "Signal file": ["Olivia_16-500.mp3"],
                "Signal description": ["OLIVIA is an amateur digital teletype mode."],
                "Location": ["Worldwide"],
                "Additional categories": [{"fulltext": "Category:HF"}, "Active"]
            }
        });
        let s = parse_signal("Olivia", &r);
        assert_eq!(s.freq_min, Some(3e6));
        assert_eq!(s.picture.as_deref(), Some("Olivia 8-250y.png"));
        assert_eq!(s.audio.as_deref(), Some("Olivia_16-500.mp3"));
        assert_eq!(s.categories, vec!["HF", "Active"]);
    }
}
