//! Counts-only sampler for an unattended run of an installed service.
//!
//! Each iteration reads the authenticated status endpoint with `curl` and plays the primary RTSP
//! stream for one segment with `ffprobe`, the same external clients and protected client inputs
//! as `verify`. The report holds only counts, durations and the battery trajectory: no device
//! identity, address, credential or payload. It is rewritten after every iteration, so a run
//! that is killed still leaves its last report.
//!
//! References:
//! - <https://curl.se/docs/manpage.html>
//! - <https://ffmpeg.org/ffprobe.html>

use super::{Error, args, capture, clients};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tethercam::onvif::media::STATUS_PATH;

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
/// `ffprobe` connects for up to 15 s before it plays for the segment.
const STREAM_SLACK: Duration = Duration::from_secs(30);

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

    /// Add a reading. A restart (the uptime or any counter went backwards) restarts the counts,
    /// so the totals add what the new process counted rather than a negative difference.
    fn status(&mut self, elapsed: Duration, sample: Sample) {
        self.samples += 1;
        self.ready_samples += u64::from(sample.ready);
        let restarted = self.previous.as_ref().is_some_and(|previous| {
            sample.uptime_secs < previous.uptime_secs
                || sample
                    .counters
                    .iter()
                    .zip(previous.counters)
                    .any(|(now, before)| *now < before)
        });
        self.process_restarts += u64::from(restarted);
        let before = self
            .previous
            .as_ref()
            .map_or([0; 8], |previous| previous.counters);
        for ((total, now), before) in self.totals.iter_mut().zip(sample.counters).zip(before) {
            *total += if restarted { now } else { now - before };
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

    /// Add a segment's packet times. A gap is a pause between consecutive packets longer than
    /// the threshold.
    fn segment(&mut self, times: &[u64], gap_ms: u64) {
        self.segments += 1;
        self.packets += u64::try_from(times.len()).unwrap_or(u64::MAX);
        let (Some(first), Some(last)) = (times.first(), times.last()) else {
            return;
        };
        self.covered_us += last.saturating_sub(*first);
        for (before, after) in times.iter().zip(times.iter().skip(1)) {
            let gap = after.saturating_sub(*before);
            self.max_gap_us = self.max_gap_us.max(gap);
            self.gaps += u64::from(gap > gap_ms * 1000);
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

/// Rewrite the report, mode `0600`, so an interrupted run keeps its latest state.
fn write_report(path: &Path, report: &Value) -> Result<(), Error> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let text = format!("{report:#}\n");
    opts.open(path)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
        })
        .map_err(Error::from)
}

fn elapsed_since(start: SystemTime) -> Duration {
    // The wall clock, deliberately: a monotonic clock stops while the host sleeps.
    start.elapsed().unwrap_or_default()
}

/// Where one iteration runs and keeps its scratch files.
struct Run<'a> {
    tools: &'a Tools<'a>,
    client_dir: &'a Path,
    base: &'a str,
    output: &'a Path,
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
        let result = capture(self.tools.curl, &cmd, &code, STATUS_BUDGET)
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

    fn stream(&self, iteration: u64, segment: Duration) -> Result<Vec<u64>, Error> {
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
        let result = capture(self.tools.ffprobe, &cmd, &out, segment + STREAM_SLACK)
            .and_then(|()| read_capped(&out))
            .and_then(|text| parse_packets(&text));
        let _ = fs::remove_file(out);
        result
    }
}

fn soak(tools: &Tools<'_>, cfg: &Path, output: &Path, plan: Plan) -> Result<(), Error> {
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
    let client_dir = output.join("clients");
    let base = clients::prepare_host(cfg, &client_dir)?;
    let run = Run {
        tools,
        client_dir: &client_dir,
        base: &base,
        output,
    };
    let result = sample_until_done(&run, plan);
    // The client inputs carry credentials; the report must be safe to share.
    let cleaned = fs::remove_dir_all(&client_dir);
    result.and(cleaned.map_err(Error::from))
}

fn sample_until_done(run: &Run<'_>, plan: Plan) -> Result<(), Error> {
    let start = SystemTime::now();
    let report_path = run.output.join("report.json");
    let mut report = Report::default();
    for iteration in 1_u64.. {
        let began = SystemTime::now();
        match run.status(iteration) {
            Ok(sample) => report.status(elapsed_since(start), sample),
            Err(_) => report.status_failed(),
        }
        let remaining = plan.duration.saturating_sub(elapsed_since(start));
        if remaining.is_zero() {
            break;
        }
        let segment = plan.segment.min(remaining).max(Duration::from_secs(1));
        match run.stream(iteration, segment) {
            Ok(times) => report.segment(&times, plan.gap_ms),
            Err(_) => report.segment_failed(),
        }
        write_report(&report_path, &report.json(&plan, elapsed_since(start)))?;
        // A segment that fails fast (no camera yet) must not turn into a busy loop.
        let period_left = plan.segment.saturating_sub(elapsed_since(began));
        std::thread::sleep(period_left.min(plan.duration.saturating_sub(elapsed_since(start))));
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
    soak(
        &Tools {
            ffprobe: Path::new(ffprobe),
            curl: Path::new(curl),
        },
        Path::new(cfg),
        Path::new(output),
        plan,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{"camera":"ready","battery_percent":80,"power":"usb","uptime_secs":5,"frames_fetched":10,"frames_served":9,"frames_dropped":1,"reconnects":0,"session_restarts":0,"connect_misses":2,"snapshots_served":0,"snapshots_refused":0}"#;

    fn sample(uptime: u64, fetched: u64) -> Sample {
        let mut counters = [0; 8];
        counters[0] = fetched;
        Sample {
            ready: true,
            battery_percent: Some(80),
            power: Some("usb".into()),
            uptime_secs: uptime,
            counters,
        }
    }

    #[test]
    fn status_documents_parse_and_every_missing_part_is_refused() {
        let ready = parse_sample(STATUS).expect("status");
        assert_eq!(ready.counters, [10, 9, 1, 0, 0, 2, 0, 0]);
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
    fn counter_totals_survive_a_process_restart() {
        let mut report = Report::default();
        let at = Duration::from_secs;
        report.status(at(0), sample(10, 100));
        report.status(at(60), sample(70, 160));
        // Restart: uptime went backwards and the counter reset.
        report.status(at(120), sample(5, 7));
        // A counter reset alone is a restart as well, even if the uptime kept growing.
        report.status(at(180), sample(65, 3));
        report.status(at(240), sample(125, 13));
        assert_eq!(report.process_restarts, 2);
        assert_eq!(report.totals[0], 100 + 60 + 7 + 3 + 10);
        assert_eq!(report.samples, 5);
        assert_eq!(report.ready_samples, 5);
        report.status_failed();
        assert_eq!(report.status_failures, 1);
    }

    #[test]
    fn the_battery_trajectory_records_changes_only_and_is_bounded() {
        let mut report = Report::default();
        let at = Duration::from_secs;
        let mut level = sample(1, 0);
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

    #[test]
    fn segments_count_packets_coverage_and_gaps_over_the_threshold() {
        let mut report = Report::default();
        // 0.0, 0.1, 0.2, then a 0.9 s pause, then 1.1
        report.segment(&[0, 100_000, 200_000, 1_100_000, 1_200_000], 500);
        report.segment(&[], 500);
        report.segment_failed();
        assert_eq!(report.segments, 3);
        assert_eq!(report.segments_failed, 1);
        assert_eq!(report.packets, 5);
        assert_eq!(report.gaps, 1);
        let plan = Plan {
            duration: Duration::from_secs(60),
            segment: Duration::from_secs(10),
            gap_ms: 500,
        };
        let json = report.json(&plan, Duration::from_secs(61));
        assert_eq!(json["stream"]["max_gap_ms"], 900);
        assert_eq!(json["stream"]["covered_secs"], 1);
        assert_eq!(json["stream"]["gaps_over_threshold"], 1);
        assert_eq!(json["status"]["last_uptime_secs"], Value::Null);
        assert_eq!(json["elapsed_secs"], 61);
        assert_eq!(json["gap_threshold_ms"], 500);
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
        // Valid numbers reach the path checks, which fail on the missing config.
        assert!(run(&values("10", "1", "500")).is_err());
        assert!(!out.exists() || fs::read_dir(&out).is_ok());
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

    #[test]
    fn a_short_run_samples_status_and_stream_and_leaves_no_credentials_behind() {
        let (temp, program, cfg) = super::super::tests::fixture();
        let curl = temp.path().join("status-curl");
        fs::copy(&program, &curl).expect("copy");
        let output = temp.path().join("soak");
        soak(
            &Tools {
                ffprobe: &program,
                curl: &curl,
            },
            &cfg,
            &output,
            plan(2),
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
        // The fixture's stream has packets at 0 s and 1 s: one gap over 500 ms.
        assert!(
            json["stream"]["gaps_over_threshold"]
                .as_u64()
                .expect("gaps")
                >= 1
        );
        assert_eq!(json["stream"]["segments_failed"], 0);
        assert!(!output.join("clients").exists());
        let text = fs::read_to_string(output.join("report.json")).expect("text");
        let secret = tethercam::config::Config::load(&cfg).expect("cfg");
        assert!(!text.contains(secret.credentials.password.expose()));
        assert!(!text.contains(&secret.endpoint_uuid.to_string()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(output.join("report.json")).expect("meta");
            assert_eq!(mode.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn failing_clients_are_counted_not_fatal() {
        let (temp, program, cfg) = super::super::tests::fixture();
        let bad_status = temp.path().join("bad200");
        fs::copy(&program, &bad_status).expect("copy");
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
            soak(&Tools { ffprobe, curl }, &cfg, &output, plan(1)).expect(name);
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
        assert!(soak(&relative, &cfg, &temp.path().join("a"), plan(1)).is_err());
        assert!(
            soak(
                &tools,
                Path::new("config.toml"),
                &temp.path().join("b"),
                plan(1)
            )
            .is_err()
        );
        assert!(soak(&tools, &cfg, Path::new("out"), plan(1)).is_err());
        // Existing output, missing config: never overwritten, never half-started.
        assert!(soak(&tools, &cfg, temp.path(), plan(1)).is_err());
        assert!(
            soak(
                &tools,
                &temp.path().join("missing.toml"),
                &temp.path().join("c"),
                plan(1)
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
        let run = Run {
            tools: &tools,
            client_dir: &missing,
            base: "http://127.0.0.1:1",
            output: &missing,
        };
        assert!(sample_until_done(&run, plan(2)).is_err());
    }
}
