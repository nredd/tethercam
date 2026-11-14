# Unattended run and recovery

How to answer "what happened during the last 24 hours" for an installed service, and the checklist
for the cases an unattended run has to survive. The counters themselves are defined in
[`service.md`](service.md#run-counters). Nothing here needs a debugger, a payload capture or a
device identity: counts, durations and the battery trajectory only.

## The 24-hour soak

`tethercam-tools soak` samples a service that is already running. Each iteration:

- reads `GET /tethercam/status` with `curl` (HTTP Digest, credentials from a protected file, never argv)
- plays the primary RTSP stream (`/live/main`, so `streams.h264` must stay enabled) for one segment
  with `ffprobe`, recording packet arrival times

These are the same external clients and protected client inputs as `tethercam-tools verify`
([`verification.md`](verification.md)). It never starts, stops or configures the service and never
opens the camera. Unlike `verify`, it accepts a wildcard listener (it dials the loopback address of
the same family) and a configured host address, because the service it samples is the real one.

```sh
cargo build --release --features repo-tools --bin tethercam-tools
target/release/tethercam-tools soak CONFIG NEW_OUTPUT_DIRECTORY FFPROBE CURL DURATION_SECS SEGMENT_SECS GAP_MS
```

All paths are absolute and the output directory must not exist. `CONFIG` is the service's own config
(macOS: `~/Library/Application Support/tethercam/config.toml`), read for the listener ports and
credentials. Limits: duration 1 s to 7 days, segment 1 s to 1 h, gap threshold 1 ms to 60 s.

For the release run (24 hours, 60-second segments, gaps over 500 ms):

```sh
target/release/tethercam-tools soak \
  "$HOME/Library/Application Support/tethercam/config.toml" \
  "$HOME/tethercam-soak-$(date +%Y%m%d)" \
  /opt/homebrew/bin/ffprobe /usr/bin/curl 86400 60 500
```

Run it detached from the terminal (e.g. `nohup ... &` or a multiplexer). It rewrites `report.json`
after every iteration, so a run that dies early still leaves its last state. The client inputs it
creates carry credentials; it deletes them when it finishes. If it is killed instead, delete
`NEW_OUTPUT_DIRECTORY/clients` by hand before sharing anything from the directory.

The soak is itself the streaming client, so the camera's live view runs during segments (that is
the load an NVR would add). Between segments the client reconnects, which takes a moment. Pauses
inside a segment are what `gaps_over_threshold` counts; time not inside any segment is
`elapsed_secs` minus `covered_secs`. A camera that is away shows up as a segment that fails fast
(`segments_failed`) and status samples reading `waiting`.

## Report

`report.json`, counts only, mode `0600`:

```json
{
  "elapsed_secs": 86412,
  "planned_secs": 86400,
  "segment_secs": 60,
  "gap_threshold_ms": 500,
  "status": {
    "samples": 1431,
    "failures": 3,
    "camera_ready_samples": 1400,
    "process_restarts_seen": 1,
    "last_uptime_secs": 41200,
    "totals": {
      "frames_fetched": 1210000, "frames_served": 1205000, "frames_dropped": 40,
      "reconnects": 2, "session_restarts": 3, "connect_misses": 55,
      "snapshots_served": 0, "snapshots_refused": 0
    }
  },
  "stream": {
    "segments": 1431, "segments_failed": 6, "packets": 1300000,
    "covered_secs": 85000, "gaps_over_threshold": 4, "max_gap_ms": 2100
  },
  "battery": {
    "points": [
      { "t_secs": 0, "percent": 80, "power": "usb" },
      { "t_secs": 5400, "percent": 79, "power": "usb" }
    ],
    "points_dropped": 0
  }
}
```

- `status.totals` are summed across process restarts: a restart is a reading whose uptime or any
  counter went backwards (`process_restarts_seen`), and the new process's counts are added rather
  than subtracted. A restart that happens between two readings and leaves every counter and the
  uptime higher is not detectable, so cross-check the service log
- `status.failures` are readings that failed (connection refused, non-200, unparsable): the service
  was down, restarting or unreachable
- `stream.segments_failed`: `ffprobe` could not play or returned no packets
- `battery.points` records each change of `(percent, power)` with seconds since the start, at most 1000
  (`points_dropped` counts the rest). `percent` is null when the camera reports none
- Also read the service's own `run_counters` lines (every five minutes) for the same totals from
  the other side, and `camera_open_failed` reasons for why a camera was away

## Recovery checklist

Run each case once during the soak, with the stream playing, and record the same things for each.
Nothing identifying the camera, host or network goes in the record.

Record for every case:

- the UTC time of the action and the UTC time the status first read `ready` again (seconds to recover)
- the counter deltas from two `/tethercam/status` readings, before and after: `session_restarts`,
  `reconnects`, `connect_misses`, and `uptime_secs` (a drop means the process restarted)
- whether the stream client resumed with no manual step (yes/no), and whether anything had to be
  done by hand (what)
- whether the camera needed a physical power-cycle (yes/no). The answer for the whole run, not per
  case, goes in the final summary

| Case | How | Expect, per the design | Also record |
| --- | --- | --- | --- |
| Camera auto power-off | Set the camera's power-save time to its shortest, leave it idle on USB and wait | Status reads `waiting`, `session_restarts` +1, the service stays up | Whether the camera powers off at all on USB power, and whether waking it is enough or it needs a power-cycle |
| USB unplug and replug | Unplug the cable, wait 10 s, replug | `waiting` with the process still running (`uptime_secs` keeps growing), `ready` again after replug, `session_restarts` +1 and `reconnects` +1 | Time unplugged; whether `camera_open_failed` reasons were `disconnected`, `busy` or `mode` meanwhile |
| Host sleep and wake | `pmset sleepnow`, wake after 10 minutes | The service survives; `uptime_secs` includes the sleep; the camera session may need to recover | Whether the session recovered by itself; whether the camera stayed powered during sleep (battery trend across it) |
| Screen lock and unlock | Lock the screen (Ctrl+Cmd+Q), wait 10 minutes, unlock | No effect on a per-user LaunchAgent, which keeps its GUI session | Any `waiting` period or `session_restarts` change. [`service.md`](service.md) notes lock is not yet validated on hardware |
| Process kill and restart, graceful | macOS: `launchctl kill TERM gui/$(id -u)/net.tethercam.agent`. Linux: `systemctl --user kill -s TERM tethercam` | The agent shuts down in order. macOS: `KeepAlive` restarts it after the ten-second throttle. Linux: the unit has `Restart=on-failure`, so a clean stop stays stopped until `systemctl --user start tethercam`; record that manual step. Counters reset | Time from kill to `ready`; the `process_restarts_seen` count |
| Process kill and restart, abrupt | macOS: `launchctl kill KILL gui/$(id -u)/net.tethercam.agent`. Linux: `systemctl --user kill -s KILL tethercam` | Restarts without a manual step (`KeepAlive`, or `Restart=on-failure` after five seconds), but the camera session was never closed | Whether the camera accepted a new session at once or needed a power-cycle, and for how long it refused |

Every failure found becomes its own issue, with only the counts above in it.
