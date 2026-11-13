# Releasing

The tag-only `Release` workflow publishes to crates.io with
[trusted publishing](https://crates.io/docs/trusted-publishing), then publishes a GitHub
immutable release containing the verified source `.crate` archive. No registry token secret
or personal access token belongs in GitHub. There are no prebuilt platform binaries yet.

The native `tethercam-tools` release-tag and disposable-CI USB/IP commands come from #65.
This workflow must not be activated until that implementation is integrated and its combined
exact-commit gate is green. `--all-features` enables the off-by-default verification tooling,
including `test-support`; this is not normal camera runtime.

## Initial publication and authentication

The legitimate `0.0.1` crate was published from protected-main commit
`214e0abc5f52a2a129822466fcac247dabe8ccfe`. Its downloaded registry archive matched the admitted
package with SHA256 `6d6a88c21f1f9885384f975c962909650f083fee5c3dcbaf874055ad801900ac`.
The one-time bootstrap token has been revoked and the local Cargo login removed. No GitHub tag,
release or asset was created for that publication.

Crates.io requires an existing crate before configuring a trusted publisher. The configured
publisher identifies repository `nredd/tethercam`, workflow filename `release.yml` and environment
`crates-io`; **Require trusted publishing** is enabled. Future publication is OIDC-only, with no
API-token fallback. Retain a direct individual owner with verified email and a linked GitHub account;
the trusted-publisher configuration endpoint checks direct user ownership. Do not add an Actions
bot as an owner.

The existing `0.0.1` cannot be published again by this workflow. Its tag must point to the
original admitted commit, never later helper or release code. Use the exact-artifact recovery
procedure below if creating its GitHub release. Bump changed code to `0.0.2` or another unused
version before its first automated release. Never retag or reuse `0.0.1`, or skip publication
of different bytes because its version already exists.

## GitHub setup, by a repository administrator

The `crates-io` environment is configured with required reviewer `nredd`, administrator bypass
disabled, and a `v*` tag-only deployment rule. Self-review is deliberately allowed for the sole
maintainer, so this is not independent human approval. Enable Prevent self-review if a separate
approver becomes available. Verify these controls before creating tags; referencing a missing
environment can create an unprotected one.
No environment secret is required. Approve only the displayed admitted tag/commit after inspecting
the successful gate and artifact. Both the registry and GitHub release jobs use this environment.

`crates-io` is part of the OIDC identity. Restricting the publisher to the exact repository,
workflow filename and environment prevents other workflows from exchanging a registry token.
Only `publish` receives `id-token: write`; the first-party
[`rust-lang/crates-io-auth-action`](https://github.com/rust-lang/crates-io-auth-action) exchanges
it for a short-lived token. The asset job receives only `contents: write`, never a registry token.
All actions are pinned to full commit SHAs; checkout does not persist repository credentials.

## Immutable releases and tag integrity

Release immutability is a **repository setting**, not a main-branch ruleset rule. Under
**Settings -> General -> Releases**, enable **Enable release immutability**. The documented REST
equivalents are:

```sh
gh api repos/nredd/tethercam/immutable-releases
gh api --method PUT repos/nredd/tethercam/immutable-releases
gh api repos/nredd/tethercam/immutable-releases
```

Read back `enabled: true`. This applies to future published releases, not historical releases.
It locks the associated tag and assets, NOT the title or release notes. Deleting a release does
not make its protected tag name reusable. GitHub generates a release attestation at publication.

`.github/rulesets/releases.json` independently denies update and deletion of `refs/tags/v*`,
including before a GitHub release exists. It permits initial tag creation, has no bypass actors,
and must not replace or alter the main ruleset or classic main protection. Before activation,
list existing rulesets and inspect every matching tag policy. Create the reviewed payload only
if no `Release tag integrity` ruleset exists:

```sh
gh api repos/nredd/tethercam/rulesets
gh api --method POST repos/nredd/tethercam/rulesets --input .github/rulesets/releases.json
# Substitute the returned ID, read back every field and compare with the payload.
gh api repos/nredd/tethercam/rulesets/ID
```

An existing policy must be inspected rather than overwritten. An API error is a stop, not a
reason to add bypasses or weaken protection. Main retains all 16 Actions-bound checks and all
its existing restrictions; see [branch-ruleset.md](branch-ruleset.md).

## Each subsequent release

1. Bump `Cargo.toml` and `Cargo.lock`, move Unreleased changelog entries under the version, and
   merge a reviewed, gated PR. Freeze the release commit; re-run its combined `make all cross`
   gate before admission. No physical camera or locally privileged provisioning is needed.
2. Only after owner approval and authentication readiness, create and push `v<version>` at that
   exact protected-main commit. Tags cannot be moved or deleted; mistakes need a new version.
3. `gate` checks ancestry in protected `main`, validates tag/version with the native Rust helper,
   provisions the synthetic USB/IP fixture on the disposable Linux runner, runs `make all cross`,
   detaches the fixture even after failure, and verifies Cargo packaging and publication dry-run.
   Privileged CI fixture setup never runs in publication jobs or on a maintainer's host.
4. `publish` waits for environment admission, checks out the same SHA, downloads the gate's
   same-run archive, repeats package verification, compares its bytes, authenticates via OIDC,
   and executes `cargo publish --locked`. It never overwrites or silently skips a registry version.
5. `github-release` waits for environment admission, downloads that same-run archive, creates a
   **draft with all assets**, then publishes it. There is no post-publication asset upload.
   Native `gh` fails if a draft/release already exists; nothing is clobbered. Global release
   concurrency queues runs rather than cancelling a partially published run.

## Failure and recovery

Do not rerun all jobs blindly. A registry publish can succeed before Cargo times out waiting for
index propagation, or a GitHub draft can exist after its creation step fails. Neither registry
versions nor immutable release tags/assets can be replaced. Retries fail closed rather than
claiming an existing version is equivalent merely because its name matches.

If Cargo definitely did not publish, fix the root cause and retry only at the unchanged admitted
commit. If `publish` succeeded and `github-release` failed before creating a draft, retry only
that failed job in the original run while its artifact is retained (30 days). Never use an
artifact from a different run/commit as a substitute.

If publication status is ambiguous, an existing draft blocks retry, or this is the manually
bootstrapped initial version, the maintainer must stop and verify:

- The protected tag resolves to the admitted full commit SHA; no tag recreation or movement.
- The crates.io version API and sparse index describe the expected non-yanked version.
- The downloaded registry `.crate` bytes/checksum match the admitted gate's `.crate` archive;
  `.cargo_vcs_info.json` identifies the admitted clean commit. Matching a version string alone
  is NOT evidence of identical content. If bytes differ, stop; publish a corrected new version.
- Any existing draft has exactly the intended assets and digests, no unexpected assets, and the
  intended tag/commit. Never replace an asset to force agreement. A published immutable release
  must already match, otherwise stop and investigate.

Only after that exact-artifact review may the maintainer use native `gh release create TAG
ARCHIVE --draft --verify-tag --target FULL_SHA --title TAG --notes-file CHANGELOG.md` when no
release exists, or finish the verified existing draft with `gh release edit TAG --draft=false`.
Stage *all* intended assets before that final command. This is explicit recovery admission,
not a workflow skip-publication switch. No initial publish, tag or release is created by setup.

## References

- [Cargo publishing and ownership](https://doc.rust-lang.org/cargo/reference/publishing.html)
- [crates.io trusted publishing](https://crates.io/docs/trusted-publishing)
- [Current token scope UI](https://github.com/rust-lang/crates.io/blob/32d211414625a7f3584a590a32eaf26df620db58/e2e/acceptance/api-token.spec.ts-snapshots/aria.yml)
- [Publishing scope/name/email enforcement](https://github.com/rust-lang/crates.io/blob/32d211414625a7f3584a590a32eaf26df620db58/src/controllers/krate/publish.rs)
- [Trusted-publisher owner validation](https://github.com/rust-lang/crates.io/blob/32d211414625a7f3584a590a32eaf26df620db58/src/controllers/trustpub/github_configs/create.rs)
- [GitHub immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases)
- [Enable repository immutability API](https://docs.github.com/en/rest/repos/repos#enable-immutable-releases)
- [GitHub environment protection](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments)
