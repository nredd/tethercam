//! Counts-only sampler for an unattended run of an installed service.
//!
//! Each iteration reads the authenticated status endpoint with `curl` and plays the primary RTSP
//! stream for one segment with `ffprobe`, the same external clients and protected client inputs
//! as `verify`. The report holds only counts, durations and the battery trajectory: no device
//! identity, address, credential or payload. It is replaced atomically after every iteration, so
//! a run that dies still leaves its last complete report. `SIGINT` and `SIGTERM` end the run
//! cleanly: the child is reaped, the report finished and the credential-bearing inputs removed.
//!
//! References:
//! - <https://curl.se/docs/manpage.html>
//! - <https://ffmpeg.org/ffprobe.html>

use super::{Error, args, capture_until, clients};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tethercam::onvif::media::STATUS_PATH;
use tokio::signal::unix::{SignalKind, signal};

/// The run counters of the status document, in report order.
const COUNTERS: [&str; 8] = [
    "frames_fetched",
    "frames_served",
    "frames_dropped",
    "reconnects",
    "session_restarts",
    "connect_misses",
    "snapshots_served",
    "snapshots_refused",
];
/// A week. Longer runs should be split so the report stays reviewable.
const MAX_DURATION_SECS: u64 = 7 * 24 * 3600;
/// An hour: a segment is one `ffprobe` run, and one packet list is read into memory.
const MAX_SEGMENT_SECS: u64 = 3600;
const MAX_GAP_MS: u64 = 60_000;
/// Largest client output read back.
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
/// Battery changes kept. Further changes are counted, not stored.
const MAX_BATTERY_POINTS: usize = 1000;
const STATUS_BUDGET: Duration = Duration::from_secs(15);
/// `ffprobe` connects for up to 15 s before it plays, and a stream that stalls mid-segment ends
/// after about 30 s of silence, with exit status 0 and the packets read so far (measured with
/// ffmpeg 9.0 against a stopped simulator).
const STREAM_SLACK: Duration = Duration::from_secs(60);
/// Longest wait from starting `ffprobe` to its first packet that is still a normal start.
const STARTUP_ALLOWANCE_US: u64 = 5_000_000;

/// What to run and for how long.
#[derive(Clone, Copy)]
struct Plan {
    duration: Duration,
    segment: Duration,
    gap_ms: u64,
}

/// The external clients.
struct Tools<'a> {
    ffprobe: &'a Path,
    curl: &'a Path,
}

/// One reading of the status endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sample {
    ready: bool,
    battery_percent: Option<u64>,
    power: Option<String>,
    uptime_secs: u64,
    run_id: String,
    counters: [u64; 8],
}

fn parse_sample(text: &str) -> Result<Sample, Error> {
    let document: Value = serde_json::from_str(text)?;
    let number = |key: &str| document.get(key).and_then(Value::as_u64);
    let camera = document
        .get("camera")
        .and_then(Value::as_str)
        .ok_or(Error::Invalid("status lacks the camera state"))?;
    let mut counters = [0; 8];
    for (slot, key) in counters.iter_mut().zip(COUNTERS) {
        *slot = number(key).ok_or(Error::Invalid("status lacks the run counters"))?;
    }
    Ok(Sample {
        ready: camera == "ready",
        battery_percent: number("battery_percent"),
        power: document
            .get("power")
            .and_then(Value::as_str)
            .map(str::to_owned),
        uptime_secs: number("uptime_secs").ok_or(Error::Invalid("status lacks the uptime"))?,
        run_id: document
            .get("run_id")
            .and_then(Value::as_str)
            .filter(|id| (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned)
            .ok_or(Error::Invalid("status lacks the run identifier"))?,
        counters,
    })
}

/// A decimal number of seconds as whole microseconds. Rejects signs, exponents and non-digits.
fn micros(text: &str) -> Option<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) {
        return None;
    }
    let fraction: String = fraction
        .chars()
        .chain(std::iter::repeat('0'))
        .take(6)
        .collect();
    whole
        .parse::<u64>()
        .ok()
        .and_then(|whole| whole.checked_mul(1_000_000))
        .zip(fraction.parse::<u64>().ok())
        .and_then(|(whole, fraction)| whole.checked_add(fraction))
}

/// Packet arrival times, in microseconds, from `ffprobe -show_packets -of json`.
fn parse_packets(text: &str) -> Result<Vec<u64>, Error> {
    let document: Value = serde_json::from_str(text)?;
    let packets = document
        .get("packets")
        .and_then(Value::as_array)
        .ok_or(Error::Invalid("stream output lacks a packet list"))?;
    let times = packets
        .iter()
        // RTCP sender reports and reference-time side data arrive as packets without a time.
        .filter_map(|packet| packet.get("pts_time"))
        .map(|time| time.as_str().and_then(micros))
        .collect::<Option<Vec<_>>>()
        .ok_or(Error::Invalid("stream packet has no usable time"))?;
    if times.is_empty() {
        return Err(Error::Invalid("stream segment carried no packets"));
    }
    Ok(times)
}

/// One `ffprobe` run: packet arrival times and the wall-clock span of the run, in microseconds
/// since the epoch, the same clock `-use_wallclock_as_timestamps` uses.
struct Segment {
    times: Vec<u64>,
    started_us: u64,
    ended_us: u64,
    /// `ffprobe` exited successfully.
    clean: bool,
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_micros()).unwrap_or(0))
}

/// Everything the report says. Counts only.
#[derive(Default)]
struct Report {
    samples: u64,
    status_failures: u64,
    ready_samples: u64,
    process_restarts: u64,
    previous: Option<Sample>,
    totals: [u64; 8],
    segments: u64,
    segments_failed: u64,
    packets: u64,
    covered_us: u64,
    gaps: u64,
    max_gap_us: u64,
    battery: Vec<Value>,
    battery_dropped: u64,
    last_battery: Option<(Option<u64>, Option<String>)>,
}

impl Report {
    fn status_failed(&mut self) {
        self.status_failures += 1;
    }

    /// Add a reading. A different `run_id` is a restart, which restarts the counts: the totals
    /// add what the new process counted rather than a negative difference. The uptime and the
    /// wall clock play no part, so a clock step cannot look like a restart.
    fn status(&mut self, elapsed: Duration, sample: Sample) {
        self.samples += 1;
        self.ready_samples += u64::from(sample.ready);
        let restarted = self
            .previous
            .as_ref()
            .is_some_and(|previous| previous.run_id != sample.run_id);
        self.process_restarts += u64::from(restarted);
        let before = self
            .previous
            .as_ref()
            .map_or([0; 8], |previous| previous.counters);
        for ((total, now), before) in self.totals.iter_mut().zip(sample.counters).zip(before) {
            *total += if restarted {
                now
            } else {
                now.saturating_sub(before)
            };
        }
        let battery = (sample.battery_percent, sample.power.clone());
        if self.last_battery.as_ref() != Some(&battery) {
            if self.battery.len() < MAX_BATTERY_POINTS {
                self.battery.push(json!({
                    "t_secs": elapsed.as_secs(),
                    "percent": battery.0,
                    "power": battery.1,
                }));
            } else {
                self.battery_dropped += 1;
            }
            self.last_battery = Some(battery);
        }
        self.previous = Some(sample);
    }

    fn segment_failed(&mut self) {
        self.segments += 1;
        self.segments_failed += 1;
    }

    /// Add a segment. A gap is silence longer than the threshold: between consecutive packets,
    /// and from the last packet to the moment `ffprobe` ended (a stream that froze or died
    /// mid-segment). The wait for the first packet counts only beyond the startup allowance.
    /// A segment whose `ffprobe` failed keeps its packets and also counts as failed.
    fn segment(&mut self, segment: &Segment, gap_ms: u64) {
        self.segments += 1;
        self.segments_failed += u64::from(!segment.clean);
        self.packets += u64::try_from(segment.times.len()).unwrap_or(u64::MAX);
        let (Some(first), Some(last)) = (segment.times.first(), segment.times.last()) else {
            return;
        };
        self.covered_us += last.saturating_sub(*first);
        let threshold = gap_ms * 1000;
        let between = segment.times.iter().zip(segment.times.iter().skip(1));
        let inner = between.map(|(before, after)| (after.saturating_sub(*before), threshold));
        let head = (
            first.saturating_sub(segment.started_us),
            STARTUP_ALLOWANCE_US,
        );
        let tail = (segment.ended_us.saturating_sub(*last), threshold);
        for (gap, allowed) in inner.chain([head, tail]) {
            if gap > allowed {
                self.gaps += 1;
                self.max_gap_us = self.max_gap_us.max(gap);
            }
        }
    }

    fn json(&self, plan: &Plan, elapsed: Duration) -> Value {
        let totals: serde_json::Map<String, Value> = COUNTERS
            .iter()
            .zip(self.totals)
            .map(|(key, total)| ((*key).to_owned(), Value::from(total)))
            .collect();
        json!({
            "elapsed_secs": elapsed.as_secs(),
            "planned_secs": plan.duration.as_secs(),
            "segment_secs": plan.segment.as_secs(),
            "gap_threshold_ms": plan.gap_ms,
            "status": {
                "samples": self.samples,
                "failures": self.status_failures,
                "camera_ready_samples": self.ready_samples,
                "process_restarts_seen": self.process_restarts,
                "last_uptime_secs": self.previous.as_ref().map(|sample| sample.uptime_secs),
                "totals": totals,
            },
            "stream": {
                "segments": self.segments,
                "segments_failed": self.segments_failed,
                "packets": self.packets,
                "covered_secs": self.covered_us / 1_000_000,
                "gaps_over_threshold": self.gaps,
                "max_gap_ms": self.max_gap_us / 1000,
            },
            "battery": {
                "points": self.battery,
                "points_dropped": self.battery_dropped,
            },
        })
    }
}

fn read_capped(path: &Path) -> Result<String, Error> {
    let mut text = String::new();
    fs::File::open(path)
        .and_then(|file| file.take(MAX_OUTPUT).read_to_string(&mut text))
        .map(|_| text)
        .map_err(Error::from)
}

/// Replace the report, mode `0600`: write a sibling temporary file, sync it, rename it over the
/// report and sync the directory, so a crash leaves the previous complete report or the new one,
/// never an empty file.
fn write_report(path: &Path, report: &Value) -> Result<(), Error> {
    let temporary = path.with_extension("json.tmp");
    let _ = fs::remove_file(&temporary);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let text = format!("{report:#}\n");
    opts.open(&temporary)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
        })
        .and_then(|()| fs::rename(&temporary, path))
        .and_then(|()| {
            path.parent().map_or(Ok(()), |dir| {
                fs::File::open(dir).and_then(|dir| dir.sync_all())
            })
        })
        .map_err(Error::from)
}

fn elapsed_since(start: SystemTime) -> Duration {
    // The wall clock, deliberately: a monotonic clock stops while the host sleeps.
    start.elapsed().unwrap_or_default()
}

/// Sleep up to `total`, waking early once `stop` is set.
fn nap(total: Duration, stop: &AtomicBool) {
    let end = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// The credential-bearing client inputs. Removed explicitly on the normal path, and by `Drop` on
/// any other exit, including a panic. A `SIGKILL` skips both and leaves the directory.
struct ClientDir {
    path: PathBuf,
    removed: bool,
}

impl ClientDir {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            removed: false,
        }
    }

    fn remove(&mut self) -> io::Result<()> {
        self.removed = true;
        fs::remove_dir_all(&self.path)
    }
}

impl Drop for ClientDir {
    fn drop(&mut self) {
        if !self.removed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

type RuntimeBuilder = fn() -> io::Result<tokio::runtime::Runtime>;

fn current_thread_runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// Wait for either signal, then set `stop`. Reports registration back through `ready`.
async fn listen(
    first: SignalKind, second: SignalKind, stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<io::Result<()>>,
) {
    match (signal(first), signal(second)) {
        (Ok(mut first), Ok(mut second)) => {
            let _ = ready.send(Ok(()));
            tokio::select! {
                _ = first.recv() => {},
                _ = second.recv() => {},
            }
            stop.store(true, Ordering::Relaxed);
        }
        (Err(error), _) | (_, Err(error)) => {
            let _ = ready.send(Err(error));
        }
    }
}

/// Set `stop` when either signal arrives. Returns once both handlers are installed, so no signal
/// between here and the first check is missed. The listener thread lives until the process ends.
fn watch_signals(
    build: RuntimeBuilder, first: SignalKind, second: SignalKind, stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let (ready, installed) = std::sync::mpsc::channel();
    std::thread::spawn(move || match build() {
        Ok(runtime) => runtime.block_on(listen(first, second, stop, ready)),
        Err(error) => {
            let _ = ready.send(Err(error));
        }
    });
    installed.recv().unwrap_or(Ok(()))
}

/// Where one iteration runs and keeps its scratch files.
struct Run<'a> {
    tools: &'a Tools<'a>,
    client_dir: &'a Path,
    base: &'a str,
    output: &'a Path,
    stop: &'a Arc<AtomicBool>,
}

impl Run<'_> {
    fn scratch(&self, kind: &str, iteration: u64) -> PathBuf {
        self.output.join(format!("{kind}-{iteration}"))
    }

    fn status(&self, iteration: u64) -> Result<Sample, Error> {
        let code = self.scratch("status-code", iteration);
        let body = self.scratch("status-body", iteration);
        let mut cmd = args(&["--config"]);
        cmd.push(self.client_dir.join("http-client.conf").into());
        cmd.extend(args(&["--max-time", "10", "-o"]));
        cmd.push(body.clone().into());
        cmd.extend(args(&["-w", "%{http_code}"]));
        cmd.push(format!("{}{STATUS_PATH}", self.base).into());
        let result = capture_until(self.tools.curl, &cmd, &code, STATUS_BUDGET, Some(self.stop))
            .and_then(|()| fs::read_to_string(&code).map_err(Error::from))
            .and_then(|status| {
                if status == "200" {
                    read_capped(&body)
                } else {
                    Err(Error::Invalid("status endpoint did not answer 200"))
                }
            })
            .and_then(|text| parse_sample(&text));
        let _ = fs::remove_file(code);
        let _ = fs::remove_file(body);
        result
    }

    fn stream(&self, iteration: u64, segment: Duration) -> Result<Segment, Error> {
        let out = self.scratch("stream", iteration);
        let mut cmd = args(&[
            "-v",
            "quiet",
            "-rtsp_transport",
            "tcp",
            "-timeout",
            "15000000",
            "-use_wallclock_as_timestamps",
            "1",
            "-read_intervals",
        ]);
        cmd.push(format!("%+{}", segment.as_secs()).into());
        cmd.extend(args(&[
            "-show_packets",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "json",
            "-/i",
        ]));
        cmd.push(self.client_dir.join("rtsp-main.url").into());
        let started_us = now_us();
        let ran = capture_until(
            self.tools.ffprobe,
            &cmd,
            &out,
            segment + STREAM_SLACK,
            Some(self.stop),
        );
        let ended_us = now_us();
        // Read the output even when `ffprobe` failed: what it saw before failing still counts.
        let parsed = read_capped(&out).and_then(|text| parse_packets(&text));
        let _ = fs::remove_file(out);
        parsed.map(|times| Segment {
            times,
            started_us,
            ended_us,
            clean: ran.is_ok(),
        })
    }
}

fn soak(
    tools: &Tools<'_>, cfg: &Path, output: &Path, plan: Plan, stop: &Arc<AtomicBool>,
) -> Result<(), Error> {
    for path in [tools.ffprobe, tools.curl, cfg, output] {
        if !path.is_absolute() {
            return Err(Error::Invalid("soak paths must be absolute"));
        }
    }
    let mut directory = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(output)?;
    let mut client_dir = ClientDir::new(output.join("clients"));
    let result = clients::prepare_host(cfg, &client_dir.path).and_then(|base| {
        let run = Run {
            tools,
            client_dir: &client_dir.path,
            base: &base,
            output,
            stop,
        };
        sample_until_done(&run, plan)
    });
    // The client inputs carry credentials, also after a partial preparation; the report must
    // be safe to share. A failure that left nothing to remove is the failure already in hand.
    let cleaned = client_dir.remove();
    result.and(cleaned.map_err(Error::from))
}

fn sample_until_done(run: &Run<'_>, plan: Plan) -> Result<(), Error> {
    let start = SystemTime::now();
    let report_path = run.output.join("report.json");
    let mut report = Report::default();
    for iteration in 1_u64.. {
        let began = SystemTime::now();
        // A run cut short by a signal drops the reading it interrupted, then reports.
        if run.stop.load(Ordering::Relaxed) {
            break;
        }
        let sample = run.status(iteration);
        let remaining = plan.duration.saturating_sub(elapsed_since(start));
        let segment = plan.segment.min(remaining).max(Duration::from_secs(1));
        let stream = if remaining.is_zero() {
            None
        } else {
            Some(run.stream(iteration, segment))
        };
        if run.stop.load(Ordering::Relaxed) {
            break;
        }
        match sample {
            Ok(sample) => report.status(elapsed_since(start), sample),
            Err(_) => report.status_failed(),
        }
        match stream {
            Some(Ok(segment)) => report.segment(&segment, plan.gap_ms),
            Some(Err(_)) => report.segment_failed(),
            None => break,
        }
        write_report(&report_path, &report.json(&plan, elapsed_since(start)))?;
        // A segment that fails fast (no camera yet) must not turn into a busy loop.
        let period_left = plan.segment.saturating_sub(elapsed_since(began));
        nap(
            period_left.min(plan.duration.saturating_sub(elapsed_since(start))),
            run.stop,
        );
    }
    write_report(&report_path, &report.json(&plan, elapsed_since(start)))
}

fn number(value: &OsString, max: u64) -> Result<u64, Error> {
    value
        .to_str()
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|number| (1..=max).contains(number))
        .ok_or(Error::Usage)
}

/// `soak CONFIG NEW_OUTPUT_DIRECTORY FFPROBE CURL DURATION_SECS SEGMENT_SECS GAP_MS`
pub(in super::super) fn run(values: &[OsString]) -> Result<(), Error> {
    let [cfg, output, ffprobe, curl, duration, segment, gap] = values else {
        return Err(Error::Usage);
    };
    let plan = Plan {
        duration: Duration::from_secs(number(duration, MAX_DURATION_SECS)?),
        segment: Duration::from_secs(number(segment, MAX_SEGMENT_SECS)?),
        gap_ms: number(gap, MAX_GAP_MS)?,
    };
    let stop = Arc::new(AtomicBool::new(false));
    watch_signals(
        current_thread_runtime,
        SignalKind::interrupt(),
        SignalKind::terminate(),
        Arc::clone(&stop),
    )
    .map_err(Error::from)
    .and_then(|()| {
        soak(
            &Tools {
                ffprobe: Path::new(ffprobe),
                curl: Path::new(curl),
            },
            Path::new(cfg),
            Path::new(output),
            plan,
            &stop,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const STATUS: &str = r#"{"camera":"ready","battery_percent":80,"power":"usb","uptime_secs":5,"frames_fetched":10,"frames_served":9,"frames_dropped":1,"reconnects":0,"session_restarts":0,"connect_misses":2,"snapshots_served":0,"snapshots_refused":0,"run_id":"00000000000000aa"}"#;

    fn sample(run_id: &str, uptime: u64, fetched: u64) -> Sample {
        let mut counters = [0; 8];
        counters[0] = fetched;
        Sample {
            ready: true,
            battery_percent: Some(80),
            power: Some("usb".into()),
            uptime_secs: uptime,
            run_id: run_id.into(),
            counters,
        }
    }

    fn stop(set: bool) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(set))
    }

    proptest! {
        #[test]
        fn tool_output_parsers_never_panic_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let text = String::from_utf8_lossy(&bytes);
            let _ = parse_sample(&text);
            let _ = parse_packets(&text);
            let _ = micros(&text);
        }

        #[test]
        fn decimal_text_never_panics_and_round_trips_whole_seconds(whole in 0_u64..1_000_000, frac in "[0-9]{0,9}") {
            let text = format!("{whole}.{frac}");
            let got = micros(&text).expect("digits parse");
            prop_assert_eq!(got / 1_000_000, whole);
        }
    }

    #[test]
    fn status_documents_parse_and_every_missing_part_is_refused() {
        let ready = parse_sample(STATUS).expect("status");
        assert_eq!(ready.counters, [10, 9, 1, 0, 0, 2, 0, 0]);
        assert_eq!(ready.run_id, "00000000000000aa");
        assert_eq!(
            (ready.ready, ready.battery_percent, ready.power.as_deref()),
            (true, Some(80), Some("usb"))
        );
        let waiting = STATUS
            .replace("ready", "waiting")
            .replace(r#""battery_percent":80,"power":"usb","#, "");
        let waiting = parse_sample(&waiting).expect("waiting");
        assert_eq!(
            (waiting.ready, waiting.battery_percent, waiting.power),
            (false, None, None)
        );
        for broken in [
            "not json".to_owned(),
            STATUS.replace(r#""camera":"ready","#, ""),
            STATUS.replace("frames_dropped", "dropped"),
            STATUS.replace("uptime_secs", "up"),
            STATUS.replace("run_id", "id"),
            STATUS.replace("00000000000000aa", ""),
            STATUS.replace("00000000000000aa", "not-hex"),
            STATUS.replace("00000000000000aa", &"a".repeat(33)),
        ] {
            assert!(parse_sample(&broken).is_err(), "{broken}");
        }
    }

    #[test]
    fn decimal_seconds_become_microseconds_without_floats() {
        for (text, want) in [
            ("2", Some(2_000_000)),
            ("1.5", Some(1_500_000)),
            ("0.1234567", Some(123_456)),
            ("1731510000.000001", Some(1_731_510_000_000_001)),
            ("", None),
            (".5", None),
            ("-1", None),
            ("+1", None),
            ("1e3", None),
            ("1.x", None),
            ("1.+5", None),
            ("99999999999999999999", None),
            ("18446744073710", None),
        ] {
            assert_eq!(micros(text), want, "{text}");
        }
    }

    #[test]
    fn packet_lists_are_total() {
        assert_eq!(
            parse_packets(r#"{"packets":[{"pts_time":"1.5"},{"pts_time":"2"}]}"#).expect("ok"),
            [1_500_000, 2_000_000]
        );
        // Packets that carry only side data have no time and are not media.
        assert_eq!(
            parse_packets(r#"{"packets":[{"side_data_list":[]},{"pts_time":"3"}]}"#).expect("ok"),
            [3_000_000]
        );
        for broken in [
            "[]",
            "not json",
            r#"{"packets":[]}"#,
            r#"{"packets":[{"size":"1"}]}"#,
            r#"{"packets":[{"pts_time":"1"},{"pts_time":"x"}]}"#,
            r#"{"packets":[{"pts_time":"x"}]}"#,
            r#"{"packets":[{"pts_time":1.5}]}"#,
        ] {
            assert!(parse_packets(broken).is_err(), "{broken}");
        }
    }

    #[test]
    fn counter_totals_survive_a_restart_but_not_a_clock_step() {
        let mut report = Report::default();
        let at = Duration::from_secs;
        report.status(at(0), sample("a1", 10, 100));
        report.status(at(60), sample("a1", 70, 160));
        // A new run identifier is a restart, whatever the counters and the uptime say.
        report.status(at(120), sample("b2", 5, 7));
        // The same run with a smaller uptime is a clock step on the host, not a restart, and a
        // counter never goes negative.
        report.status(at(180), sample("b2", 1, 7));
        // A restart whose counters and uptime are higher than before is still seen.
        report.status(at(240), sample("c3", 9_000, 50_000));
        assert_eq!(report.process_restarts, 2);
        assert_eq!(report.totals[0], 100 + 60 + 7 + 50_000);
        assert_eq!(report.samples, 5);
        assert_eq!(report.ready_samples, 5);
        report.status_failed();
        assert_eq!(report.status_failures, 1);
    }

    #[test]
    fn the_battery_trajectory_records_changes_only_and_is_bounded() {
        let mut report = Report::default();
        let at = Duration::from_secs;
        let mut level = sample("a1", 1, 0);
        for (second, percent) in [(0, Some(90)), (60, Some(90)), (120, Some(89)), (180, None)] {
            level.battery_percent = percent;
            report.status(at(second), level.clone());
        }
        let points: Vec<_> = report
            .battery
            .iter()
            .map(|point| (point["t_secs"].as_u64(), point["percent"].as_u64()))
            .collect();
        assert_eq!(
            points,
            [
                (Some(0), Some(90)),
                (Some(120), Some(89)),
                (Some(180), None)
            ]
        );
        for percent in 0..=100 {
            for second in 0..11 {
                level.battery_percent = Some(percent);
                level.power = Some(format!("p{second}"));
                report.status(at(300), level.clone());
            }
        }
        assert_eq!(report.battery.len(), MAX_BATTERY_POINTS);
        assert!(report.battery_dropped > 0);
    }

    fn segment(times: &[u64], started_us: u64, ended_us: u64, clean: bool) -> Segment {
        Segment {
            times: times.to_vec(),
            started_us,
            ended_us,
            clean,
        }
    }

    #[test]
    fn segments_count_gaps_between_packets_at_the_head_and_at_the_tail() {
        let plan = Plan {
            duration: Duration::from_secs(60),
            segment: Duration::from_secs(10),
            gap_ms: 500,
        };
        // Healthy: first packet 0.1 s after the start, 0.1 s steps, ended 0.1 s after the last.
        let mut report = Report::default();
        report.segment(
            &segment(&[100_000, 200_000, 300_000], 0, 400_000, true),
            500,
        );
        assert_eq!((report.gaps, report.max_gap_us), (0, 0));
        // A 0.9 s pause between packets.
        report.segment(
            &segment(
                &[0, 100_000, 200_000, 1_100_000, 1_200_000],
                0,
                1_300_000,
                true,
            ),
            500,
        );
        assert_eq!((report.gaps, report.max_gap_us), (1, 900_000));
        // The stream froze mid-segment: the last packet is long before ffprobe gave up.
        report.segment(&segment(&[100_000, 200_000], 0, 30_200_000, true), 500);
        assert_eq!((report.gaps, report.max_gap_us), (2, 30_000_000));
        // A slow start beyond the startup allowance is a gap; within it, it is not.
        report.segment(&segment(&[4_900_000, 5_000_000], 0, 5_100_000, true), 500);
        assert_eq!(report.gaps, 2);
        report.segment(&segment(&[5_100_000, 5_200_000], 0, 5_300_000, true), 500);
        assert_eq!(report.gaps, 3);
        // A failed ffprobe keeps what it read, counts as failed, and its tail still counts.
        report.segment(&segment(&[0, 100_000], 0, 2_000_000, false), 500);
        assert_eq!(
            (report.segments, report.segments_failed, report.gaps),
            (6, 1, 4)
        );
        assert_eq!(report.packets, 3 + 5 + 2 + 2 + 2 + 2);
        // No packets at all: nothing to measure.
        report.segment(&segment(&[], 0, 1, true), 500);
        report.segment_failed();
        assert_eq!((report.segments, report.segments_failed), (8, 2));
        let json = report.json(&plan, Duration::from_secs(61));
        assert_eq!(json["stream"]["max_gap_ms"], 30_000);
        assert_eq!(json["stream"]["gaps_over_threshold"], 4);
        assert_eq!(json["status"]["last_uptime_secs"], Value::Null);
        assert_eq!(json["elapsed_secs"], 61);
        assert_eq!(json["gap_threshold_ms"], 500);
        assert!(json["stream"]["covered_secs"].as_u64().is_some());
    }

    #[test]
    fn arguments_are_bounded_before_any_file_or_process_is_touched() {
        let temp = tempfile::tempdir().expect("temp");
        let out = temp.path().join("never");
        let values = |duration: &str, segment: &str, gap: &str| -> Vec<OsString> {
            [
                "/cfg",
                out.to_str().expect("utf8"),
                "/ffprobe",
                "/curl",
                duration,
                segment,
                gap,
            ]
            .into_iter()
            .map(OsString::from)
            .collect()
        };
        assert!(run(&[]).is_err());
        for (duration, segment, gap) in [
            ("0", "1", "1"),
            ("604801", "1", "1"),
            ("10", "0", "1"),
            ("10", "3601", "1"),
            ("10", "1", "0"),
            ("10", "1", "60001"),
            ("ten", "1", "1"),
        ] {
            assert!(run(&values(duration, segment, gap)).is_err());
        }
        // Valid numbers reach the config load, which fails: no sampling, no report.
        assert!(run(&values("10", "1", "500")).is_err());
        assert!(!out.join("report.json").exists());
        let non_utf8 = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(vec![0xff])
        };
        assert!(number(&non_utf8, 5).is_err());
    }

    fn plan(seconds: u64) -> Plan {
        Plan {
            duration: Duration::from_secs(seconds),
            segment: Duration::from_secs(1),
            gap_ms: 500,
        }
    }

    fn report(output: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(output.join("report.json")).expect("report"))
            .expect("json")
    }

    fn copy_as(program: &Path, dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::copy(program, &path).expect("copy fixture");
        path
    }

    #[test]
    fn a_short_run_samples_status_and_stream_and_leaves_no_credentials_behind() {
        let (temp, program, cfg) = super::super::tests::fixture();
        let curl = copy_as(&program, temp.path(), "status-curl");
        let output = temp.path().join("soak");
        soak(
            &Tools {
                ffprobe: &program,
                curl: &curl,
            },
            &cfg,
            &output,
            plan(2),
            &stop(false),
        )
        .expect("soak");
        let json = report(&output);
        assert!(json["status"]["samples"].as_u64().expect("samples") >= 2);
        assert_eq!(json["status"]["failures"], 0);
        assert_eq!(json["status"]["process_restarts_seen"], 0);
        assert_eq!(json["status"]["totals"]["frames_fetched"], 10);
        assert_eq!(json["status"]["totals"]["connect_misses"], 2);
        assert_eq!(
            json["battery"]["points"].as_array().expect("points").len(),
            1
        );
        assert_eq!(json["battery"]["points"][0]["power"], "usb");
        // The fixture's packets are 0.9 s apart, then the run ends at once: one gap per segment.
        let stream = &json["stream"];
        assert!(stream["gaps_over_threshold"].as_u64().expect("gaps") >= 1);
        assert!(stream["max_gap_ms"].as_u64().expect("max") >= 900);
        assert!(stream["max_gap_ms"].as_u64().expect("max") < 5_000);
        assert_eq!(stream["segments_failed"], 0);
        assert!(!output.join("clients").exists());
        assert!(!output.join("report.json.tmp").exists());
        let text = fs::read_to_string(output.join("report.json")).expect("text");
        let secret = tethercam::config::Config::load(&cfg).expect("cfg");
        assert!(!text.contains(secret.credentials.password.expose()));
        assert!(!text.contains(&secret.endpoint_uuid.to_string()));
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(output.join("report.json")).expect("meta");
            assert_eq!(mode.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_stream_that_went_silent_before_the_segment_ended_is_a_tail_gap() {
        let (temp, program, cfg) = super::super::tests::fixture();
        let curl = copy_as(&program, temp.path(), "status-curl");
        // The fixture reports packets 3.0 s and 2.9 s before it exits: silence at the end.
        let silent = copy_as(&program, temp.path(), "tail-probe");
        let output = temp.path().join("tail");
        soak(
            &Tools {
                ffprobe: &silent,
                curl: &curl,
            },
            &cfg,
            &output,
            plan(1),
            &stop(false),
        )
        .expect("soak");
        let stream = &report(&output)["stream"];
        assert!(stream["gaps_over_threshold"].as_u64().expect("gaps") >= 1);
        assert!(stream["max_gap_ms"].as_u64().expect("max") >= 2_900);
        assert_eq!(stream["segments_failed"], 0);
    }

    #[test]
    fn a_failed_ffprobe_keeps_the_packets_it_read() {
        let (temp, program, cfg) = super::super::tests::fixture();
        let curl = copy_as(&program, temp.path(), "status-curl");
        let partial = copy_as(&program, temp.path(), "partial-tail-probe");
        let output = temp.path().join("partial");
        soak(
            &Tools {
                ffprobe: &partial,
                curl: &curl,
            },
            &cfg,
            &output,
            plan(1),
            &stop(false),
        )
        .expect("soak");
        let stream = &report(&output)["stream"];
        assert_eq!(stream["segments_failed"], stream["segments"]);
        assert!(stream["packets"].as_u64().expect("packets") >= 2);
        assert!(stream["gaps_over_threshold"].as_u64().expect("gaps") >= 1);
    }

    #[test]
    fn failing_clients_are_counted_not_fatal() {
        let (temp, program, cfg) = super::super::tests::fixture();
        let bad_status = copy_as(&program, temp.path(), "bad200");
        for (name, curl, ffprobe, stream_fails) in [
            (
                "refused",
                Path::new("/usr/bin/false"),
                Path::new("/usr/bin/false"),
                true,
            ),
            (
                "not-ok",
                bad_status.as_path(),
                Path::new("/usr/bin/true"),
                true,
            ),
            (
                "empty",
                Path::new("/usr/bin/true"),
                program.as_path(),
                false,
            ),
        ] {
            let output = temp.path().join(name);
            soak(
                &Tools { ffprobe, curl },
                &cfg,
                &output,
                plan(1),
                &stop(false),
            )
            .expect(name);
            let json = report(&output);
            assert_eq!(json["status"]["samples"], 0, "{name}");
            assert!(json["status"]["failures"].as_u64().expect("failures") >= 1);
            let stream = &json["stream"];
            assert!(
                stream["segments"].as_u64().expect("segments") >= 1,
                "{name}"
            );
            assert_eq!(
                stream["segments_failed"] == stream["segments"],
                stream_fails,
                "{name}"
            );
        }
    }

    #[test]
    fn unusable_inputs_fail_before_any_sampling() {
        let (temp, program, cfg) = super::super::tests::fixture();
        let tools = Tools {
            ffprobe: &program,
            curl: &program,
        };
        let relative = Tools {
            ffprobe: Path::new("ffprobe"),
            curl: &program,
        };
        let never = stop(false);
        assert!(soak(&relative, &cfg, &temp.path().join("a"), plan(1), &never).is_err());
        assert!(
            soak(
                &tools,
                Path::new("config.toml"),
                &temp.path().join("b"),
                plan(1),
                &never
            )
            .is_err()
        );
        assert!(soak(&tools, &cfg, Path::new("out"), plan(1), &never).is_err());
        // Existing output, missing config: never overwritten, never half-started.
        assert!(soak(&tools, &cfg, temp.path(), plan(1), &never).is_err());
        assert!(
            soak(
                &tools,
                &temp.path().join("missing.toml"),
                &temp.path().join("c"),
                plan(1),
                &never
            )
            .is_err()
        );
    }

    #[test]
    fn an_unwritable_report_ends_the_run_with_an_error() {
        let temp = tempfile::tempdir().expect("temp");
        let tools = Tools {
            ffprobe: Path::new("/usr/bin/false"),
            curl: Path::new("/usr/bin/false"),
        };
        let missing = temp.path().join("missing");
        let never = stop(false);
        let run = Run {
            tools: &tools,
            client_dir: &missing,
            base: "http://127.0.0.1:1",
            output: &missing,
            stop: &never,
        };
        assert!(sample_until_done(&run, plan(2)).is_err());
    }

    #[test]
    fn a_report_is_replaced_atomically_even_past_a_stale_temporary_file() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("report.json");
        fs::write(path.with_extension("json.tmp"), "stale").expect("stale");
        write_report(&path, &json!({"n": 1})).expect("first");
        write_report(&path, &json!({"n": 2})).expect("second");
        assert_eq!(report(temp.path())["n"], 2);
        assert!(!path.with_extension("json.tmp").exists());
        // A directory where the report belongs makes the rename fail, and the old name stays.
        let blocked = temp.path().join("dir.json");
        fs::create_dir(&blocked).expect("dir");
        assert!(write_report(&blocked, &json!({})).is_err());
        assert!(blocked.is_dir());
    }

    #[test]
    fn a_signal_before_the_first_check_ends_the_run_with_a_report_and_no_credentials() {
        let (temp, program, cfg) = super::super::tests::fixture();
        let output = temp.path().join("preset");
        soak(
            &Tools {
                ffprobe: &program,
                curl: &program,
            },
            &cfg,
            &output,
            plan(5),
            &stop(true),
        )
        .expect("soak");
        assert_eq!(report(&output)["status"]["samples"], 0);
        assert!(!output.join("clients").exists());
    }

    #[test]
    fn a_signal_during_a_segment_kills_and_reaps_ffprobe_and_still_reports() {
        let (temp, program, cfg) = super::super::tests::fixture();
        let curl = copy_as(&program, temp.path(), "status-curl");
        // This fixture sleeps for 30 s instead of printing packets.
        let hang = copy_as(&program, temp.path(), "hang-probe");
        let output = temp.path().join("interrupted");
        let flag = stop(false);
        let setter = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            setter.store(true, Ordering::Relaxed);
        });
        let began = Instant::now();
        soak(
            &Tools {
                ffprobe: &hang,
                curl: &curl,
            },
            &cfg,
            &output,
            plan(120),
            &flag,
        )
        .expect("soak");
        assert!(began.elapsed() < Duration::from_secs(20));
        let json = report(&output);
        assert_eq!(
            json["status"]["samples"], 0,
            "the interrupted iteration is dropped"
        );
        assert!(!output.join("clients").exists());
    }

    #[test]
    fn client_inputs_are_removed_on_every_exit_including_a_panic() {
        let temp = tempfile::tempdir().expect("temp");
        let make = |name: &str| {
            let path = temp.path().join(name);
            fs::create_dir(&path).expect("dir");
            fs::write(path.join("secret"), "x").expect("file");
            path
        };
        let explicit = make("explicit");
        let mut guard = ClientDir::new(explicit.clone());
        guard.remove().expect("remove");
        assert!(!explicit.exists());
        fs::create_dir(&explicit).expect("recreated by someone else");
        drop(guard);
        assert!(
            explicit.exists(),
            "an explicit removal is not repeated on drop"
        );
        let dropped = make("dropped");
        drop(ClientDir::new(dropped.clone()));
        assert!(!dropped.exists());
        let panicked = make("panicked");
        let path = panicked.clone();
        let result = std::panic::catch_unwind(move || {
            let _guard = ClientDir::new(path);
            panic!("synthetic failure");
        });
        assert!(result.is_err());
        assert!(!panicked.exists());
    }

    fn send(signal: &str) {
        let pid = std::process::id().to_string();
        let status = std::process::Command::new("/bin/kill")
            .args([signal, pid.as_str()])
            .status()
            .expect("kill");
        assert!(status.success());
    }

    #[test]
    fn signals_set_the_stop_flag_and_registration_failures_are_reported() {
        let flag = stop(false);
        watch_signals(
            current_thread_runtime,
            SignalKind::user_defined1(),
            SignalKind::user_defined2(),
            Arc::clone(&flag),
        )
        .expect("installed");
        assert!(!flag.load(Ordering::Relaxed));
        send("-USR2");
        // Up to ten seconds for the listener thread to see the signal.
        let seen = (0..500).find(|_| {
            std::thread::sleep(Duration::from_millis(20));
            flag.load(Ordering::Relaxed)
        });
        assert!(seen.is_some());
        assert!(flag.load(Ordering::Relaxed));
        // SIGKILL cannot be handled: either slot failing is reported, not swallowed.
        let forbidden = SignalKind::from_raw(9);
        for (first, second) in [
            (forbidden, SignalKind::user_defined1()),
            (SignalKind::user_defined1(), forbidden),
        ] {
            let failed = watch_signals(current_thread_runtime, first, second, stop(false));
            assert!(failed.is_err());
        }
        let no_runtime = watch_signals(
            || Err(io::Error::other("synthetic")),
            SignalKind::user_defined1(),
            SignalKind::user_defined2(),
            stop(false),
        );
        assert!(no_runtime.is_err());
    }

    #[test]
    fn a_nap_ends_early_once_stopped() {
        let began = Instant::now();
        nap(Duration::from_secs(30), &stop(true));
        nap(Duration::ZERO, &stop(false));
        nap(Duration::from_millis(150), &stop(false));
        assert!(began.elapsed() < Duration::from_secs(5));
    }
}
