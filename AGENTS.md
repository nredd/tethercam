# tethercam -- repo rules

These override generic procedure. Read `docs/architecture.md` before touching a layer.

Gate:
- `make all` must be green before any push: `fmt-check`, `lint`, `check`, `test`, `doc`, `bench-check`, `deny`, `coverage`
- CI runs the same targets as separate workflows. A red workflow is never merged past

Hard rules:
- Rust only. No C, C++ or other-language sources in this repo, no `build.rs` that compiles them. A dependency that vendors or links native code needs a written justification in `docs/architecture.md` and a `deny.toml` entry, and must sit behind a cargo feature that is off by default
- `unsafe_code` is forbidden crate-wide. Do not lift it
- No lint waivers: no `#[allow(...)]` / `#[expect(...)]` to get past clippy, no widening `clippy.toml` or `deny.toml` to make a check pass. Fix the code. If a lint is genuinely wrong for the whole crate, change the config in its own commit and say why
- No `unwrap` / `expect` / `panic!` / slice indexing outside tests. Every fallible path returns a typed `thiserror` error
- No `println!` / `eprintln!`. Logging goes through the logging facade; CLI output goes through an explicit `Write`
- Every parser of bytes from the camera or the network is total: it returns an error on any input and never panics. Each one gets a `proptest` "never panics on arbitrary bytes" test
- Every line, function and region under `src/` is covered by the test suite (100%, `cargo llvm-cov nextest`; `#[cfg(test)]` code counts too). `make coverage` is part of the gate and a required check. No exclusions: no `--ignore-filename-regex` for convenience, no `#[coverage(off)]`. Unreachable code is deleted or restructured so the state cannot be expressed, never waived. Hardware-bound logic sits behind the transport trait so the simulated camera covers it
- `coverage-pending.txt` (`<src path> <tracking issue>` per line) is the transition list for files that predate the gate. It may only shrink: `scripts/coverage-pending.sh shrink origin/main` fails a run that adds a path, and `validate` fails a path that does not exist. Delete the file and the mechanism when it is empty
- `make coverage` fails when any pending file reaches 100% lines, functions and regions; remove that entry
- Everything runs without a camera. Hardware access sits behind a transport trait with an in-memory simulated camera; CI never needs USB
- Must build for `aarch64-unknown-linux-gnu` and `armv7-unknown-linux-gnueabihf` (`make cross`). No macOS-only code outside `cfg`-gated modules with a portable fallback
- Per-frame paths get a `criterion` benchmark in `benches/`
- Security is not optional: no unauthenticated network surface, no default or hardcoded credentials, secrets never logged, constant-time credential comparison
- `tests/debug_guard.rs` checks raw `password`, `authorization`, `response`, `headers`, `nonce`, `nonces`, `www_authenticate`, `seen`, request `body`, and ONVIF response `body` fields; extend this list when adding another sensitive field name
- This repository is PUBLIC. Never commit real IP addresses, hostnames, serial numbers, MAC addresses, credentials or anything identifying a private network. Use `192.0.2.0/24` (TEST-NET-1) and obviously synthetic values in docs, tests and fixtures

Layout:
- One package: library `tethercam` plus binary `tethercam`. Layers are modules under `src/`
- Unit tests live beside the code in `#[cfg(test)] mod tests`. Tests through the public API live in `tests/`
- `examples/` holds runnable usage examples; `benches/` holds `criterion` benchmarks

Docs and changelog:
- A behavior change updates `README.md` or `docs/` in the same PR
- Every user-visible change adds a line under `## [Unreleased]` in `CHANGELOG.md`
- Public items are documented (`missing_docs` is denied). Module docs carry a `References:` list for the spec they implement

Style:
- Commits: capitalized imperative subject, no trailing period, no `feat:` / `fix:` prefix. PR titles leave room for ` (#N)` within 72 chars
- `TODO(redd):` / `NOTE(redd):` are the only marker forms
