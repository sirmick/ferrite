//! `ferrite-ctl` — CLI driver for a running ferrited.
//!
//! A thin human front-end over the MCP operations layer in
//! [`mcp`]. Every subcommand parses argv, builds the same typed args an
//! MCP tool would receive, calls the matching `FerriteServer::*_op`
//! method, and prints the JSON it returns. The MCP stdio server
//! (`ferrite-ctl mcp`) and this CLI therefore share one implementation
//! per verb — they cannot drift.
//!
//! Output is JSON: pretty-printed by default, one-line compact under
//! `--json` (friendly for `jq -c` / line consumers). Drives the same
//! running ferrited the browser UI uses (`--connect`, defaults to
//! `http://127.0.0.1:10001`).

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{Map, Value};

mod mcp;

use mcp::{
    AwaitArgs, BandAtArgs, BlockParamsArgs, ChatPostArgs, FerriteServer, Http, LoadPresetArgs,
    PresetDetailArgs, RecentDecodesArgs, ReplayCaptureArgs, SelectDeviceArgs, SetViewStateArgs,
    SignalsArgs, StartCaptureArgs, StartCaptureAudioArgs, TranscribeArgs, TuneArgs,
    ViewSnapshotArgs,
};

const DEFAULT_CONNECT: &str = "http://127.0.0.1:10001";

#[derive(Parser, Debug)]
#[command(
    name = "ferrite-ctl",
    version,
    about = "CLI driver for a running ferrited",
    long_about = None,
)]
struct Cli {
    /// Base URL of the ferrited to drive. `FERRITE_CTL_CONNECT` sets it
    /// for every caller at once (run.sh exports it; the AI sidecar and
    /// the chat-mirror hook read the same variable).
    #[arg(long, env = "FERRITE_CTL_CONNECT", default_value = DEFAULT_CONNECT, global = true)]
    connect: String,

    /// Emit compact one-line JSON instead of the pretty-printed
    /// default. Output is JSON either way.
    #[arg(long, global = true)]
    json: bool,

    /// HTTP timeout per request (seconds).
    #[arg(long, default_value_t = 10, global = true)]
    timeout: u64,

    /// Free-form annotation attached to every mutating request as
    /// `X-Ferrite-Note`. The daemon broadcasts it under target
    /// `ai::activity` so the UI's log panel surfaces what's happening
    /// and why ("checking APRS at 144.39 MHz", etc). Read-only polls
    /// don't log, so `tail` / `status` don't flood the stream.
    #[arg(long, global = true)]
    note: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Manage / inspect SDR devices known to the daemon.
    #[command(subcommand)]
    Device(DeviceCmd),
    /// Browse + load preset flowgraphs.
    #[command(subcommand)]
    Preset(PresetCmd),
    /// Tune the live source's centre frequency (and optionally
    /// bandwidth / sample-rate / gain). Routes through the dodge-aware
    /// tune path.
    Tune {
        /// Listen (dial) frequency in Hz. Accepts plain integers or the
        /// `123.45M` / `915k` shorthand. Omit with `--carrier`.
        freq: Option<String>,
        /// Put this RF carrier on an audio tone instead: dial = carrier
        /// − tone (USB) / + tone (LSB). For CW: the decoder's tone is
        /// used unless `--tone` is given.
        #[arg(long)]
        carrier: Option<String>,
        /// Tone (Hz) for `--carrier`; default = the decoder's rx_freq_hz.
        #[arg(long)]
        tone: Option<f64>,
        /// Optional bandwidth (Hz, with same shorthand).
        #[arg(long)]
        bw: Option<String>,
        /// Optional sample rate (Hz).
        #[arg(long)]
        rate: Option<String>,
        /// Optional gain (dB). Folds `agc=false` in so the manual
        /// value lands on AGC-defaulting drivers.
        #[arg(long)]
        gain: Option<f64>,
        /// Optional sample-rate floor (Hz) for the tune span.
        #[arg(long)]
        span: Option<String>,
        /// Per-driver DC-spike dodge ratio (HackRF ~0.7; DC-tracking
        /// drivers 0). Must exceed 0.5 when non-zero.
        #[arg(long)]
        offset: Option<f64>,
        /// Stay-in-span: if the target is already inside the current span,
        /// retune the channelizer only (no LO move, no graph restart, no
        /// worker reload). Ideal for hopping stations during a survey.
        #[arg(long)]
        keep_lo: bool,
        /// Leave the spectrum analyzer if it holds the radio, then tune.
        #[arg(long)]
        force: bool,
    },
    /// Patch parameter(s) on a pipeline block — the advanced/direct
    /// surface. `key=value` pairs; values parse as JSON when possible
    /// (numbers, booleans), else string. The `src` block accepts
    /// gain_db / gain_mode / gain_elements / agc / antenna / bandwidth_hz
    /// / dc_offset_correction / settings.
    ///
    /// Prefer the convenience paths where one exists: use `tune` for
    /// frequency (NOT `param src center_freq_hz` / `param chan
    /// freq_shift_hz` — they skip the DC-spike dodge and can park the
    /// carrier on DC), and the `gain_db` auto dial (NOT
    /// `gain_mode=manual`/`gain_elements`) unless you need per-stage gain.
    Param {
        /// Block id, e.g. `demod`, `audio_nr`, `chan`, `src`.
        block: String,
        /// One or more `key=value` pairs.
        #[arg(value_parser = parse_kv, required = true)]
        kv: Vec<(String, Value)>,
    },
    /// Start the pipeline (instantiates blocks if stopped).
    Start,
    /// Stop the pipeline.
    Stop,
    /// Toggle speech-to-text (VoiceTranscribe tap). Follow the transcript
    /// with `await transcribe -f`.
    Transcribe {
        /// `on` to enable transcription, `off` to disable.
        #[arg(value_parser = ["on", "off"])]
        state: String,
        /// Where it runs: `node` is headless (whisper in `ferrited`, no
        /// browser); `browser` uses the in-browser worker. Omit to leave
        /// the current placement unchanged.
        #[arg(long, value_parser = ["node", "browser"])]
        placement: Option<String>,
    },
    /// One-shot world snapshot: pipeline status (+ `ready`), source
    /// config, active flowgraph, UI sinks.
    Status,
    /// Capability schema of the currently-bound source (antennas, gain
    /// ladder, rate/bw/freq ranges, driver settings).
    Caps,
    /// Per-block flow diagnostics (throughput / process-time / ring fill)
    /// — the "are samples actually moving?" health check. `null` stopped.
    Flowdiag,
    /// Latest driver readback (actual gain / AGC / antenna / bandwidth as
    /// the hardware reports them). `null` when stopped or non-Soapy.
    Readback,
    /// Every block in the active preset with its full spec + values.
    Blocks,
    /// Every registered block type's schema (static).
    BlockTypes,
    /// List the curated `samples/` tree of replayable IQ/audio fixtures.
    Captures,
    /// Replay a recorded sample as the source (full RX chain via
    /// ModulatedFileSource) — deterministic testing with no SDR.
    Replay {
        /// Sample path (absolute server `path` or `samples/`-relative
        /// `rel` from `captures`), or any server file path.
        path: String,
        /// `iq` or `audio`. Defaults from the capture entry, else audio.
        #[arg(long)]
        kind: Option<String>,
        /// Audio replay carrier: `fm` / `am` / `usb` / `lsb` / `ssb`.
        #[arg(long)]
        modulation: Option<String>,
        /// Sample-rate hint (Hz, SI shorthand). Defaults from the entry.
        #[arg(long)]
        rate: Option<String>,
        /// Centre frequency to claim (Hz, SI shorthand). Defaults entry.
        #[arg(long)]
        center: Option<String>,
    },
    /// What US allocation band(s) contain a frequency.
    BandAt {
        /// Frequency in Hz (SI shorthand OK).
        freq: String,
    },
    /// Current strongest-signal watchlist (snapshot, ranked by power).
    /// Each row's `freq_hz` feeds straight into `tune`.
    Signals {
        /// Report the list empty if nothing was detected within this many
        /// ms (0 = no freshness gate). Default 2000.
        #[arg(long)]
        max_age_ms: Option<u64>,
    },
    /// Few-Hz-resolution spectrum of a range from live IQ (no retune):
    /// peaks with interpolated frequency, level over the floor, width,
    /// and whether they're keyed (CW / beacons). Feed a peak's freq_hz
    /// to `tune --carrier`.
    Spectrum {
        /// Range start (Hz; `14.040M` shorthand ok).
        lo: String,
        /// Range end.
        hi: String,
        /// Seconds of IQ to analyse.
        #[arg(long, default_value_t = 3.0)]
        secs: f64,
        /// Target resolution, Hz/bin.
        #[arg(long, default_value_t = 10.0)]
        res: f64,
        /// Peaks to list.
        #[arg(long, default_value_t = 25)]
        peaks: usize,
        /// Only keyed peaks (CW / beacons).
        #[arg(long)]
        keyed: bool,
        /// Analyse a kept snapshot instead of taking a fresh one.
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// Spectrum-analyzer sweep across any range (steps the radio ~7.6 MHz
    /// at a time): floor, emitters, max-hold profile. Takes the radio —
    /// `--force` if a receiver is running, `--restore` to give it back.
    Sweep {
        /// Range start (Hz; `88M` shorthand ok). Omit to keep the current.
        lo: Option<String>,
        /// Range end.
        hi: Option<String>,
        #[arg(long)]
        rbw: Option<f64>,
        #[arg(long)]
        averages: Option<u32>,
        /// Fresh passes to fold (max-hold, duty).
        #[arg(long, default_value_t = 1)]
        passes: u32,
        #[arg(long)]
        threshold: Option<f32>,
        #[arg(long, default_value_t = 20)]
        peaks: usize,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        restore: bool,
    },
    /// Clear the AI transcript (every AI panel empties; also resets the
    /// in-app assistant's conversation context).
    ClearAi,
    /// The emitter list (both detectors merged, numbered, persisted).
    Emitters {
        /// new | identified | ignored | all (default: all but ignored).
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        lo: Option<String>,
        #[arg(long)]
        hi: Option<String>,
        /// level | freq | recent | new
        #[arg(long)]
        sort: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Label an emitter (sets it identified), or `--status ignored`.
    LabelEmitter {
        id: u64,
        label: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        confidence: Option<f32>,
        #[arg(long)]
        notes: Option<String>,
    },
    /// Record a few seconds of live IQ and keep it by id (no retune).
    Snapshot {
        #[arg(long, default_value_t = 3.0)]
        secs: f64,
    },
    /// Time × frequency picture of a range (printed as text; drawn in
    /// the AI tab).
    Spectrogram {
        lo: String,
        hi: String,
        #[arg(long)]
        snapshot: Option<String>,
        #[arg(long, default_value_t = 3.0)]
        secs: f64,
        /// Hz per column.
        #[arg(long, default_value_t = 10.0)]
        res: f64,
        /// ms per row.
        #[arg(long, default_value_t = 20.0)]
        step: f64,
    },
    /// On/off timeline at one frequency (printed as a #/. strip).
    Envelope {
        freq: String,
        #[arg(long)]
        snapshot: Option<String>,
        #[arg(long, default_value_t = 3.0)]
        secs: f64,
        #[arg(long, default_value_t = 200.0)]
        bw: f64,
        /// Window, ms.
        #[arg(long, default_value_t = 10.0)]
        window: f64,
    },
    /// Signal Identification Guide (sigidwiki.com) lookups.
    #[command(subcommand)]
    Sigwiki(SigwikiCmd),
    /// Where the FT8 / FT4 / WSPR stations heard are: distance + bearing
    /// table (and a map card in the AI tab).
    SpotMap {
        /// `ft8`, `ft4` or `wspr`.
        #[arg(long, default_value = "ft8")]
        kind: String,
        /// Look back this many minutes.
        #[arg(long, default_value_t = 30.0)]
        minutes: f64,
        /// Receive grid (default: the daemon's FERRITE_RX_GRID).
        #[arg(long)]
        rx_grid: Option<String>,
    },
    /// Save a preset's sigidwiki reference waterfall to a file; print
    /// its refs.
    ReferenceImage {
        preset: String,
        /// Output path (default /tmp/ferrite-ref-<preset>.<ext>).
        #[arg(long)]
        out: Option<String>,
    },
    /// Capture a slice of signal to disk (IQ / audio / FFT). Blocks
    /// until the async job finishes, then prints the job record.
    #[command(subcommand)]
    Capture(CaptureCmd),
    /// Read / replace / edit the running flowgraph.
    #[command(subcommand)]
    Flowgraph(FlowgraphCmd),
    /// Wait for the next record(s) of a store kind on the event plane
    /// (MQTT subscription — no polling). Prints the records as JSON when
    /// `--max` arrived or `--timeout` elapsed; with `--follow`, streams
    /// one record per line until SIGINT.
    Await {
        /// Store kind or log subtree: `adsb`, `pager`, `transcribe`,
        /// `activity`, `graph`, `log/decoder`, `log/decoder/pocsag`, …
        kind: String,
        /// Stop after this many records (ignored with --follow).
        #[arg(long, default_value_t = 1)]
        max: usize,
        /// Give up after this many seconds (ignored with --follow). Not
        /// `--timeout`: that's the global per-request HTTP timeout.
        #[arg(long = "wait", default_value_t = 30.0)]
        wait_s: f64,
        /// Skip records with seq <= this.
        #[arg(long)]
        since_seq: Option<u64>,
        /// Also print the retained (current) record first.
        #[arg(long)]
        retained: bool,
        /// Stream records until SIGINT (one JSON line each).
        #[arg(long, short = 'f')]
        follow: bool,
        /// Only records whose data field matches: `--where status=done`;
        /// a comma list is any-of (`--where status=done,failed`). Values
        /// that parse as JSON numbers/bools compare as such. Repeatable.
        #[arg(long = "where", value_name = "FIELD=VALUE")]
        where_: Vec<String>,
    },
    /// Tail log lines live (event-plane subscription) until SIGINT.
    Tail {
        /// Tracing-target prefix to follow (e.g. `decoder::pocsag`,
        /// `decoder` for everything decoder-side, `driver`, `radio`).
        #[arg(default_value = "decoder")]
        category: String,
        /// First print this many seconds of history from the log ring.
        #[arg(long, default_value_t = 0.0)]
        lookback: f64,
    },
    /// Inspect the decoder log buffer (one-shot recall).
    #[command(subcommand)]
    Decoder(DecoderCmd),
    /// Snapshot a spectrum / waterfall canvas to a PNG, print the path.
    /// Requires a UI tab subscribed to `/ws/ui-views`.
    View {
        /// One of `wide-spectrum`, `wide-waterfall`, `channel-spectrum`,
        /// `channel-waterfall`.
        pane: String,
    },
    /// Print the browser's authored chrome state (active tab,
    /// channel-detail visibility, per-pane zoom + pause).
    ViewGet,
    /// Push a chrome-state patch to the operator's browser.
    ViewSet {
        /// Wide left column: `wide` (FFT/Waterfall) or a left view id —
        /// `transcript`, `ft8`, `adsb`, `aprs`, `fldigi`, `journal:<kind>`
        /// (`advanced` = the first).
        #[arg(long)]
        main_pane: Option<String>,
        /// Narrow right column: `off`, `channel`, `signals`, or a right
        /// view id (`calls:dsd`).
        #[arg(long)]
        right_pane: Option<String>,
        /// Show/hide the channel-detail pane.
        #[arg(long)]
        channel_detail: Option<bool>,
    },
    /// Prompt the in-app AI (the ferrite-ai sidecar). The turn runs as
    /// if typed into the AI panel — thinking, tool calls and replies
    /// show on every panel. Continues the mode's latest session unless
    /// `--session` or `--new`.
    AiPrompt {
        /// What to ask.
        text: String,
        /// `explorer` (default), `decoder`, `diagnose` or `chat`.
        #[arg(long)]
        mode: Option<String>,
        /// Model id override (e.g. `claude-sonnet-5-5`).
        #[arg(long)]
        model: Option<String>,
        /// Continue this session id.
        #[arg(long)]
        session: Option<String>,
        /// Start a fresh session.
        #[arg(long)]
        new: bool,
        /// Wait for the turn to end and print the reply.
        #[arg(long)]
        wait: bool,
    },
    /// Mirror a conversation turn into the operator's browser AI panel
    /// (so a headless driver's prompts / replies / tool calls show in
    /// every AI panel alongside the in-browser assistant, via the `ai`
    /// session bus — no tab needed).
    ChatPost {
        /// `user` (a prompt), `assistant` (a reply, auto-closed), or
        /// `tool` (a tool-call card in the open assistant turn).
        #[arg(value_parser = ["user", "assistant", "tool"])]
        role: String,
        /// Message text (user / assistant). Optional for `tool`.
        #[arg(default_value = "")]
        text: String,
        /// `tool` role: the tool name shown on the card.
        #[arg(long)]
        tool_name: Option<String>,
        /// `tool` role: the tool input rendered under the card.
        #[arg(long)]
        tool_input: Option<String>,
        /// `tool` role: the tool result text, if any.
        #[arg(long)]
        tool_result: Option<String>,
        /// The driving session's id (groups its turns on the panel).
        #[arg(long)]
        session: Option<String>,
        /// Who is talking (label on the panel). Default `claude-code`.
        #[arg(long)]
        source: Option<String>,
    },
    /// Dev codegen: regenerate the if-filter-ladders artifact from the
    /// Rust source of truth. Purely local — no running ferrited.
    #[command(hide = true)]
    GenTables,
    /// Run as an MCP (Model Context Protocol) server over stdio. Exposes
    /// every ferrite-ctl verb as a discrete tool to any MCP-enabled
    /// client. See tools/ferrite-ctl/src/mcp.rs and docs/02-protocol.md.
    Mcp,
}

#[derive(Subcommand, Debug)]
enum SigwikiCmd {
    /// Rank candidates for a signal: `sigwiki identify 7.0362M --bw 60 --mode CW`.
    Identify {
        freq: String,
        /// Occupied bandwidth, Hz.
        #[arg(long)]
        bw: Option<f64>,
        /// CW / USB / LSB / AM / NFM / WFM …
        #[arg(long)]
        mode: Option<String>,
        /// OOK / FSK / MFSK / PSK / AFSK / FM …
        #[arg(long)]
        modulation: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Find signals by words.
    Search {
        query: Vec<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One signal's entry; `--picture <path>` saves its waterfall image.
    Page {
        title: Vec<String>,
        #[arg(long)]
        picture: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum CaptureCmd {
    /// Capture IQ. Default = non-disruptive live narrowband (tees the
    /// post-channelizer cf32 stream). `--wideband` = full-rate
    /// `Source → FileIqSink` (clobbers the live preset; needs a freq).
    Iq {
        /// Centre frequency (Hz, SI shorthand). Optional: omit to
        /// record the session as-is, give one to retune first.
        freq: Option<String>,
        /// Capture duration in seconds.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// Sample rate (Hz). `--wideband` only.
        #[arg(long, default_value = "2.0M")]
        rate: String,
        /// Bandwidth (Hz). `--wideband` only.
        #[arg(long)]
        bw: Option<String>,
        /// Front-end gain (dB). `--wideband` only.
        #[arg(long)]
        gain: Option<f64>,
        /// Output path. Defaults under `/tmp/ferrite-captures/`.
        #[arg(long)]
        out: Option<String>,
        /// `cf32` or `wav-s16`. `--wideband` only.
        #[arg(long, default_value = "cf32")]
        format: String,
        /// Tee the raw full-span IQ off the SDR (block `src`) instead
        /// of the channelized slice. Live, no preset swap; 8 B × rate
        /// per second of disk.
        #[arg(long)]
        wideband: bool,
        /// Proceed even if it would tear down an active transcription.
        #[arg(long)]
        force: bool,
    },
    /// Capture demodulated audio to a WAV — a live tee of what the
    /// operator is hearing (no preset swap). Omit FREQ to record the
    /// current session as-is; give one to retune first.
    Audio {
        /// Centre frequency (Hz, SI shorthand). Optional.
        freq: Option<String>,
        /// Capture duration in seconds.
        #[arg(long, default_value_t = 10.0)]
        duration: f64,
        /// Source rate (Hz).
        #[arg(long, default_value = "2.4M")]
        rate: String,
        /// Bandwidth (Hz). Defaults to the rate.
        #[arg(long)]
        bw: Option<String>,
        /// Gain (dB).
        #[arg(long)]
        gain: Option<f64>,
        /// Preset to use (must end in a FileAudioSink-shaped block).
        #[arg(long, default_value = "fm-audio-record")]
        preset: String,
        /// Output path. Defaults under `/tmp/ferrite-captures/`.
        #[arg(long)]
        out: Option<String>,
        /// Proceed even if it would tear down an active transcription.
        #[arg(long)]
        force: bool,
    },
    /// Capture the byte-quantised FFT waterfall (live tee on `logmag`).
    Fft {
        /// Centre frequency (Hz, SI shorthand). Optional.
        freq: Option<String>,
        /// Capture duration in seconds.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// Output path. Defaults under `/tmp/ferrite-captures/`.
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum DeviceCmd {
    /// List devices the daemon currently sees (warm cache + fresh
    /// enumerate), with full capability schemas.
    List,
    /// Set the source by Soapy-style args, e.g.
    /// `driver=sdrplay,serial=224001D748`. Resets source params to the
    /// driver's defaults; follow with `tune`. Triggers a source restart.
    Select {
        /// Soapy args string (`driver=…,serial=…`).
        args: String,
    },
    /// Recover from an in-process SoapySDR driver wedge — tear down and
    /// re-load every Soapy module. Refuses (409) while running.
    Reload,
}

#[derive(Subcommand, Debug)]
enum PresetCmd {
    /// List presets the daemon can load (name + label + description).
    List,
    /// Load a preset by name (matches `flowgraphs/<name>.json`).
    Load { name: String },
    /// Full JSON of one preset by name (ai_notes, block graph, sigwiki
    /// refs, sample path) — the gotchas + canonical frequencies.
    Detail { name: String },
    /// Save the running graph (live params included) as a user preset.
    Save {
        name: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Replace an earlier save of the same name.
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Subcommand, Debug)]
enum FlowgraphCmd {
    /// Print the running preset doc (live params folded in).
    Get,
    /// Replace the running preset with a doc from a JSON file (`-` = stdin).
    Put { file: String },
    /// Swap one block's type + params, keeping its id and wiring:
    /// `flowgraph swap morse CwDemod afc=false rx_freq_hz=700`.
    Swap {
        block: String,
        #[arg(value_name = "TYPE")]
        type_name: String,
        /// `key=value` params for the new block (JSON values parse).
        #[arg(value_parser = parse_kv)]
        kv: Vec<(String, Value)>,
    },
}

#[derive(Subcommand, Debug)]
enum DecoderCmd {
    /// One-shot recall of recent decoder log entries.
    Recent {
        /// Tracing-target prefix to filter on.
        #[arg(long, default_value = "decoder")]
        category: String,
        /// Pull entries emitted in the last N seconds.
        #[arg(long, default_value_t = 30.0)]
        lookback: f64,
        /// Incremental cursor: only entries strictly after this unix-ms
        /// watermark. Pass the previous call's `now` to get just what's
        /// new. Overrides `--lookback` when set.
        #[arg(long)]
        since: Option<u64>,
        /// `tail` — print only the newest N entries (0 = no cap).
        #[arg(long, default_value_t = 0_usize)]
        limit: usize,
    },
}

fn parse_kv(s: &str) -> Result<(String, Value), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {s:?}"))?;
    let parsed = serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.to_string()));
    Ok((k.to_string(), parsed))
}

/// Parse 100M / 100k / 100.0 / 100000 — SI suffixes for freq / bw.
fn parse_si(s: &str) -> Result<f64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty number");
    }
    let (num, mul) = match s.chars().last().unwrap() {
        'k' | 'K' => (&s[..s.len() - 1], 1e3),
        'M' => (&s[..s.len() - 1], 1e6),
        'G' => (&s[..s.len() - 1], 1e9),
        _ => (s, 1.0),
    };
    let v: f64 = num.parse().with_context(|| format!("parse {s:?}"))?;
    Ok(v * mul)
}

/// Optional SI parse — `None` stays `None`.
fn parse_si_opt(s: Option<&str>) -> Result<Option<f64>> {
    s.map(parse_si).transpose()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let client = match build_client(cli.timeout) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ferrite-ctl: {e:#}");
            return ExitCode::from(2);
        }
    };
    let base = cli.connect.trim_end_matches('/').to_string();
    let http = Http::new(client, base, cli.note.clone());

    // The MCP server owns stdio for the JSON-RPC stream, so it's handled
    // before constructing the CLI-facing server (which would print).
    if matches!(cli.cmd, Cmd::Mcp) {
        return match mcp::serve(http.via_mcp()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ferrite-ctl: {e:#}");
                ExitCode::from(1)
            }
        };
    }
    if matches!(cli.cmd, Cmd::GenTables) {
        return match ferrite_sdr_tables::write_web_ladders() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ferrite-ctl: {e:#}");
                ExitCode::from(1)
            }
        };
    }

    let server = FerriteServer::from_http(http);
    match run(cli.cmd, &server, cli.json).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ferrite-ctl: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn build_client(timeout_s: u64) -> Result<reqwest::Client> {
    // Bare client — the ops layer stamps `x-ferrite-command` +
    // `x-ferrite-note` per mutating call (see mcp::Http), so no default
    // activity headers are baked in here.
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_s))
        .build()
        .context("build http client")
}

/// Print a Value: pretty by default, compact under `--json`.
fn emit(v: &Value, compact: bool) -> Result<()> {
    let s = if compact {
        serde_json::to_string(v)?
    } else {
        serde_json::to_string_pretty(v)?
    };
    println!("{s}");
    Ok(())
}

/// Analysis replies: the text rendering for a person, JSON with `--json`.
fn print_text_or_json(v: &Value, json: bool) -> Result<()> {
    if json {
        return emit(v, true);
    }
    if let Some(t) = v.get("text").and_then(Value::as_str) {
        if let Some(s) = v.get("snapshot") {
            println!(
                "{} · {} · {:.1} s",
                s.get("id").and_then(Value::as_str).unwrap_or("?"),
                v.get("analysis_id").and_then(Value::as_str).unwrap_or("?"),
                s.get("secs").and_then(Value::as_f64).unwrap_or(0.0)
            );
        }
        print!("{t}");
        return Ok(());
    }
    emit(v, false)
}

/// Adapt an op's `McpError` into the CLI's `anyhow` error channel.
fn op_err(e: rmcp::ErrorData) -> anyhow::Error {
    anyhow!("{e}")
}

async fn run(cmd: Cmd, server: &FerriteServer, json: bool) -> Result<()> {
    match cmd {
        Cmd::Device(DeviceCmd::List) => {
            emit(&server.list_devices_op().await.map_err(op_err)?, json)
        }
        Cmd::Device(DeviceCmd::Select { args }) => {
            let v = server
                .select_device_op(SelectDeviceArgs { args, note: None })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Device(DeviceCmd::Reload) => {
            emit(&server.reload_drivers_op().await.map_err(op_err)?, json)
        }
        Cmd::Preset(PresetCmd::List) => {
            emit(&server.list_presets_op().await.map_err(op_err)?, json)
        }
        Cmd::Preset(PresetCmd::Load { name }) => {
            let v = server
                .load_preset_op(LoadPresetArgs { name, note: None })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Preset(PresetCmd::Save {
            name,
            label,
            description,
            overwrite,
        }) => {
            let v = server
                .save_preset_op(mcp::SavePresetArgs {
                    name,
                    label,
                    description,
                    overwrite,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Flowgraph(FlowgraphCmd::Get) => {
            emit(&server.get_flowgraph_op().await.map_err(op_err)?, json)
        }
        Cmd::Flowgraph(FlowgraphCmd::Put { file }) => {
            let text = if file == "-" {
                let mut t = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut t)?;
                t
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("read {file}"))?
            };
            let doc: Value = serde_json::from_str(&text).context("parse flowgraph JSON")?;
            let v = server
                .set_flowgraph_op(mcp::SetFlowgraphArgs { doc, note: None })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Flowgraph(FlowgraphCmd::Swap {
            block,
            type_name,
            kv,
        }) => {
            let params: Map<String, Value> = kv.into_iter().collect();
            let v = server
                .swap_block_op(mcp::SwapBlockArgs {
                    block,
                    type_name,
                    params: Some(params),
                    note: None,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Preset(PresetCmd::Detail { name }) => {
            let v = server
                .preset_detail_op(PresetDetailArgs { name })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Tune {
            freq,
            carrier,
            tone,
            bw,
            rate,
            gain,
            span,
            offset,
            keep_lo,
            force,
        } => {
            if freq.is_none() && carrier.is_none() {
                bail!("give a frequency or --carrier");
            }
            let args = TuneArgs {
                freq_hz: parse_si_opt(freq.as_deref())?,
                carrier_hz: parse_si_opt(carrier.as_deref())?,
                tone_hz: tone,
                span_hz: parse_si_opt(span.as_deref())?,
                offset_ratio: offset,
                gain_db: gain,
                bandwidth_hz: parse_si_opt(bw.as_deref())?,
                sample_rate_hz: parse_si_opt(rate.as_deref())?,
                keep_lo,
                force,
                note: None,
            };
            emit(&server.tune_op(args).await.map_err(op_err)?, json)
        }
        Cmd::Param { block, kv } => {
            let mut params = Map::new();
            for (k, v) in kv {
                params.insert(k, v);
            }
            let v = server
                .set_block_param_op(BlockParamsArgs {
                    block,
                    params: Value::Object(params),
                    note: None,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Start => emit(&server.start_op().await.map_err(op_err)?, json),
        Cmd::Stop => emit(&server.stop_op().await.map_err(op_err)?, json),
        Cmd::Transcribe { state, placement } => {
            let v = server
                .transcribe_op(TranscribeArgs {
                    enabled: state == "on",
                    placement,
                    note: None,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Status => emit(&server.status_op().await.map_err(op_err)?, json),
        Cmd::Caps => emit(
            &server.source_capabilities_op().await.map_err(op_err)?,
            json,
        ),
        Cmd::Flowdiag => emit(&server.flowdiag_op().await.map_err(op_err)?, json),
        Cmd::Readback => emit(&server.source_readback_op().await.map_err(op_err)?, json),
        Cmd::Blocks => emit(&server.list_blocks_op().await.map_err(op_err)?, json),
        Cmd::BlockTypes => emit(&server.list_block_types_op().await.map_err(op_err)?, json),
        Cmd::Captures => emit(&server.list_captures_op().await.map_err(op_err)?, json),
        Cmd::Replay {
            path,
            kind,
            modulation,
            rate,
            center,
        } => {
            let v = server
                .replay_capture_op(ReplayCaptureArgs {
                    path,
                    kind,
                    modulation,
                    rate_hz_hint: parse_si_opt(rate.as_deref())?,
                    center_freq_hz: parse_si_opt(center.as_deref())?,
                    loop_playback: None,
                    note: None,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::BandAt { freq } => {
            let v = server
                .band_at_op(BandAtArgs {
                    freq_hz: parse_si(&freq)?,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Spectrum {
            lo,
            hi,
            secs,
            res,
            peaks,
            keyed,
            snapshot,
        } => {
            let mut v = server
                .spectrum_op(mcp::SpectrumArgs {
                    snapshot,
                    lo_hz: parse_si(&lo)?,
                    hi_hz: parse_si(&hi)?,
                    secs: Some(secs),
                    res_hz: Some(res),
                    max_peaks: Some(peaks),
                    // The UI draws the record's profile; only the CLI's
                    // own printout drops it.
                    profile_points: Some(120),
                    note: None,
                })
                .await
                .map_err(op_err)?;
            if keyed {
                if let Some(arr) = v.get_mut("peaks").and_then(Value::as_array_mut) {
                    arr.retain(|p| p.get("keyed").and_then(Value::as_bool) == Some(true));
                }
            }
            if !json {
                if let Some(o) = v.as_object_mut() {
                    o.remove("profile");
                }
            }
            emit(&v, json)
        }
        Cmd::Sweep {
            lo,
            hi,
            rbw,
            averages,
            passes,
            threshold,
            peaks,
            force,
            restore,
        } => {
            let mut v = server
                .sweep_op(mcp::SweepArgs {
                    lo_hz: lo.as_deref().map(parse_si).transpose()?,
                    hi_hz: hi.as_deref().map(parse_si).transpose()?,
                    rbw_hz: rbw,
                    averages,
                    passes: Some(passes),
                    threshold_db: threshold,
                    max_peaks: Some(peaks),
                    profile_points: Some(64),
                    force,
                    restore,
                    timeout_s: None,
                    note: None,
                })
                .await
                .map_err(op_err)?;
            if !json {
                if let Some(text) = v.get("text").and_then(Value::as_str) {
                    print!("{text}");
                    return Ok(());
                }
                if let Some(o) = v.as_object_mut() {
                    o.remove("profile");
                }
            }
            emit(&v, json)
        }
        Cmd::ClearAi => emit(
            &server
                .clear_ai_op(mcp::ClearAiArgs::default())
                .await
                .map_err(op_err)?,
            json,
        ),
        Cmd::Emitters {
            status,
            lo,
            hi,
            sort,
            limit,
        } => {
            let v = server
                .emitters_op(mcp::EmittersArgs {
                    status,
                    lo_hz: lo.as_deref().map(parse_si).transpose()?,
                    hi_hz: hi.as_deref().map(parse_si).transpose()?,
                    since_ms: None,
                    sort,
                    limit: Some(limit),
                })
                .await
                .map_err(op_err)?;
            if json {
                return emit(&v, json);
            }
            for e in v
                .get("emitters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let f = |k: &str| e.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                println!(
                    "#{:<4} {:>12.4} MHz {:>8.1} kHz {:>6.1} dBFS  {:<9} {}{}",
                    e.get("id").and_then(Value::as_u64).unwrap_or(0),
                    f("freq_hz") / 1e6,
                    f("width_hz") / 1e3,
                    f("level_dbfs"),
                    e.get("status").and_then(Value::as_str).unwrap_or(""),
                    e.get("label").and_then(Value::as_str).unwrap_or(""),
                    e.get("band")
                        .and_then(Value::as_str)
                        .map(|b| format!("  [{b}]"))
                        .unwrap_or_default(),
                );
            }
            Ok(())
        }
        Cmd::LabelEmitter {
            id,
            label,
            status,
            confidence,
            notes,
        } => emit(
            &server
                .label_emitter_op(mcp::LabelEmitterArgs {
                    id,
                    label,
                    status,
                    confidence,
                    evidence: None,
                    notes,
                })
                .await
                .map_err(op_err)?,
            json,
        ),
        Cmd::Snapshot { secs } => emit(
            &server
                .snapshot_op(mcp::SnapshotArgs { secs: Some(secs) })
                .await
                .map_err(op_err)?,
            json,
        ),
        Cmd::Spectrogram {
            lo,
            hi,
            snapshot,
            secs,
            res,
            step,
        } => {
            let v = server
                .spectrogram_op(mcp::SpectrogramArgs {
                    snapshot,
                    secs: Some(secs),
                    lo_hz: parse_si(&lo)?,
                    hi_hz: parse_si(&hi)?,
                    res_hz: Some(res),
                    step_ms: Some(step),
                })
                .await
                .map_err(op_err)?;
            print_text_or_json(&v, json)
        }
        Cmd::Envelope {
            freq,
            snapshot,
            secs,
            bw,
            window,
        } => {
            let v = server
                .envelope_op(mcp::EnvelopeArgs {
                    snapshot,
                    secs: Some(secs),
                    freq_hz: parse_si(&freq)?,
                    bw_hz: Some(bw),
                    window_ms: Some(window),
                })
                .await
                .map_err(op_err)?;
            print_text_or_json(&v, json)
        }
        Cmd::Sigwiki(SigwikiCmd::Identify {
            freq,
            bw,
            mode,
            modulation,
            limit,
        }) => {
            let v = server
                .sigwiki_identify_op(mcp::SigwikiIdentifyArgs {
                    freq_hz: parse_si(&freq)?,
                    bandwidth_hz: bw,
                    mode,
                    modulation,
                    limit: Some(limit),
                })
                .await
                .map_err(op_err)?;
            print_text_or_json(&v, json)
        }
        Cmd::Sigwiki(SigwikiCmd::Search { query, limit }) => {
            let v = server
                .sigwiki_search_op(mcp::SigwikiSearchArgs {
                    query: query.join(" "),
                    limit: Some(limit),
                })
                .await
                .map_err(op_err)?;
            if json {
                return emit(&v, true);
            }
            for r in v
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("");
                println!("{} — {} · bw {}", s("title"), s("freq"), s("bw"));
            }
            Ok(())
        }
        Cmd::Sigwiki(SigwikiCmd::Page { title, picture }) => {
            let (v, image) = server
                .sigwiki_page_op(mcp::SigwikiPageArgs {
                    title: title.join(" "),
                })
                .await
                .map_err(op_err)?;
            if let (Some(path), Some((bytes, _))) = (picture, image) {
                std::fs::write(&path, bytes)?;
                eprintln!("picture → {path}");
            }
            print_text_or_json(&v, json)
        }
        Cmd::SpotMap {
            kind,
            minutes,
            rx_grid,
        } => {
            let v = server
                .spot_map_op(mcp::SpotMapArgs {
                    kind: Some(kind),
                    minutes: Some(minutes),
                    rx_grid,
                })
                .await
                .map_err(op_err)?;
            print_text_or_json(&v, json)
        }
        Cmd::ReferenceImage { preset, out } => {
            let (mut info, bytes, mime) = server
                .reference_image_op(mcp::ReferenceImageArgs {
                    preset: preset.clone(),
                })
                .await
                .map_err(op_err)?;
            let ext = mime
                .rsplit('/')
                .next()
                .unwrap_or("img")
                .replace("jpeg", "jpg");
            let path = out.unwrap_or_else(|| format!("/tmp/ferrite-ref-{preset}.{ext}"));
            std::fs::write(&path, &bytes)?;
            info["path"] = Value::String(path);
            emit(&info, json)
        }
        Cmd::Signals { max_age_ms } => {
            let v = server
                .signals_op(SignalsArgs { max_age_ms })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Capture(c) => capture(c, server, json).await,
        Cmd::Await {
            kind,
            max,
            wait_s,
            since_seq,
            retained,
            follow,
            where_,
        } => {
            let filter = parse_where(&where_)?;
            if follow {
                return follow_records(&kind, since_seq, retained, &filter, server, json).await;
            }
            let v = server
                .await_op(AwaitArgs {
                    kind,
                    max: Some(max),
                    timeout_s: Some(wait_s),
                    since_seq,
                    include_retained: Some(retained),
                    filter: Some(filter),
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Tail { category, lookback } => tail(&category, lookback, server, json).await,
        Cmd::Decoder(DecoderCmd::Recent {
            category,
            lookback,
            since,
            limit,
        }) => {
            let v = server
                .recent_decodes_op(RecentDecodesArgs {
                    category: Some(category),
                    since_ms: since,
                    // An explicit --since supersedes the relative window.
                    lookback_secs: if since.is_some() {
                        None
                    } else {
                        Some(lookback)
                    },
                    limit: if limit == 0 { None } else { Some(limit) },
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::View { pane } => {
            let v = server
                .view_snapshot_op(ViewSnapshotArgs { pane })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::ViewGet => emit(&server.view_state_op().await.map_err(op_err)?, json),
        Cmd::ViewSet {
            main_pane,
            right_pane,
            channel_detail,
        } => {
            let v = server
                .set_view_state_op(SetViewStateArgs {
                    main_pane,
                    right_pane,
                    channel_detail_visible: channel_detail,
                    left_tab: None,
                    note: None,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::AiPrompt {
            text,
            mode,
            model,
            session,
            new,
            wait,
        } => {
            let v = server
                .http()
                .ai_prompt(
                    serde_json::json!({
                        "text": text,
                        "mode": mode,
                        "model": model,
                        "session_id": session,
                        "new_session": new,
                        "wait": wait,
                    }),
                    wait,
                )
                .await
                .map_err(op_err)?;
            if wait && !json {
                if let Some(t) = v.get("text").and_then(Value::as_str) {
                    println!("{t}");
                    eprintln!(
                        "── {} · session {}",
                        v.get("status").and_then(Value::as_str).unwrap_or("?"),
                        v.get("session_id").and_then(Value::as_str).unwrap_or("?")
                    );
                    return Ok(());
                }
            }
            emit(&v, json)
        }
        Cmd::ChatPost {
            role,
            text,
            tool_name,
            tool_input,
            tool_result,
            session,
            source,
        } => {
            let v = server
                .chat_post_op(ChatPostArgs {
                    role,
                    text: if text.is_empty() { None } else { Some(text) },
                    tool_name,
                    tool_input,
                    tool_result,
                    session,
                    source,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        // Handled in main() before the server is built.
        Cmd::GenTables | Cmd::Mcp => unreachable!("dispatched in main()"),
    }
}

/// Kick an async capture op, then wait on the job's record on the event
/// plane until it leaves `running`, and print the final job record.
async fn capture(cmd: CaptureCmd, server: &FerriteServer, json: bool) -> Result<()> {
    let (job, duration) = match cmd {
        CaptureCmd::Iq {
            freq,
            duration,
            rate,
            bw,
            gain,
            out,
            format,
            wideband,
            force,
        } => {
            let args = StartCaptureArgs {
                duration_s: duration,
                freq_hz: parse_si_opt(freq.as_deref())?,
                out,
                wideband,
                sample_rate_hz: Some(parse_si(&rate)?),
                bandwidth_hz: parse_si_opt(bw.as_deref())?,
                gain_db: gain,
                format: Some(format),
                force,
                note: None,
            };
            (
                server.start_capture_iq_op(args).await.map_err(op_err)?,
                duration,
            )
        }
        CaptureCmd::Audio {
            freq,
            duration,
            rate,
            bw,
            gain,
            preset,
            out,
            force,
        } => {
            let args = StartCaptureAudioArgs {
                freq_hz: parse_si_opt(freq.as_deref())?,
                duration_s: Some(duration),
                sample_rate_hz: Some(parse_si(&rate)?),
                bandwidth_hz: parse_si_opt(bw.as_deref())?,
                gain_db: gain,
                preset: Some(preset),
                out,
                force,
                note: None,
            };
            (
                server.start_capture_audio_op(args).await.map_err(op_err)?,
                duration,
            )
        }
        CaptureCmd::Fft {
            freq,
            duration,
            out,
        } => {
            let args = StartCaptureArgs {
                duration_s: duration,
                freq_hz: parse_si_opt(freq.as_deref())?,
                out,
                ..Default::default()
            };
            (
                server.start_capture_fft_op(args).await.map_err(op_err)?,
                duration,
            )
        }
    };

    let Some(job_id) = job.get("job_id").and_then(Value::as_str).map(str::to_owned) else {
        return emit(&job, json);
    };
    if !json {
        eprintln!("ferrite-ctl: capturing for ~{duration:.1}s (job {job_id})…");
    }
    // Wait on the job's own record (`ferrite/state/jobs/<id>`) until it
    // leaves `running` — a subscription, not a poll — with a safety
    // ceiling past the duration. The retained record covers a job that
    // finished before we subscribed.
    let mut filter = serde_json::Map::new();
    filter.insert(
        "status".into(),
        serde_json::json!(["done", "failed", "interrupted"]),
    );
    let v = server
        .await_op(AwaitArgs {
            kind: format!("jobs/{job_id}"),
            max: Some(1),
            timeout_s: Some(duration + 15.0),
            since_seq: None,
            include_retained: Some(true),
            filter: Some(filter),
        })
        .await
        .map_err(op_err)?;
    match v.pointer("/records/0/record/record/data") {
        Some(job) => emit(job, json),
        None => {
            // Timed out: report the job's current record from the store.
            let snap = server
                .decodes_op(mcp::DecodesArgs {
                    kind: Some("jobs".into()),
                })
                .await
                .map_err(op_err)?;
            let current = snap
                .pointer(&format!("/current/{job_id}/data"))
                .cloned()
                .unwrap_or(v);
            emit(&current, json)
        }
    }
}

/// `--where field=value[,value…]` → an `await_events` filter object.
fn parse_where(items: &[String]) -> Result<serde_json::Map<String, Value>> {
    let mut out = serde_json::Map::new();
    for item in items {
        let (k, v) = item
            .split_once('=')
            .ok_or_else(|| anyhow!("--where wants FIELD=VALUE, got {item:?}"))?;
        let parse = |s: &str| serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::from(s));
        let vals: Vec<Value> = v.split(',').map(parse).collect();
        let val = if vals.len() == 1 {
            vals.into_iter().next().unwrap_or(Value::Null)
        } else {
            Value::Array(vals)
        };
        out.insert(k.trim().to_string(), val);
    }
    Ok(out)
}

fn ferrite_ctl_record_matches(value: &Value, filter: &serde_json::Map<String, Value>) -> bool {
    mcp::record_matches(value, filter)
}

/// `await --follow`: one subscription, one JSON line per record, until
/// SIGINT. The event-plane form of `tail` for structured kinds.
async fn follow_records(
    kind: &str,
    since_seq: Option<u64>,
    retained: bool,
    filter: &serde_json::Map<String, Value>,
    server: &FerriteServer,
    json: bool,
) -> Result<()> {
    let since = since_seq.unwrap_or(0);
    let stream = server
        .http()
        .subscribe_records(kind, Duration::from_secs(u64::MAX / 4), |rec| {
            if (rec.retain && !retained) || rec.seq.is_some_and(|s| s <= since) {
                return true;
            }
            if !ferrite_ctl_record_matches(&rec.value, filter) {
                return true;
            }
            let line = if json {
                serde_json::json!({ "topic": rec.topic, "record": rec.value }).to_string()
            } else {
                serde_json::to_string_pretty(&rec.value).unwrap_or_default()
            };
            println!("{line}");
            true
        });
    tokio::select! {
        r = stream => { r.map_err(op_err)?; Ok(()) }
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

/// Tail log lines for a tracing-target prefix until SIGINT: optional
/// history from the daemon's log ring (`--lookback`), then a live
/// subscription to `ferrite/state/log/<target>` on the event plane.
async fn tail(category: &str, lookback: f64, server: &FerriteServer, json: bool) -> Result<()> {
    if lookback > 0.0 {
        let v = server
            .recent_decodes_op(RecentDecodesArgs {
                category: Some(category.to_string()),
                since_ms: None,
                lookback_secs: Some(lookback),
                limit: None,
            })
            .await
            .map_err(op_err)?;
        for e in v
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            print_entry(e, json);
        }
    }
    let topic = format!("log/{}", category.replace("::", "/"));
    let stream =
        server
            .http()
            .subscribe_records(&topic, Duration::from_secs(u64::MAX / 4), |rec| {
                print_entry(&rec.value, json);
                true
            });
    tokio::select! {
        r = stream => { r.map_err(op_err)?; Ok(()) }
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

fn print_entry(e: &Value, json: bool) {
    if json {
        if let Ok(s) = serde_json::to_string(e) {
            println!("{s}");
        }
    } else {
        let target = e.get("target").and_then(Value::as_str).unwrap_or("?");
        let level = e.get("level").and_then(Value::as_str).unwrap_or("?");
        let msg = e.get("message").and_then(Value::as_str).unwrap_or("");
        println!("[{level}] {target}: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_si_handles_suffixes() {
        assert!((parse_si("100M").unwrap() - 100e6).abs() < f64::EPSILON);
        assert!((parse_si("915k").unwrap() - 915e3).abs() < f64::EPSILON);
        assert!((parse_si("2.4M").unwrap() - 2.4e6).abs() < 1.0);
        assert!((parse_si("144390000").unwrap() - 144_390_000.0).abs() < f64::EPSILON);
        assert!(parse_si("").is_err());
    }
}
