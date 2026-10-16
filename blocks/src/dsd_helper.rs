//! `DsdHelper` — digital voice (DMR / P25 / NXDN / D-STAR / YSF / dPMR)
//! via the operator-installed `dsd-fme` helper binary.
//!
//! Ferrite does not ship an AMBE vocoder (patents), so digital voice is
//! the *native-helper* track of the decoder roadmap
//! (`docs/decoder-roadmap/06-phase-6-digital-voice.md`): the operator
//! installs `dsd-fme` (+ mbelib) themselves and this block drives it as
//! a subprocess —
//!
//! ```text
//! FmDemod (48 kHz discriminator) ─► in ─┐
//!                                        │ TCP 127.0.0.1:<a>  s16le 48 kHz   (dsd-fme -i tcp:…)
//!                                        ▼
//!                                     dsd-fme
//!                                        │ UDP 127.0.0.1:<b>  s16le 8 kHz    (dsd-fme -o udp:…)
//!                                        ▼
//!                               out (8 kHz voice) ─► RealF32Resamp ─► AudioSink
//!                               events (`ui:dsd`)  ◄─ stderr call / sync lines
//! ```
//!
//! dsd-fme's TCP input is the SDR++ "network sink" convention (raw
//! s16le mono at its `-s` rate, 48 kHz default) and connects *to* us,
//! so the block listens on an ephemeral loopback port and passes it on
//! the command line. Its UDP "socket blaster" output is 8 kHz s16le, a
//! 20 ms frame per datagram, stereo (slot 1 left / slot 2 right) in the
//! TDMA modes and mono elsewhere; both are accepted and summed to mono.
//! Call metadata is what dsd-fme prints to its terminal (stderr, ANSI
//! coloured): `Sync: +DMR BS` lines name the protocol, the per-frame
//! lines carry `SLOT n`, `TGT=`/`SRC=` (DMR), `Group n Source n` /
//! `TG: n` (P25), `CC=`; see [`parse_line`].
//!
//! **Clocking.** The block is a pure function of its input clock: every
//! call produces `consumed × 8000 / in_rate` output samples, filled from
//! the voice ring when dsd-fme has synthesised speech and zeros
//! otherwise — so silence, a missing helper, or an exited helper all
//! leave the audio chain running (quiet) instead of stalling it, per
//! the block-progress rule. Audio is never fed back; this is not a tap.
//!
//! **Placement::NativeOnly** — a subprocess has no browser analogue.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    drain_events_port, Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind,
    ParamSpec, Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// What dsd-fme's TCP input expects (its `-s` default).
pub const HELPER_IN_RATE_HZ: f64 = 48_000.0;
/// What dsd-fme's synthesised speech comes back at.
pub const HELPER_OUT_RATE_HZ: f64 = 8_000.0;
/// Cap on owed output samples: one second at 8 kHz.
const MAX_OWED_SAMPLES: f64 = 8_000.0;
/// Binary name when `helper_path` is empty and `FERRITE_DSD_FME` is unset.
pub const DEFAULT_HELPER: &str = "dsd-fme";

/// `mode` values → dsd-fme `-f` flags.
pub const MODES: &[&str] = &[
    "auto", "dmr", "p25", "p25p2", "nxdn48", "nxdn96", "dstar", "ysf", "dpmr", "x2tdma", "trunk",
];

/// dsd-fme's `-f` argument for a `mode`.
pub fn mode_flag(mode: &str) -> Result<&'static str> {
    Ok(match mode {
        "auto" => "-fa",
        "dmr" => "-fs",
        "p25" => "-f1",
        "p25p2" => "-f2",
        "nxdn48" => "-fi",
        "nxdn96" => "-fn",
        "dstar" => "-fd",
        "ysf" => "-fy",
        "dpmr" => "-fm",
        "x2tdma" => "-fx",
        "trunk" => "-ft",
        other => bail!("DsdHelper: mode must be one of {MODES:?}, got {other:?}"),
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DsdHelperParams {
    /// Decoder selection, see [`MODES`] (`auto` = dsd-fme's `-fa`).
    pub mode: String,
    /// Path to the helper. Empty → `$FERRITE_DSD_FME`, else `dsd-fme`
    /// on `PATH`.
    pub helper_path: String,
    /// Extra command-line arguments appended verbatim (whitespace
    /// separated), e.g. `-V 1` to hear slot 1 only.
    pub extra_args: String,
    /// Emit dsd-fme's call / alias / sync records. Off when a native
    /// metadata decoder (`DmrDecode`) shares the `ui:dsd` kind and this
    /// block is only the voice plug-in; `helper` / `voice` records are
    /// always emitted.
    pub metadata: bool,
}

impl Default for DsdHelperParams {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            helper_path: String::new(),
            extra_args: String::new(),
            metadata: true,
        }
    }
}

/// One parsed dsd-fme terminal line, serialised as the `events` record
/// `{"t":"dsd","event":"call"|"sync"|"helper", …}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DsdEvent {
    pub t: &'static str,
    pub event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tgt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enc: Option<bool>,
    pub text: String,
}

/// Drop ANSI SGR / cursor sequences (`ESC [ … <letter>`).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The value after `key` (`TGT=1234`, `TGT: 1234;`, `Group 1234`): the
/// token up to the next separator. Every occurrence of `key` is tried,
/// so `Group Voice Channel User - Group 1201` yields `1201`, not
/// `Voice`; with `numeric` only an all-digit token counts.
fn value_after(line: &str, key: &str, numeric: bool) -> Option<String> {
    for (idx, _) in line.match_indices(key) {
        let rest = line[idx + key.len()..].trim_start_matches([' ', '=', ':']);
        let tok: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if tok.is_empty() {
            continue;
        }
        if numeric && !tok.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        return Some(tok);
    }
    None
}

/// What the parser carries between terminal lines: dsd-fme prints the
/// protocol, colour code and active slot on its per-frame `Sync:` line
/// and the IDs on separate call lines.
#[derive(Debug, Default, Clone)]
pub struct ParseState {
    pub protocol: Option<String>,
    pub cc: Option<u16>,
}

/// Parse one (ANSI-stripped) terminal line. Lines carry a leading
/// `HH:MM:SS` timestamp, so markers are searched anywhere in the line.
/// Real formats (dsd-fme, 441.850 MHz DMR, 2026-10-13):
///
/// ```text
/// 20:58:01 Sync: +DMR   slot1  [SLOT2] | Color Code=01 | VC3
///  SLOT 2 TGT=31068 SRC=3214431 Group Call
///  Slot 1 - Talker Alias Block Num: 1; Valid Block; Talker Alias: KR1JOE
/// ```
///
/// Returns `None` for per-frame noise; a `sync` event only when the
/// protocol changes.
pub fn parse_line(line: &str, st: &mut ParseState) -> Option<DsdEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let ev = |event: &'static str, st: &ParseState| DsdEvent {
        t: "dsd",
        event,
        protocol: st.protocol.clone(),
        slot: None,
        tgt: None,
        src: None,
        cc: st.cc,
        enc: None,
        text: line.to_string(),
    };
    if let Some(idx) = line.find("Sync:") {
        let rest = line[idx + "Sync:".len()..].trim();
        if rest.to_ascii_lowercase().starts_with("no sync") {
            return None;
        }
        let head = rest.split('|').next().unwrap_or(rest);
        let proto = head
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches(['+', '-'])
            .to_string();
        if let Some(cc) = value_after(rest, "Color Code", true).and_then(|v| v.parse().ok()) {
            st.cc = Some(cc);
        }
        if proto.is_empty() {
            return None;
        }
        let changed = st.protocol.as_deref() != Some(proto.as_str());
        st.protocol = Some(proto.clone());
        return changed.then(|| DsdEvent {
            protocol: Some(proto),
            text: head.trim().to_string(),
            ..ev("sync", st)
        });
    }
    if let Some(idx) = line.find("Talker Alias:") {
        let alias = line[idx + "Talker Alias:".len()..].trim();
        if alias.is_empty() {
            return None;
        }
        let slot = value_after(line, "Slot", true).and_then(|s| s.parse::<u8>().ok());
        return Some(DsdEvent {
            slot,
            text: alias.to_string(),
            ..ev("alias", st)
        });
    }
    let tgt = value_after(line, "TGT", false)
        .or_else(|| value_after(line, "TG:", false))
        .or_else(|| value_after(line, "Group ", true))
        .or_else(|| value_after(line, "Target", true));
    let src = value_after(line, "SRC", false)
        .or_else(|| value_after(line, "Source ", true))
        .or_else(|| value_after(line, "RID", true));
    if tgt.is_none() && src.is_none() {
        return None;
    }
    // Activity-update / channel-grant summaries name TGs but are not
    // calls on this channel.
    if line.contains("Activity Update") || line.contains("Channel Update") {
        return None;
    }
    let slot = value_after(line, "SLOT", true).and_then(|s| s.parse::<u8>().ok());
    if let Some(cc) = value_after(line, "CC", true).and_then(|s| s.parse().ok()) {
        st.cc = Some(cc);
    }
    let upper = line.to_ascii_uppercase();
    let enc = (upper.contains(" ENC") || upper.contains("ENCRYPT") || upper.contains("PRIVACY"))
        .then_some(true);
    Some(DsdEvent {
        slot,
        tgt,
        src,
        enc,
        ..ev("call", st)
    })
}

/// A dsd-fme UDP datagram → mono f32 at 8 kHz. 20 ms frames: 1280 B =
/// 320 stereo s16 frames (TDMA slots L/R, summed), anything else =
/// mono s16.
pub fn unpack_voice(datagram: &[u8], out: &mut Vec<f32>) {
    const S: f32 = 1.0 / 32768.0;
    let stereo = datagram.len() == 1280 || datagram.len() == 640;
    let samples = datagram
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes(*b)) * S);
    if stereo {
        let mut it = samples;
        while let (Some(l), Some(r)) = (it.next(), it.next()) {
            out.push((l + r).clamp(-1.0, 1.0));
        }
    } else {
        out.extend(samples);
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use crate::spsc_ring::SpscRing;

    use super::{parse_line, strip_ansi, unpack_voice, DsdEvent};
    use std::time::Instant;

    /// Discriminator audio queued for the helper: ~1.4 s at 48 kHz
    /// (`SpscRing` needs a power of two).
    pub(super) const TAP_CAPACITY: usize = 65_536;
    /// Synthesised voice waiting for the output clock: 2 s at 8 kHz.
    const VOICE_CAPACITY: usize = 16_000;

    pub struct Worker {
        pub tap: Arc<Mutex<SpscRing<f32>>>,
        pub voice: Arc<Mutex<VecDeque<f32>>>,
        pub events: Arc<Mutex<Vec<String>>>,
        pub dropped: Arc<AtomicU64>,
        pub alive: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        handles: Vec<JoinHandle<()>>,
        child: Arc<Mutex<Option<Child>>>,
    }

    /// Resolve the helper: explicit path, else `$FERRITE_DSD_FME`, else
    /// the bare name (PATH lookup by `Command`).
    pub fn resolve_helper(explicit: &str) -> PathBuf {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
        if let Ok(p) = std::env::var("FERRITE_DSD_FME") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        // Search PATH ourselves so the result is absolute and
        // `sibling_lib` can find `<prefix>/lib` (libmbe) for a
        // `~/.local/bin/dsd-fme` install; fall back to the bare name.
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let cand = dir.join(super::DEFAULT_HELPER);
                if cand.is_file() {
                    return cand;
                }
            }
        }
        PathBuf::from(super::DEFAULT_HELPER)
    }

    /// `<prefix>/bin/dsd-fme` → `<prefix>/lib`, so a helper installed
    /// under `~/.local` finds its `libmbe.so` without the operator
    /// exporting `LD_LIBRARY_PATH` for the daemon.
    fn sibling_lib(helper: &Path) -> Option<PathBuf> {
        let bin = helper.parent()?;
        if bin.file_name()? != "bin" {
            return None;
        }
        let lib = bin.parent()?.join("lib");
        lib.is_dir().then_some(lib)
    }

    fn push_event(events: &Arc<Mutex<Vec<String>>>, ev: &DsdEvent) {
        if let Ok(s) = serde_json::to_string(ev) {
            let mut q = events.lock().unwrap();
            if q.len() < 512 {
                q.push(s);
            }
        }
    }

    fn helper_event(events: &Arc<Mutex<Vec<String>>>, text: String) {
        helper_event_kind(events, "helper", text);
    }

    fn helper_event_kind(events: &Arc<Mutex<Vec<String>>>, event: &'static str, text: String) {
        push_event(
            events,
            &DsdEvent {
                t: "dsd",
                event,
                protocol: None,
                slot: None,
                tgt: None,
                src: None,
                cc: None,
                enc: None,
                text,
            },
        );
    }

    impl Worker {
        pub fn spawn(helper: &Path, mode_flag: &str, extra_args: &str) -> Self {
            let tap = Arc::new(Mutex::new(SpscRing::new(TAP_CAPACITY)));
            let voice = Arc::new(Mutex::new(VecDeque::with_capacity(VOICE_CAPACITY)));
            let events = Arc::new(Mutex::new(Vec::new()));
            let dropped = Arc::new(AtomicU64::new(0));
            let alive = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let child = Arc::new(Mutex::new(None));
            let mut handles = Vec::new();

            // Loopback endpoints first, so the ports go on the command line.
            let listener = TcpListener::bind("127.0.0.1:0").ok();
            let udp = UdpSocket::bind("127.0.0.1:0").ok();
            let (Some(listener), Some(udp)) = (listener, udp) else {
                helper_event(&events, "could not bind loopback sockets".into());
                return Self {
                    tap,
                    voice,
                    events,
                    dropped,
                    alive,
                    stop,
                    handles,
                    child,
                };
            };
            let tcp_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
            let udp_port = udp.local_addr().map(|a| a.port()).unwrap_or(0);

            let mut cmd = Command::new(helper);
            cmd.arg("-i")
                .arg(format!("tcp:127.0.0.1:{tcp_port}"))
                .arg("-o")
                .arg(format!("udp:127.0.0.1:{udp_port}"))
                .arg(mode_flag)
                .args(extra_args.split_whitespace())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(lib) = sibling_lib(helper) {
                let mut v = lib.into_os_string();
                if let Ok(cur) = std::env::var("LD_LIBRARY_PATH") {
                    if !cur.is_empty() {
                        v.push(":");
                        v.push(cur);
                    }
                }
                cmd.env("LD_LIBRARY_PATH", v);
            }
            let cmdline = format!(
                "{} {}",
                helper.display(),
                cmd.get_args()
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            // ETXTBSY ("Text file busy") is transient: another thread that
            // forked while the helper binary was open for writing (an
            // installer, or a test writing a stand-in script) holds the fd
            // until its child execs. Retry briefly instead of giving up.
            let mut spawned = cmd.spawn();
            for _ in 0..20 {
                match &spawned {
                    Err(e) if e.raw_os_error() == Some(26) => {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                        spawned = cmd.spawn();
                    }
                    _ => break,
                }
            }
            let mut proc = match spawned {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(target: "decoder::dsd", error = %e, helper = %helper.display(), "dsd-fme helper failed to start; install dsd-fme (+ mbelib) and set FERRITE_DSD_FME or `helper_path`");
                    helper_event(
                        &events,
                        format!("helper failed to start: {e} ({})", helper.display()),
                    );
                    return Self {
                        tap,
                        voice,
                        events,
                        dropped,
                        alive,
                        stop,
                        handles,
                        child,
                    };
                }
            };
            tracing::info!(target: "decoder::dsd", cmd = %cmdline, "dsd-fme helper started");
            helper_event(&events, format!("started: {cmdline}"));
            alive.store(true, Ordering::Relaxed);

            // stderr: the terminal output → events. stdout: drained, logged at debug.
            if let Some(stderr) = proc.stderr.take() {
                let (events, stop) = (Arc::clone(&events), Arc::clone(&stop));
                handles.push(thread::spawn(move || {
                    let mut st = super::ParseState::default();
                    // dsd-fme repeats call / alias lines every frame and
                    // the two TDMA slots interleave: one record per
                    // change *per slot*.
                    let mut last: std::collections::HashMap<(&'static str, Option<u8>), String> =
                        std::collections::HashMap::new();
                    for line in BufReader::new(stderr).lines() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let Ok(line) = line else { break };
                        let clean = strip_ansi(&line);
                        tracing::debug!(target: "decoder::dsd::term", "{}", clean.trim_end());
                        if let Some(ev) = parse_line(&clean, &mut st) {
                            let key = format!("{:?}{:?}{:?}{}", ev.protocol, ev.tgt, ev.src, if ev.event == "alias" { ev.text.as_str() } else { "" });
                            if last.get(&(ev.event, ev.slot)) != Some(&key) {
                                last.insert((ev.event, ev.slot), key);
                                tracing::info!(target: "decoder::dsd", event = ev.event, "{}", ev.text);
                                push_event(&events, &ev);
                            }
                        }
                    }
                }));
            }
            if let Some(stdout) = proc.stdout.take() {
                handles.push(thread::spawn(move || {
                    for line in BufReader::new(stdout).lines() {
                        let Ok(line) = line else { break };
                        tracing::debug!(target: "decoder::dsd::term", "{}", strip_ansi(&line).trim_end());
                    }
                }));
            }
            *child.lock().unwrap() = Some(proc);

            // Audio out to the helper: accept its TCP connection, then stream s16le.
            {
                let (tap, stop, dropped, events, alive, child) = (
                    Arc::clone(&tap),
                    Arc::clone(&stop),
                    Arc::clone(&dropped),
                    Arc::clone(&events),
                    Arc::clone(&alive),
                    Arc::clone(&child),
                );
                let _ = listener.set_nonblocking(true);
                handles.push(thread::spawn(move || {
                    let mut conn: Option<TcpStream> = None;
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while conn.is_none() && !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((s, _)) => {
                                let _ = s.set_nodelay(true);
                                let _ = s.set_nonblocking(false);
                                conn = Some(s);
                            }
                            Err(_) if std::time::Instant::now() < deadline => {
                                thread::sleep(Duration::from_millis(20));
                            }
                            Err(_) => break,
                        }
                    }
                    let Some(mut sock) = conn else {
                        if !stop.load(Ordering::Relaxed) {
                            tracing::warn!(target: "decoder::dsd", "dsd-fme never connected to the audio socket");
                            helper_event(&events, "helper never connected to the audio socket".into());
                            alive.store(false, Ordering::Relaxed);
                        }
                        return;
                    };
                    tracing::info!(target: "decoder::dsd", "dsd-fme connected; streaming 48 kHz discriminator audio");
                    let mut scratch = vec![0.0_f32; 4_096];
                    let mut bytes = Vec::with_capacity(8_192);
                    while !stop.load(Ordering::Relaxed) {
                        let n = tap.lock().unwrap().read(&mut scratch);
                        if n == 0 {
                            // Exited? Report once; the block keeps clocking silence.
                            if let Some(c) = child.lock().unwrap().as_mut() {
                                if let Ok(Some(status)) = c.try_wait() {
                                    if alive.swap(false, Ordering::Relaxed) {
                                        tracing::warn!(target: "decoder::dsd", %status, "dsd-fme helper exited");
                                        helper_event(&events, format!("helper exited: {status}"));
                                    }
                                    return;
                                }
                            }
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        bytes.clear();
                        for &s in &scratch[..n] {
                            #[allow(clippy::cast_possible_truncation)]
                            let v = (s.clamp(-1.0, 1.0) * 32_767.0) as i16;
                            bytes.extend_from_slice(&v.to_le_bytes());
                        }
                        if sock.write_all(&bytes).is_err() {
                            if alive.swap(false, Ordering::Relaxed) {
                                tracing::warn!(target: "decoder::dsd", "dsd-fme audio socket closed");
                                helper_event(&events, "helper closed the audio socket".into());
                            }
                            return;
                        }
                        dropped.fetch_add(0, Ordering::Relaxed);
                    }
                }));
            }

            // Voice back from the helper.
            {
                let (voice, stop, events) =
                    (Arc::clone(&voice), Arc::clone(&stop), Arc::clone(&events));
                let _ = udp.set_read_timeout(Some(Duration::from_millis(50)));
                handles.push(thread::spawn(move || {
                    let mut buf = [0_u8; 4_096];
                    let mut pcm = Vec::with_capacity(1_024);
                    // One `voice` event per over: audio after ≥ 1.5 s quiet.
                    let mut last_rx: Option<Instant> = None;
                    let mut over_start: Option<Instant> = None;
                    while !stop.load(Ordering::Relaxed) {
                        if let (Some(t), Some(s0)) = (last_rx, over_start) {
                            if t.elapsed() > Duration::from_millis(1_500) {
                                let secs = t.duration_since(s0).as_secs_f32();
                                tracing::info!(target: "decoder::dsd", secs, "voice over ended");
                                helper_event_kind(
                                    &events,
                                    "voice",
                                    format!("voice audio ended ({secs:.1} s)"),
                                );
                                over_start = None;
                            }
                        }
                        match udp.recv(&mut buf) {
                            Ok(n) if n > 0 => {
                                let now = Instant::now();
                                if over_start.is_none() {
                                    over_start = Some(now);
                                    tracing::info!(target: "decoder::dsd", "voice over started");
                                    helper_event_kind(
                                        &events,
                                        "voice",
                                        "voice audio started".into(),
                                    );
                                }
                                last_rx = Some(now);
                                pcm.clear();
                                unpack_voice(&buf[..n], &mut pcm);
                                let mut q = voice.lock().unwrap();
                                for &s in &pcm {
                                    if q.len() >= VOICE_CAPACITY {
                                        q.pop_front();
                                    }
                                    q.push_back(s);
                                }
                            }
                            _ => {}
                        }
                    }
                }));
            }

            Self {
                tap,
                voice,
                events,
                dropped,
                alive,
                stop,
                handles,
                child,
            }
        }
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(mut c) = self.child.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            for h in self.handles.drain(..) {
                let _ = h.join();
            }
        }
    }
}

pub struct DsdHelper {
    params: DsdHelperParams,
    mode_flag: &'static str,
    input_rate_hz: f64,
    /// Output samples owed for input already consumed (fractional),
    /// capped at [`MAX_OWED_SAMPLES`] so an undrained output (no audio
    /// sink this split) never grows it without bound.
    carry: f64,
    events_out: Vec<u8>,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<native::Worker>,
    warned_off_rate: bool,
}

impl DsdHelper {
    pub fn new(params: DsdHelperParams) -> Result<Self> {
        let mode_flag = mode_flag(&params.mode)?;
        Ok(Self {
            params,
            mode_flag,
            input_rate_hz: HELPER_IN_RATE_HZ,
            carry: 0.0,
            events_out: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            worker: None,
            warned_off_rate: false,
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn ensure_worker(&mut self) {
        if self.worker.is_none() {
            let helper = native::resolve_helper(&self.params.helper_path);
            self.worker = Some(native::Worker::spawn(
                &helper,
                self.mode_flag,
                &self.params.extra_args,
            ));
        }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for DsdHelper {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "DsdHelper",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[
                PortSpec {
                    name: "out",
                    port_type: PortType::RealF32,
                },
                PortSpec {
                    name: "events",
                    port_type: PortType::Events,
                },
            ],
            params: &[
                ParamSpec {
                    key: "mode",
                    label: "Mode",
                    kind: ParamKind::EnumString {
                        values: MODES,
                        default: "auto",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "dsd-fme decoder selection: auto (-fa) detects DMR / P25 / NXDN / D-STAR / YSF / dPMR from the sync pattern; pin dmr (-fs, DMR BS+MS), p25 (Phase 1), p25p2, nxdn48, nxdn96, dstar, ysf, dpmr, x2tdma when auto flips between sync types on a noisy channel; trunk (-ft) is dsd-fme's P25/DMR trunking follower (needs its own channel map; not wired to the tuner yet).",
                },
                ParamSpec {
                    key: "helper_path",
                    label: "dsd-fme path",
                    kind: ParamKind::Text { default: "" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Path to the operator-installed dsd-fme binary. Empty = $FERRITE_DSD_FME, else `dsd-fme` on PATH. A `<prefix>/bin/dsd-fme` layout gets `<prefix>/lib` on LD_LIBRARY_PATH automatically (libmbe). Ferrite does not ship it (AMBE vocoder patents); a `helper` event in the Journal says whether it started.",
                },
                ParamSpec {
                    key: "metadata",
                    label: "Call metadata",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Emit dsd-fme's call / alias / sync records. The `dmr` preset turns this off because the native DmrDecode block already reports DMR calls (and runs in the browser); helper and voice start/stop records are always emitted.",
                },
                ParamSpec {
                    key: "extra_args",
                    label: "Extra args",
                    kind: ParamKind::Text { default: "" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Appended to the dsd-fme command line verbatim, e.g. `-V 1` (voice from TDMA slot 1 only), `-q` (reverse mute), `-K <key>` / `-b <bits>` for basic privacy keys. Leave empty normally.",
                },
            ],
            ai_notes: "Digital voice via the operator-installed dsd-fme helper subprocess: feed it a 48 kHz FmDemod discriminator output (max_deviation_hz 5000, no de-emphasis, no audio shaping before it), it returns synthesised 8 kHz speech on `out` (resample before AudioSink) and call events on `events` → `ui:dsd` ({event: call|sync|helper, protocol, slot, tgt, src, cc, enc, text}). Encrypted calls show events, no audio. Needs the tuner on-centre of a 12.5 kHz channel; a `helper` event with 'failed to start' means dsd-fme is not installed. Output: `tail decoder --category dsd`; `await dsd`.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.input_rate_hz = rate;
                if (rate - HELPER_IN_RATE_HZ).abs() > 1.0 && !self.warned_off_rate {
                    tracing::warn!(
                        target: "decoder::dsd",
                        "DsdHelper input is {rate} Hz but dsd-fme expects {HELPER_IN_RATE_HZ} Hz; put a Channelizer/Resamp at 48 kHz upstream"
                    );
                    self.warned_off_rate = true;
                }
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.ensure_worker();
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.input_rate_hz = rate;
            }
        }
        Ok(())
    }

    fn output_rate_hz(&self, port: usize) -> Option<f64> {
        (port == 0).then_some(HELPER_OUT_RATE_HZ)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        // Block-progress rule: *always* consume every input sample. The
        // output is owed, not gated: each consumed sample adds 8/48 of an
        // output sample to `owed`, and we emit as much of the debt as the
        // output port's contiguous room allows this tick. Gating input on
        // output room deadlocked live (the ring hands out the contiguous
        // tail, which can be tiny at the wrap point).
        let consumed = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
            .map_or(0, <[f32]>::len);
        #[allow(clippy::cast_precision_loss)]
        {
            self.carry = (self.carry + consumed as f64 * HELPER_OUT_RATE_HZ / self.input_rate_hz)
                .min(MAX_OWED_SAMPLES);
        }

        #[cfg(not(target_arch = "wasm32"))]
        if let Some(worker) = self.worker.as_ref() {
            if consumed > 0 && worker.alive.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(src) = io
                    .inputs
                    .iter()
                    .find(|p| p.name == "in")
                    .and_then(InputPort::as_real_f32)
                {
                    let wrote = worker.tap.lock().unwrap().write(src);
                    if wrote < src.len() {
                        worker.dropped.fetch_add(
                            (src.len() - wrote) as u64,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                }
            }
            let mut q = worker.events.lock().unwrap();
            let metadata = self.params.metadata;
            for line in q.drain(..).filter(|l| {
                metadata || l.contains("\"event\":\"helper\"") || l.contains("\"event\":\"voice\"")
            }) {
                self.events_out.extend_from_slice(line.as_bytes());
                self.events_out.push(b'\n');
            }
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        if let Some((j, port)) = io
            .outputs
            .iter_mut()
            .enumerate()
            .find(|(_, p)| p.name == "out")
        {
            if let Some(dst) = port.as_real_f32_mut() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let n = (self.carry.floor() as usize).min(dst.len());
                let dst = &mut dst[..n];
                dst.fill(0.0);
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(worker) = self.worker.as_ref() {
                    let mut v = worker.voice.lock().unwrap();
                    for d in dst.iter_mut() {
                        match v.pop_front() {
                            Some(s) => *d = s,
                            None => break,
                        }
                    }
                }
                #[allow(clippy::cast_precision_loss)]
                {
                    self.carry -= n as f64;
                }
                w.produced[j] = n;
            }
        }
        drain_events_port(io, &mut self.events_out, &mut w);
        Ok(w)
    }
}

impl BlockFactory for DsdHelper {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: DsdHelperParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(DsdHelper::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        mode_flag, parse_line, strip_ansi, unpack_voice, DsdHelper, DsdHelperParams, ParseState,
    };

    #[test]
    fn modes_map_to_flags() {
        assert_eq!(mode_flag("auto").unwrap(), "-fa");
        assert_eq!(mode_flag("dmr").unwrap(), "-fs");
        assert_eq!(mode_flag("p25").unwrap(), "-f1");
        assert!(mode_flag("tetra").is_err());
        assert!(DsdHelper::new(DsdHelperParams {
            mode: "bogus".into(),
            ..DsdHelperParams::default()
        })
        .is_err());
    }

    #[test]
    fn strips_ansi_colour() {
        assert_eq!(
            strip_ansi("\u{1b}[1;31m SLOT 1\u{1b}[0m TGT=9"),
            " SLOT 1 TGT=9"
        );
    }

    #[test]
    fn sync_lines_set_protocol_and_colour_code_once() {
        let mut st = ParseState::default();
        // Real dsd-fme lines: leading timestamp, `|`-separated fields.
        let ev = parse_line(
            "20:58:01 Sync: +DMR  [slot1]  slot2  | Color Code=01 | IDLE",
            &mut st,
        )
        .unwrap();
        assert_eq!(ev.event, "sync");
        assert_eq!(ev.protocol.as_deref(), Some("DMR"));
        assert_eq!(st.cc, Some(1));
        assert!(parse_line(
            "20:58:01 Sync: +DMR   slot1  [SLOT2] | Color Code=01 | VC3",
            &mut st
        )
        .is_none());
        assert!(parse_line("20:58:02 Sync: no sync", &mut st).is_none());
        assert!(parse_line(
            "20:58:02 Sync:  DMR  [SLOT1]  slot2  | Frame Sync Err: 7",
            &mut st
        )
        .is_none());
        assert_eq!(st.protocol.as_deref(), Some("DMR"));
    }

    #[test]
    fn dmr_call_line_yields_tg_src_slot() {
        let mut st = ParseState {
            protocol: Some("DMR".into()),
            cc: Some(1),
        };
        let ev = parse_line(" SLOT 2 TGT=31068 SRC=3214431 Group Call", &mut st).unwrap();
        assert_eq!(ev.event, "call");
        assert_eq!(ev.slot, Some(2));
        assert_eq!(ev.tgt.as_deref(), Some("31068"));
        assert_eq!(ev.src.as_deref(), Some("3214431"));
        assert_eq!(ev.cc, Some(1), "colour code carried from the sync line");
        assert_eq!(ev.protocol.as_deref(), Some("DMR"));
        assert_eq!(ev.enc, None);
    }

    #[test]
    fn talker_alias_and_activity_updates() {
        let mut st = ParseState::default();
        let ev = parse_line(
            " Slot 1 - Talker Alias Block Num: 1; Valid Block; Talker Alias: KR1JOE",
            &mut st,
        )
        .unwrap();
        assert_eq!(ev.event, "alias");
        assert_eq!(ev.slot, Some(1));
        assert_eq!(ev.text, "KR1JOE");
        assert!(parse_line(
            " Activity Update TS1: Idle; Hash: 0; TS2: Group Voice; Hash: 140;",
            &mut st
        )
        .is_none());
        assert!(parse_line(" SLCO NULL", &mut st).is_none());
    }

    #[test]
    fn p25_group_voice_line_and_frame_noise() {
        let mut st = ParseState {
            protocol: Some("P25p1".into()),
            cc: None,
        };
        let ev = parse_line(
            " Group Voice Channel User - Group 1201 Source 7001234",
            &mut st,
        )
        .unwrap();
        assert_eq!(ev.tgt.as_deref(), Some("1201"));
        assert_eq!(ev.src.as_deref(), Some("7001234"));
        assert!(parse_line("  VC6 ", &mut st).is_none());
    }

    #[test]
    fn encrypted_flag() {
        let mut st = ParseState::default();
        let ev = parse_line(" SLOT 1 TGT=1 SRC=2 ENC Privacy Indicator", &mut st).unwrap();
        assert_eq!(ev.enc, Some(true));
    }

    #[test]
    fn unpacks_mono_and_stereo_datagrams() {
        let mono: Vec<u8> = [16_384_i16, -16_384]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        unpack_voice(&mono, &mut out);
        assert_eq!(out, vec![0.5, -0.5]);

        // 320 stereo frames (1280 B): L=0.25, R=0.25 → summed 0.5.
        let frame: Vec<u8> = [8_192_i16, 8_192]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let stereo: Vec<u8> = frame.iter().copied().cycle().take(1280).collect();
        out.clear();
        unpack_voice(&stereo, &mut out);
        assert_eq!(out.len(), 320);
        assert!((out[0] - 0.5).abs() < 1e-6);
    }

    /// The whole subprocess path against a stand-in helper (bash): the
    /// worker binds its loopback sockets, launches the "helper" with
    /// `-i tcp:… -o udp:…`, the helper connects, prints a coloured sync
    /// line and a DMR call line on stderr, waits for our audio, and
    /// sends one voice datagram back. Covers ring construction (a
    /// non-power-of-two tap capacity panicked here on first live start),
    /// spawn, accept, the stderr parser and the UDP receiver.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn worker_round_trip_with_stand_in_helper() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!("dsd-helper-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-dsd-fme");
        std::fs::write(
            &script,
            "#!/bin/bash\n\
             tcp=${2#tcp:}; udp=${4#udp:}\n\
             exec 3<>/dev/tcp/${tcp%:*}/${tcp##*:} || exit 3\n\
             printf '\\033[1;31mSync: +DMR BS  mod: GFSK\\033[0m\\n' >&2\n\
             echo ' SLOT 1 TGT=91 SRC=3106123 Group Call CC=1' >&2\n\
             head -c 1000 <&3 >/dev/null\n\
             printf '\\x00\\x40\\x00\\x40' > /dev/udp/${udp%:*}/${udp##*:}\n\
             sleep 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let worker = super::native::Worker::spawn(&script, "-fa", "");
        assert_eq!(
            worker.tap.lock().unwrap().capacity(),
            super::native::TAP_CAPACITY
        );
        worker.tap.lock().unwrap().write(&[0.1_f32; 2_000]);

        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut events, mut voice) = (Vec::new(), Vec::new());
        while Instant::now() < deadline
            && (voice.len() < 2 || !events.iter().any(|e: &String| e.contains("\"call\"")))
        {
            events.extend(worker.events.lock().unwrap().drain(..));
            voice.extend(worker.voice.lock().unwrap().drain(..));
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(worker);
        let _ = std::fs::remove_dir_all(&dir);

        let joined = events.join("\n");
        assert!(
            joined.contains("\"event\":\"helper\""),
            "no helper start event: {joined}"
        );
        assert!(
            joined.contains("\"event\":\"sync\"") && joined.contains("\"protocol\":\"DMR\""),
            "no DMR sync: {joined}"
        );
        assert!(
            joined.contains("\"tgt\":\"91\"")
                && joined.contains("\"src\":\"3106123\"")
                && joined.contains("\"slot\":1"),
            "no call event: {joined}"
        );
        assert_eq!(voice, vec![0.5, 0.5], "voice datagram not received");
    }

    /// `process` with two outputs: audio on port 0 is clocked at 8/48 of
    /// the input (silence when there is no voice) and the helper event
    /// lands on port 1. Pins the `drain_events_port` index fix — it used
    /// to write the events byte count into `produced[0]`, so the audio
    /// count was clobbered and the block stalled live (330 k calls, input
    /// stuck at one ring's worth).
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn process_clocks_audio_and_routes_events_to_port_1() {
        use crate::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
        use crate::Block;

        let mut b = DsdHelper::new(DsdHelperParams {
            helper_path: "/nonexistent/dsd-fme".into(),
            ..DsdHelperParams::default()
        })
        .unwrap();
        b.ensure_worker();
        let input = vec![0.0_f32; 600];
        let mut audio = vec![1.0_f32; 1_000];
        let mut events = vec![0_u8; 4_096];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(&input),
        }];
        let mut outputs = [
            OutputPort {
                name: "out",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut audio),
            },
            OutputPort {
                name: "events",
                meta: PortMeta::default(),
                buf: OutBuf::Events(&mut events),
            },
        ];
        let w = b
            .process(&mut BlockIo {
                inputs: &mut inputs,
                outputs: &mut outputs,
            })
            .unwrap();
        assert_eq!(w.consumed[0], 600);
        assert_eq!(w.produced[0], 100, "600 in at 48 kHz → 100 out at 8 kHz");
        assert!(audio[..100].iter().all(|&s| s == 0.0), "no voice → silence");
        assert!(
            w.produced[1] > 0,
            "helper event must be committed on port 1"
        );
        let text = String::from_utf8_lossy(&events[..w.produced[1]]);
        assert!(text.contains("failed to start"), "{text}");
    }

    /// Block-progress rule: with *no* output room (the ring's contiguous
    /// tail at the wrap point) the block must still consume all of its
    /// input and emit the owed audio next tick. Gating input on output
    /// room stalled the whole chain back to the SDR live.
    #[test]
    fn consumes_input_with_zero_output_room_and_repays_later() {
        use crate::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
        use crate::Block;

        let mut b = DsdHelper::new(DsdHelperParams::default()).unwrap();
        let input = vec![0.0_f32; 1_200];
        let run = |b: &mut DsdHelper, room: usize| {
            let mut audio = vec![0.0_f32; room];
            let mut inputs = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&input),
            }];
            let mut outputs = [OutputPort {
                name: "out",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut audio),
            }];
            b.process(&mut BlockIo {
                inputs: &mut inputs,
                outputs: &mut outputs,
            })
            .unwrap()
        };
        let w = run(&mut b, 0);
        assert_eq!(w.consumed[0], 1_200, "must consume despite zero room");
        assert_eq!(w.produced[0], 0);
        let w = run(&mut b, 1_000);
        assert_eq!(w.consumed[0], 1_200);
        assert_eq!(w.produced[0], 400, "200 owed + 200 new");
    }
}
