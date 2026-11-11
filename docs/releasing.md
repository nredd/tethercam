# Releasing

Releases publish the crate to crates.io from GitHub Actions using
[trusted publishing](https://crates.io/docs/trusted-publishing). No API token is stored in the
repository.

One-time setup, by a crate owner:

1. Publish the first version by hand: `cargo publish`. crates.io requires the crate to exist
   before a trusted publisher can be attached
2. On crates.io, open the crate's settings, add a trusted publisher: repository
   `nredd/tethercam`, workflow `release.yml`, environment `crates-io`
3. In the GitHub repository settings, create the `crates-io` environment and add required
   reviewers

Each release:

1. Move the `## [Unreleased]` entries in `CHANGELOG.md` under a new version heading and bump
   `version` in `Cargo.toml`, in one PR
2. After it merges, tag that commit `v<version>` and push the tag
3. The `Release` workflow checks the tag matches `Cargo.toml`, runs the full gate, waits for
   approval on the `crates-io` environment, then publishes
