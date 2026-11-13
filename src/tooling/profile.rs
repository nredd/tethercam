//! Offline numerical profiling summaries. Inputs are never executed or echoed.
//!
//! References:
//! - <https://ffmpeg.org/ffmpeg-formats.html#framehash-1>
//! - <https://ffmpeg.org/ffprobe.html>
//! - <https://man.freebsd.org/cgi/man.cgi?query=time&sektion=1>
//! - <https://docs.rs/rustix/latest/rustix/fs/fn.open.html>

use std::fs::{self, File, Metadata};
use std::io::{Read, Write};
use std::path::Path;

use serde_json::{Value, json};

use super::Error;

// About 55 minutes at 30 fps, with room for native ffprobe metadata and PRFT side data.
const INPUT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECORDS: usize = 100_000;
const LINE_BYTES: usize = 16 * 1024;
const MAX_COLUMNS: usize = 64;

fn invalid() -> Error {
    Error::Invalid("invalid or empty profiling measurements")
}

fn number(text: &str) -> Result<f64, Error> {
    let value: f64 = text.trim().parse().map_err(|_| invalid())?;
    if !value.is_finite() || value < 0.0 {
        return Err(invalid());
    }
    Ok(value)
}

/// Profiling CSV uses simple scalar fields; quoted/nested CSV is rejected, never guessed.
fn fields(line: &str) -> Result<Vec<&str>, Error> {
    if line.len() > LINE_BYTES
        || line.contains('"')
        || line.chars().any(|c| c.is_control() && c != '\t')
    {
        return Err(invalid());
    }
    let columns: Vec<_> = line
        .split(',')
        .take(MAX_COLUMNS + 1)
        .map(str::trim)
        .collect();
    if columns.len() > MAX_COLUMNS {
        return Err(invalid());
    }
    Ok(columns)
}

struct Frames {
    duration: f64,
    count: f64,
    distinct: f64,
    bytes: f64,
}

fn frames(input: &str) -> Result<Frames, Error> {
    let mut timebase = None;
    let mut first = None;
    let mut last = 0.0;
    let mut last_stamp = None;
    let mut count = 0.0;
    let mut distinct = 0.0;
    let mut bytes = 0.0;
    let mut previous = None;
    for line in records(input)? {
        if let Some(value) = line.strip_prefix("#tb 0:") {
            if timebase.is_some() {
                return Err(invalid());
            }
            let (num, den) = value.trim().split_once('/').ok_or_else(invalid)?;
            let ratio = number(num)? / number(den)?;
            if !ratio.is_finite() || ratio <= 0.0 {
                return Err(invalid());
            }
            timebase = Some(ratio);
        } else if !line.is_empty() && !line.starts_with('#') {
            let row = fields(line)?;
            let [stream, _dts, pts, duration, size, hash, ..] = row.as_slice() else {
                return Err(invalid());
            };
            if *stream != "0" || hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(invalid());
            }
            let stamp: f64 = pts.parse().map_err(|_| invalid())?;
            if !stamp.is_finite()
                || stamp.abs() > 9_007_199_254_740_991.0
                || pts.parse::<i64>().is_err()
            {
                return Err(invalid());
            }
            let ticks = number(duration)?;
            if ticks <= 0.0 || ticks.fract() != 0.0 {
                return Err(invalid());
            }
            if last_stamp.is_some_and(|previous| stamp <= previous) {
                return Err(invalid());
            }
            first.get_or_insert(stamp);
            last_stamp = Some(stamp);
            last = stamp + ticks;
            count += 1.0;
            bytes += number(size)?;
            if previous != Some(*hash) {
                distinct += 1.0;
            }
            previous = Some(*hash);
        }
    }
    let duration = (last - first.ok_or_else(invalid)?) * timebase.ok_or_else(invalid)?;
    if !duration.is_finite() || duration <= 0.0 || !bytes.is_finite() {
        return Err(invalid());
    }
    Ok(Frames {
        duration,
        count,
        distinct,
        bytes,
    })
}

/// Nearest-rank-style percentiles match the existing offline summaries.
fn stats(mut values: Vec<f64>) -> Result<Value, Error> {
    if values.is_empty()
        || values.len() > MAX_RECORDS
        || values.iter().any(|v| !v.is_finite() || *v < 0.0)
    {
        return Err(invalid());
    }
    values.sort_by(f64::total_cmp);
    // Percentiles and median positions are bounded by the nonempty sorted inventory.
    let at = |index| values.get(index).copied().unwrap_or_default();
    let rank = |percent| at((values.len() - 1) * percent / 100);
    let middle = values.len() / 2;
    let median = if values.len().is_multiple_of(2) {
        f64::midpoint(at(middle - 1), at(middle))
    } else {
        at(middle)
    };
    let count: f64 = values.iter().map(|_| 1.0).sum();
    let mean = values.iter().sum::<f64>() / count;
    let deviation = (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / count).sqrt();
    if !mean.is_finite() || !deviation.is_finite() {
        return Err(invalid());
    }
    Ok(
        json!({"median":median,"p95":rank(95),"p99":rank(99),"maximum":at(values.len() - 1),"stddev":deviation}),
    )
}

fn packets(input: &[u8]) -> Result<Value, Error> {
    if input.len() as u64 > INPUT_BYTES {
        return Err(invalid());
    }
    let value: Value = serde_json::from_slice(input)?;
    let rows = value
        .get("packets")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if rows.len() > MAX_RECORDS {
        return Err(invalid());
    }
    let mut intervals = Vec::new();
    let mut previous = None;
    for row in rows {
        let stamp = number(
            row.get("pts_time")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?,
        )?;
        if let Some(last) = previous {
            if stamp <= last {
                return Err(invalid());
            }
            intervals.push((stamp - last) * 1000.0);
        }
        previous = Some(stamp);
    }
    stats(intervals)
}

fn source(input: &str) -> Result<Value, Error> {
    let mut lines = records(input)?;
    let header = fields(lines.next().ok_or_else(invalid)?)?;
    let columns: Vec<_> = header
        .iter()
        .enumerate()
        .filter(|(_, name)| **name == "fetch_s")
        .map(|(i, _)| i)
        .collect();
    let [column] = columns.as_slice() else {
        return Err(invalid());
    };
    let mut values = Vec::new();
    for line in lines {
        let row = fields(line)?;
        if row.len() != header.len() {
            return Err(invalid());
        }
        // The column came from this exact-length header, so the scalar is present.
        values.push(1000.0 * number(row.get(*column).copied().unwrap_or_default())?);
    }
    let summary = stats(values)?;
    Ok(
        json!({"median":summary.get("median"),"p95":summary.get("p95"),"max":summary.get("maximum")}),
    )
}

/// Retain source-only cadence/bandwidth measurements when the complete acquisition CSV is present.
fn acquisition(input: &str) -> Result<Option<Value>, Error> {
    let mut lines = records(input)?;
    let header = fields(lines.next().ok_or_else(invalid)?)?;
    let mut columns = Vec::new();
    for name in ["elapsed_s", "jpeg_bytes", "fingerprint"] {
        let found: Vec<_> = header
            .iter()
            .enumerate()
            .filter(|(_, field)| **field == name)
            .map(|(index, _)| index)
            .collect();
        match found.as_slice() {
            [] => (),
            [column] => columns.push(*column),
            _ => return Err(invalid()),
        }
    }
    if columns.is_empty() {
        return Ok(None);
    }
    let [elapsed_column, size_column, hash_column] = columns.as_slice() else {
        return Err(invalid());
    };
    let mut last = 0.0;
    let mut count = 0.0;
    let mut distinct = 0.0;
    let mut bytes = 0.0;
    let mut previous = None;
    for line in lines {
        let row = fields(line)?;
        if row.len() != header.len() {
            return Err(invalid());
        }
        let elapsed = number(row.get(*elapsed_column).copied().unwrap_or_default())?;
        let size = number(row.get(*size_column).copied().unwrap_or_default())?;
        let hash = row.get(*hash_column).copied().unwrap_or_default();
        if elapsed <= last
            || size <= 0.0
            || size.fract() != 0.0
            || hash.is_empty()
            || hash.len() > 64
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        count += 1.0;
        bytes += size;
        if previous != Some(hash) {
            distinct += 1.0;
        }
        previous = Some(hash);
        last = elapsed;
    }
    if count == 0.0 {
        return Err(invalid());
    }
    let fps = count / last;
    let unique_fps = distinct / last;
    let payload = bytes * 8.0 / last / 1_000_000.0;
    let mean = bytes / count;
    if [fps, unique_fps, payload, mean]
        .iter()
        .any(|value| !value.is_finite())
    {
        return Err(invalid());
    }
    Ok(Some(
        json!({"duration_s":last,"frames":count,"distinct_consecutive_frames":distinct,"fps":fps,"unique_fps":unique_fps,"jpeg_payload_mbit_s":payload,"jpeg_mean_bytes":mean}),
    ))
}

fn timing(input: &str) -> Result<(f64, f64, f64), Error> {
    let mut times = std::collections::BTreeMap::new();
    let mut rss = None;
    for line in records(input)? {
        let words: Vec<_> = line.split_whitespace().collect();
        match words.as_slice() {
            [name @ ("real" | "user" | "sys"), value] => {
                if times.insert(*name, number(value)?).is_some() {
                    return Err(invalid());
                }
            }
            [value, "maximum", "resident", "set", "size", suffix @ ..]
                if suffix.is_empty() || suffix == ["kib"] =>
            {
                let divisor = if suffix.is_empty() {
                    1_048_576.0
                } else {
                    1024.0
                };
                if rss.replace(number(value)? / divisor).is_some() {
                    return Err(invalid());
                }
            }
            _ => (),
        }
    }
    let wall = *times.get("real").ok_or_else(invalid)?;
    let user = *times.get("user").ok_or_else(invalid)?;
    let sys = *times.get("sys").ok_or_else(invalid)?;
    if wall == 0.0 {
        return Err(invalid());
    }
    let cpu = 100.0 * (user + sys) / wall;
    if !cpu.is_finite() {
        return Err(invalid());
    }
    Ok((wall, cpu, rss.ok_or_else(invalid)?))
}

fn summarize(
    framehash: &str, packet_json: &[u8], metrics: Option<&str>, source_csv: Option<&str>,
    h264_bytes: u64,
) -> Result<Value, Error> {
    let frames = frames(framehash)?;
    let native = metrics.map(timing).transpose()?;
    let acquisition = source_csv.map(acquisition).transpose()?.flatten();
    let (wall, cpu, rss) = (
        native.map(|v| v.0),
        native.map(|v| v.1),
        native.map(|v| v.2),
    );
    let h264 = serde_json::Number::from(h264_bytes)
        .as_f64()
        .unwrap_or_default()
        * 8.0
        / frames.duration
        / 1_000_000.0;
    if [
        h264,
        frames.count / frames.duration,
        frames.bytes * 8.0 / frames.duration / 1_000_000.0,
    ]
    .iter()
    .any(|value| !value.is_finite())
    {
        return Err(invalid());
    }
    Ok(json!({
        "duration_s":frames.duration,"jpeg_output_frames":frames.count,"jpeg_output_fps":frames.count / frames.duration,
        "jpeg_distinct_consecutive_frames":frames.distinct,"jpeg_unique_fps":frames.distinct / frames.duration,
        "jpeg_repeat_percent":100.0 * (frames.count - frames.distinct) / frames.count,
        "jpeg_payload_mbit_s":frames.bytes * 8.0 / frames.duration / 1_000_000.0,
        "jpeg_mean_bytes":frames.bytes / frames.count,"h264_payload_mbit_s":h264,
        "packet_arrival_interval_ms":packets(packet_json)?,"source_fetch_ms":source_csv.map(source).transpose()?,
        "native_cpu_percent_one_core":cpu,"native_peak_rss_mib":rss,"native_wall_s":wall,
        "source_acquisition":acquisition,
        "absolute_glass_to_glass_latency":"not measured"
    }))
}

pub(super) fn check(paths: [&Path; 5], out: &mut dyn Write) -> Result<(), Error> {
    let [framehash, packets, metrics, source, h264] = paths;
    let media = fs::metadata(h264)?;
    if !media.is_file() {
        return Err(invalid());
    }
    let summary = summarize(
        &text(framehash)?,
        &aggregate(packets)?,
        optional(metrics)?.as_deref(),
        optional(source)?.as_deref(),
        media.len(),
    )?;
    serde_json::to_writer_pretty(&mut *out, &summary)?;
    writeln!(out)?;
    Ok(())
}

fn optional(path: &Path) -> Result<Option<String>, Error> {
    if path == Path::new("-") {
        return Ok(None);
    }
    Ok(Some(text(path)?))
}

/// Bound text records before allocating per-row columns or statistics.
fn records(input: &str) -> Result<std::str::Lines<'_>, Error> {
    if input.len() as u64 > INPUT_BYTES
        || input
            .lines()
            .enumerate()
            .any(|(row, line)| row >= MAX_RECORDS || line.len() > LINE_BYTES)
    {
        return Err(invalid());
    }
    Ok(input.lines())
}

/// Native nonblocking open prevents a regular-file-to-FIFO swap from hanging before validation.
fn open_aggregate(path: &Path) -> Result<File, Error> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, open};
        Ok(open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?
        .into())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(Error::Invalid(
            "nonblocking aggregate file reads are unsupported on this platform",
        ))
    }
}

/// Read only a validated opened regular handle, never trust a path metadata preflight.
fn aggregate(path: &Path) -> Result<Vec<u8>, Error> {
    opened(&mut open_aggregate(path)?, File::metadata)
}

/// The metadata syscall seam lets failure tests retain an actual owned file handle.
fn opened(
    file: &mut File, metadata: fn(&File) -> std::io::Result<Metadata>,
) -> Result<Vec<u8>, Error> {
    let metadata = metadata(file)?;
    if !metadata.is_file() || metadata.len() > INPUT_BYTES {
        return Err(invalid());
    }
    bounded(file, INPUT_BYTES)
}

/// Cap bytes while reading, including files that grow after handle metadata was inspected.
fn bounded(reader: &mut dyn Read, limit: u64) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid());
    }
    Ok(bytes)
}

fn text(path: &Path) -> Result<String, Error> {
    String::from_utf8(aggregate(path)?).map_err(|_| invalid())
}

#[cfg(test)]
mod tests;
