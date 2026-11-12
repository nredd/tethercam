# Media pipeline: JPEG decoder and H.264 encoder evaluation

Decision for `tethercam::media` (issue #8): decode with `zune-jpeg`, encode with
`rusty_h264-encoder` (features `std` and `asm`, `global-alloc` off).

Target stream: 1024x680, 15 fps, about 3 Mbps, Baseline or Main, pure Rust.

## Method

- Machine: Apple M4 (10 cores), macOS, `rustc` 1.99, release profile. Single-threaded unless noted
- In-repo numbers: `cargo bench --bench media` (`criterion`, one element is one frame, so
  `elem/s` is frames per second). Synthetic textured moving scene, JPEG made in memory
- Candidate comparison: a throwaway harness feeding the same 45 synthetic 1024x680 frames to each
  encoder at 3 Mbps with a 30-frame keyframe interval. Output was decoded by `ffmpeg` by hand to
  confirm it is a valid stream, and PSNR was measured against the source. `ffmpeg` is not used by
  the repository's tests
- Numbers are from a desktop core. An `aarch64` or `armv7` SoC board is not available to this
  work, so real time on the intended hardware is **not measured**; see "What is not known"

## H.264 encoders

Pure-Rust H.264 encoders found on crates.io:

| Crate | License | Output | Verdict |
|---|---|---|---|
| `rusty_h264-encoder` 0.17.2 | BSD-2-Clause | Constrained Baseline and Main, I and P, CAVLC / CABAC, ABR rate control | chosen |
| `veloce-h264enc` 0.1.7 | MIT OR Apache-2.0 | Baseline / Main / High, rayon by default | runner-up, see below |
| `less-avc` 0.1.5 | MIT OR Apache-2.0 | lossless I_PCM only, no rate control | unusable: about 1 MB per frame, roughly 125 Mbps at 15 fps |
| `rumpeg-h264` 0.1.2 | MIT OR Apache-2.0 | intra only (`I_PCM`, `I_16x16`) | no inter frames, cannot hold 3 Mbps at this size |
| `oxideav-h264` 0.1.12 | MIT | no encoder, crate is mid-rewrite | n/a |
| `threers-codec` 0.0.12 | MIT | umbrella crate, 0.0.x | not evaluated further: new, broad, unproven |

C encoders (`x264`, `openh264`) are out of scope by rule.

### Measured: `rusty_h264-encoder`

Same 45-frame harness, 3 Mbps target, 30-frame keyframe interval:

| Configuration | fps | achieved Mbps | PSNR (dB, avg of Y/U/V) |
|---|---|---|---|
| Constrained Baseline, `Fast`, `asm` | 138 | 3.14 | 36.1 |
| Main (CABAC), `Fast`, `asm` | 88 | 3.03 | 36.4 |
| Main (CABAC), `Balanced`, `asm` | 17 | 3.03 | 36.7 |
| Constrained Baseline, `Fast`, scalar | 70 | 3.14 | 36.1 |
| Main (CABAC), `Fast`, scalar | 39 | 3.03 | 36.4 |
| Main (CABAC), `Balanced`, scalar | 4.3 | 3.03 | 36.7 |

In-repo benchmark (`SoftwareH264Encoder`, `asm`, 1024x680, whole `encode` call):

| Profile | time per frame | frames per second |
|---|---|---|
| Baseline | 7.3 ms | 137 |
| Main | 9.0 ms | 111 |
| Baseline, scalar (`asm` off) | 24 ms | 41 |
| Main, scalar (`asm` off) | 61 ms | 16 |

All three outputs decode with `ffmpeg` without a single decoder message, report the right profile
(`Constrained Baseline` / `Main`) and a 1024x680 picture (the 680 rows are signalled with SPS frame
cropping, 680 is not a multiple of 16).

### Measured: `veloce-h264enc`

Same harness, Baseline, `Cbr { kbps: 3000 }`, scene-cut off:

| Configuration | fps | achieved Mbps | PSNR (dB) |
|---|---|---|---|
| `Speed`, 1 thread | 38 | 2.99 | 16.9 |
| `Speed`, all cores | 154 | 2.99 | 16.9 |
| `Balanced`, 1 thread | 18 | 2.99 | 16.9 |

Valid streams (`ffmpeg` decodes all 45 frames). Rate control collapsed in this harness: the first
frames come out near 26 dB and fall from there, against 34 to 36 dB for `rusty_h264-encoder` at the
same size. This could be harness misuse (settings the crate expects differently); it was not
investigated further because the speed is also lower per core and the crate is far younger.
Worth re-testing if `rusty_h264-encoder` stalls.

### Why `rusty_h264-encoder`

- Correctness: decodes cleanly under `ffmpeg`; its own test suite gates output against a second
  decoder across QP 0 to 51
- Speed: 137 fps Baseline on one desktop core with `asm`, 9x the 15 fps target. Even scalar it is
  2.7x the target
- Features that matter here: ABR rate control, SPS cropping for 680 rows, IDR placement by
  interval, `request_keyframe`, one access unit per call with lookahead off, `encode_into` a
  caller-owned buffer (no per-frame allocation)
- License: BSD-2-Clause, permissive. `cargo deny` needs `BSD-2-Clause` added to the allow list
- Maintenance: active, with a changelog of measured speed-ups. It is a young crate maintained by one organisation, so the `Encoder` trait is the escape hatch: a
  second backend replaces it without touching callers

### How it is built into this crate

- `default-features = false`, `features = ["std", "asm"]`
- `global-alloc` is **off**. Its default installs a process-wide `#[global_allocator]` from a
  library, which a library crate must never do
- `asm` is the portable-Rust SIMD kernels in `rusty_h264-accel`. They are plain Rust intrinsics, not
  C or assembly files, with `unsafe` quarantined in that dependency; the encoder crate itself is
  `forbid(unsafe_code)`. This crate keeps `unsafe_code = "forbid"`. The kernels exist for
  `x86_64` and `aarch64`; on `armv7` the encoder falls back to scalar code (`cargo check` for both
  Linux targets passes) and runs at the scalar numbers above
- Settings: `EncoderConfig::baseline` as the base (no lookahead, no scene cut, no mb-tree, no
  B-frames, one reference frame, `Fast` preset), Main adds CABAC. The `level_idc` is chosen by
  this crate from H.264 Table A-1 (frame size, macroblock rate and bit rate); 1024x680 at 15 fps
  and 3 Mbps is level 3.1. The default level 3.0 cannot hold 2752 macroblocks per frame

Rejected: the `Balanced` preset costs 5x the time for 0.3 dB in the table above, the wrong trade on
a board. High profile was not measured.

## JPEG decoders

Decoding the same 1024x680 4:2:0 JPEG (72 KB), YCbCr output, 200 iterations:

| Decoder | License | fps | Notes |
|---|---|---|---|
| `zune-jpeg` 0.5, safe path | MIT OR Apache-2.0 OR Zlib | about 530 to 590 | decodes into a caller buffer, no `unsafe` flag |
| `zune-jpeg` 0.5, `use_unsafe` | same | about 340 to 450 | slower in this build |
| `jpeg-decoder` 0.3.2 | MIT OR Apache-2.0 | 91 | allocates the output on each call, no decode-into |

`zune-jpeg` wins on speed and on API (`decode_into` a reusable buffer), handles baseline and
progressive, any subsampling, and odd dimensions. The `x86` / `neon` features and the
`use_unsafe` option were measured and are **not** used: the safe path was as fast or faster.

In-repo (`JpegDecoder::decode_into`, including the YCbCr to I420 repack):

| Input | time per frame | frames per second |
|---|---|---|
| 4:2:0 | 1.9 ms | 535 |
| 4:2:2 | 2.0 ms | 494 |

Decode plus encode is about 9 ms per frame (Baseline) on the desktop core, so roughly 110 fps
for the whole pipeline against the 15 fps target.

### Colour handling

`zune-jpeg` emits full-resolution interleaved YCbCr (it upsamples subsampled chroma itself).
`JpegDecoder` averages 2x2 chroma back down and rescales JPEG's full range to limited range
(Y 16..=235, CbCr 16..=240). The encoder does not write VUI colour information, so players assume
limited range; passing full-range samples through would clip highlights and shadows. The matrix is
BT.601 as JFIF specifies.

## What is not known

- Real-time on the intended boards. Scalar Baseline at 24 ms per frame on an M4 core leaves a
  budget of 66 ms per frame at 15 fps; a small `armv7` core several times slower than a desktop
  core may not make it, `aarch64` with the NEON kernels probably does. Needs a measurement on
  hardware. The tracked question in `docs/architecture.md` stays open until then
- Quality on real camera content. All numbers use a synthetic scene; the PSNR figures only compare
  encoders against each other
- Whether UniFi Protect accepts Baseline or needs Main (tracked separately). Both are selectable

## References

- [`rusty_h264-encoder`](https://docs.rs/rusty_h264-encoder), [`rusty_h264-accel`](https://docs.rs/rusty_h264-accel)
- [`veloce-h264enc`](https://docs.rs/veloce-h264enc), [`less-avc`](https://docs.rs/less-avc)
- [`zune-jpeg`](https://docs.rs/zune-jpeg), [`jpeg-decoder`](https://docs.rs/jpeg-decoder)
- ITU-T H.264, Table A-1 (level limits)
