//! Synthetic, credential-free metrics including PRFT side-data hashes.

use super::*;

fn hashes() -> String {
    format!(
        "#tb 0: 1/20\n#hash: SHA256\n0, 0, 0, 1, 1000, {}, S=1, {}\n0, 1, 1, 1, 1000, {}, S=1, {}\n0, 2, 2, 1, 1000, {}\n",
        "a".repeat(64),
        "b".repeat(64),
        "a".repeat(64),
        "c".repeat(64),
        "d".repeat(64)
    )
}
const PACKETS: &[u8] = br#"{"packets":[{"pts_time":"0"},{"pts_time":"0.05"},{"pts_time":"0.10"}]}"#;
const METRICS: &str = "real 2\nuser 1\nsys 0.5\n 2097152 maximum resident set size\nother metric\n";
const SOURCE: &str = "n,fetch_s\n1,0.02\n2,0.04\n";

#[test]
fn synthetic_rates_payloads_cpu_rss_and_jitter() {
    for rate in [15.0, 20.0, 30.0] {
        let framehash = hashes().replace("1/20", &format!("1/{rate}"));
        let summary =
            summarize(&framehash, PACKETS, Some(METRICS), Some(SOURCE), 3000).expect("summary");
        let close = |key: &str, expected: f64| {
            assert!((summary[key].as_f64().expect("number") - expected).abs() < 1e-8);
        };
        close("jpeg_output_fps", rate);
        close("jpeg_unique_fps", 2.0 * rate / 3.0);
        close("jpeg_output_frames", 3.0);
        close("jpeg_distinct_consecutive_frames", 2.0);
        close("jpeg_repeat_percent", 100.0 / 3.0);
        close("jpeg_mean_bytes", 1000.0);
        close("jpeg_payload_mbit_s", 0.008 * rate);
        close("h264_payload_mbit_s", 0.008 * rate);
        close("native_cpu_percent_one_core", 75.0);
        close("native_peak_rss_mib", 2.0);
        close("native_wall_s", 2.0);
        assert_eq!(summary["source_fetch_ms"]["median"], 30.0);
        assert_eq!(summary["packet_arrival_interval_ms"]["median"], 50.0);
        assert_eq!(summary["absolute_glass_to_glass_latency"], "not measured");
    }
    assert_eq!(
        stats(vec![1.0, 2.0, 4.0]).expect("odd median")["median"],
        2.0
    );
    assert_eq!(stats(vec![1.0]).expect("one measurement")["stddev"], 0.0);
}

#[test]
fn malformed_empty_nonfinite_and_duplicate_inputs_fail_closed() {
    for text in ["", "NaN", "inf", "-1", "bad"] {
        assert!(number(text).is_err());
    }
    for text in ["a,\"b\"", "a,\0b"] {
        assert!(fields(text).is_err());
    }
    for input in [
        String::new(),
        "#tb 0: bad".into(),
        "#tb 0: 1/0".into(),
        "#tb 0: 0/1".into(),
        "#tb 0: 1/1\n#tb 0: 1/1".into(),
        "0, 1".into(),
        hashes().replace("0, 0", "1, 0"),
        hashes().replace(&"a".repeat(64), "bad"),
        hashes().replace(&"a".repeat(64), &"z".repeat(64)),
        hashes().replace("0, 0, 0", "0, 0, bad"),
        hashes().replace(", 1, 1000", ", 0, 1000"),
        hashes().replace(", 1, 1000", ", 1.5, 1000"),
        hashes().replace("1/20", "1e300/1e-300"),
        hashes().replace("1/20", "1e308/1"),
        hashes().replace("#tb 0: 1/20\n", ""),
        hashes().replace("0, 2, 2", "0, 2, 1"),
        hashes().replace("1000", "1e308"),
    ] {
        assert!(frames(&input).is_err(), "accepted malformed framehash");
    }
    for input in [
        b"".as_slice(),
        b"{}",
        br#"{"packets":[]}"#,
        br#"{"packets":[{}]}"#,
        br#"{"packets":[{"pts_time":"0"}]}"#,
        br#"{"packets":[{"pts_time":"1"},{"pts_time":"0"}]}"#,
    ] {
        assert!(packets(input).is_err());
    }
    for input in [
        "",
        "wrong\n1\n",
        "fetch_s,fetch_s\n1,1",
        "fetch_s\n",
        "fetch_s\n1,2",
        "fetch_s\nNaN",
    ] {
        assert!(source(input).is_err());
    }
    for input in [
        "",
        "real 0\nuser 1\nsys 1\n1 maximum resident set size",
        "real 1\nuser 1",
        "real 1\nuser 1\nsys 1",
        "real 1\nreal 1",
        "1 maximum resident set size\n1 maximum resident set size",
        "real 1e-300\nuser 1e300\nsys 1e300\n1 maximum resident set size",
    ] {
        assert!(timing(input).is_err());
    }
    for values in [
        vec![],
        vec![f64::NAN],
        vec![-1.0],
        vec![1e308, 1e308],
        vec![0.0, 1e300],
    ] {
        assert!(stats(values).is_err());
    }
    assert!(
        summarize(
            &hashes().replace("1/20", "1e-320/1"),
            PACKETS,
            Some(METRICS),
            Some(SOURCE),
            u64::MAX
        )
        .is_err()
    );
}

#[test]
fn actual_files_and_io_failures() {
    let temp = tempfile::tempdir().expect("temp");
    let files =
        ["frames", "packets", "metrics", "source", "video"].map(|name| temp.path().join(name));
    let paths = files.each_ref().map(std::path::PathBuf::as_path);
    assert!(check(paths, &mut Vec::new()).is_err());
    for (path, bytes) in paths.iter().zip([
        hashes().as_bytes(),
        PACKETS,
        METRICS.as_bytes(),
        SOURCE.as_bytes(),
        &[0; 3000],
    ]) {
        fs::write(path, bytes).expect("write");
    }
    let mut output = Vec::new();
    check(paths, &mut output).expect("read summary");
    assert!(serde_json::from_slice::<Value>(&output).is_ok());
    assert!(check(paths, &mut crate::tests::Broken).is_err());
    let summary = summarize(&hashes(), PACKETS, None, None, 3000).expect("partial inputs");
    assert!(summary["native_wall_s"].is_null());
    assert!(summary["source_fetch_ms"].is_null());
    assert!(optional(Path::new("-")).expect("omitted").is_none());
    assert!(optional(&temp.path().join("missing")).is_err());
}

proptest::proptest! {
    #[test]
    fn arbitrary_profile_text_never_panics(input in ".*") {
        let _ = frames(&input); let _ = source(&input); let _ = acquisition(&input); let _ = timing(&input); let _ = packets(input.as_bytes());
    }
}

#[test]
fn deterministic_parser_error_boundaries_do_not_depend_on_generated_inputs() {
    for input in [
        "#tb 0: bad/1".into(),
        "#tb 0: 1/bad".into(),
        hashes().replace("0, 0, 0", "0, 0, NaN"),
        hashes().replace("0, 0, 0", "0, 0, 9007199254740992"),
        hashes().replace("0, 0, 0", "0, 0, 0.5"),
        hashes().replace(", 1, 1000", ", bad, 1000"),
        hashes().replace("1000", "bad"),
    ] {
        assert!(frames(&input).is_err());
    }
    frames(&format!("\n{}", hashes())).expect("blank line");
    assert!(packets(br#"{"packets":[{"pts_time":"bad"}]}"#).is_err());
    for input in ["\"bad\"", "fetch_s\n\"bad\"", "fetch_s\n1e308"] {
        assert!(source(input).is_err());
    }
    for input in [
        "real bad",
        "real 1\nsys 1\n1 maximum resident set size",
        "NaN maximum resident set size",
        "real 1\nuser 1\nsys 1\n1 maximum resident set size KiB",
        "1 maximum resident set size kib\n1 maximum resident set size kib",
    ] {
        assert!(timing(input).is_err());
    }
    assert_eq!(
        timing("real 1\nuser 1\nsys 1\n2048 maximum resident set size kib").expect("GNU metrics"),
        (1.0, 200.0, 2.0)
    );
    for (framehash, packet, metrics, csv) in [
        ("bad".into(), PACKETS, Some(METRICS), Some(SOURCE)),
        (hashes(), b"bad".as_slice(), Some(METRICS), Some(SOURCE)),
        (hashes(), PACKETS, Some("bad"), Some(SOURCE)),
        (hashes(), PACKETS, Some(METRICS), Some("bad")),
    ] {
        assert!(summarize(&framehash, packet, metrics, csv, 1000).is_err());
    }
}

#[test]
fn every_aggregate_file_failure_propagates_and_media_is_metadata_only() {
    let temp = tempfile::tempdir().expect("temp");
    let files =
        ["frames", "packets", "metrics", "source", "video"].map(|name| temp.path().join(name));
    let contents = [
        hashes().into_bytes(),
        PACKETS.to_vec(),
        METRICS.as_bytes().to_vec(),
        SOURCE.as_bytes().to_vec(),
        vec![0; 3000],
    ];
    for (path, content) in files.iter().zip(&contents) {
        fs::write(path, content).expect("write");
    }
    let paths = files.each_ref().map(std::path::PathBuf::as_path);
    let mut args = vec![std::ffi::OsString::from("profile-summary")];
    args.extend(files.iter().map(|path| path.clone().into_os_string()));
    crate::dispatch(&args, &mut Vec::new(), &mut std::io::empty()).expect("CLI");
    for (path, content) in files.iter().zip(&contents) {
        fs::remove_file(path).expect("remove");
        assert!(check(paths, &mut Vec::new()).is_err());
        fs::write(path, content).expect("restore");
    }
    for path in files.iter().take(4) {
        let original = fs::read(path).expect("read");
        fs::write(path, [255]).expect("bad UTF-8/JSON");
        assert!(check(paths, &mut Vec::new()).is_err());
        fs::write(path, original).expect("restore");
    }
    check(
        [paths[0], paths[1], Path::new("-"), Path::new("-"), paths[4]],
        &mut Vec::new(),
    )
    .expect("optional files");
    fs::remove_file(paths[4]).expect("remove video");
    fs::create_dir(paths[4]).expect("directory is not media");
    assert!(check(paths, &mut Vec::new()).is_err());
}

struct Budget(usize);
impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0 == 0 {
            return Err(std::io::Error::other("output quota"));
        }
        let length = self.0.min(bytes.len());
        self.0 -= length;
        Ok(length)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn final_output_newline_failure_is_not_discarded() {
    let temp = tempfile::tempdir().expect("temp");
    let files =
        ["frames", "packets", "metrics", "source", "video"].map(|name| temp.path().join(name));
    let contents = [
        hashes().into_bytes(),
        PACKETS.to_vec(),
        METRICS.as_bytes().to_vec(),
        SOURCE.as_bytes().to_vec(),
        vec![0; 3000],
    ];
    for (path, content) in files.iter().zip(&contents) {
        fs::write(path, content).expect("write");
    }
    let paths = files.each_ref().map(std::path::PathBuf::as_path);
    let mut output = Vec::new();
    check(paths, &mut output).expect("output");
    let mut quota = Budget(output.len() - 1);
    assert!(check(paths, &mut quota).is_err());
    quota.flush().expect("flush");
}

#[test]
fn source_only_cadence_bandwidth_and_all_column_guards_are_preserved() {
    let csv = "elapsed_s,fetch_s,jpeg_bytes,fingerprint\n0.1,0.1,1000,aa\n0.2,0.1,1000,aa\n0.3,0.1,1000,bb\n";
    let value = acquisition(csv)
        .expect("source metrics")
        .expect("complete columns");
    assert_eq!(value["frames"], 3.0);
    assert_eq!(value["distinct_consecutive_frames"], 2.0);
    assert_eq!(value["fps"], 10.0);
    assert_eq!(value["jpeg_mean_bytes"], 1000.0);
    assert_eq!(value["jpeg_payload_mbit_s"], 0.08);
    assert!(
        acquisition(SOURCE)
            .expect("fetch-only legacy input")
            .is_none()
    );
    let summary = summarize(&hashes(), PACKETS, None, Some(csv), 3000).expect("combined metrics");
    assert_eq!(summary["source_acquisition"]["frames"], 3.0);
    for input in [
        String::new(),
        "\"bad\"".into(),
        "elapsed_s".into(),
        "elapsed_s,elapsed_s,jpeg_bytes,fingerprint".into(),
        "elapsed_s,jpeg_bytes,fingerprint".into(),
        csv.replace("0.1,0.1,1000,aa", "0.1,0.1"),
        csv.replace("0.1,0.1,1000,aa", "\"bad\""),
        csv.replace("0.1,0.1,1000,aa", "bad,0.1,1000,aa"),
        csv.replace("0.1,0.1,1000,aa", "0.1,0.1,bad,aa"),
        csv.replace("0.1,0.1,1000,aa", "0,0.1,1000,aa"),
        csv.replace("0.2,0.1,1000,aa", "0.1,0.1,1000,aa"),
        csv.replace("1000", "0"),
        csv.replace("1000", "1.5"),
        csv.replace("aa", ""),
        csv.replace("aa", &"a".repeat(65)),
        csv.replace("aa", "z"),
        csv.replace("1000", "1e308"),
        "elapsed_s,jpeg_bytes,fingerprint\n1e-320,1,a\n".into(),
    ] {
        assert!(acquisition(&input).is_err(), "invalid source CSV");
    }
    assert!(summarize(&hashes(), PACKETS, None, Some("elapsed_s"), 1).is_err());
}
