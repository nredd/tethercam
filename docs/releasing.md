# Releasing

The tag-only `Release` workflow publishes to crates.io with
[trusted publishing](https://crates.io/docs/trusted-publishing), then publishes a GitHub
immutable release containing the verified source `.crate` archive. No registry token secret
or personal access token belongs in GitHub. There are no prebuilt platform binaries yet.

The native `tethercam-tools` release-tag and disposable-CI USB/IP commands come from #65.
This workflow must not be activated until that implementation is integrated and its combined
exact-commit gate is green. `--all-features` enables the off-by-default verification tooling,
including `test-support`; this is not normal camera runtime.

## Initial name ownership and authentication

Crate names are first-come-first-served. An available-name check is NOT reservation. Only a
legitimate initial publication establishes ownership; never upload a placeholder to squat a name.
The version in `Cargo.toml` is not evidence that anything exists on crates.io.

As of the official documentation checked on 2026-11-12, the crate must already exist before
adding a trusted publisher. There is no documented pending-publisher bootstrap for an unpublished
name. The first legitimate release needs a short-lived API token:

1. Log in at [crates.io](https://crates.io/) with the intended GitHub owner account. In
   [Profile](https://crates.io/settings/profile), supply an email and follow the emailed
   verification link. Verified email is required to publish and configure trusted publishing.
   Enable GitHub 2FA and satisfy any account-specific login challenges. The current Cargo/crates.io
   publishing documentation does not specify a separate universal crates.io 2FA enrollment.
2. In [API Tokens](https://crates.io/settings/tokens), generate a token named, for example,
   `tethercam-first-publish`. Select **only `publish-new`** (Publish new crates), removing the
   default `publish-update` selection. Do not grant `change-owners`, `trusted-publishing` or
   `yank`. Under Crates, **Add pattern** with exact `tethercam`, not `*` or `tethercam*`.
   Choose Custom expiration for the shortest practical publication window, or the shortest UI
   preset (currently seven days), and revoke immediately after publishing. Exact name patterns
   are accepted for unpublished crates; no unrestricted token is necessary.
3. Keep the token in a private local environment, exposed only as `CARGO_REGISTRY_TOKEN` for the
   admitted `cargo publish --locked` process. Never send it in chat, place it in command arguments,
   write it to the repository/history/logs, or run `cargo login` to persist it in Cargo's plaintext
   credential store. The release workflow has no bootstrap-token fallback.
4. Before initial publication, a maintainer must admit the exact reviewed commit, its version and
   package contents. On that frozen clean commit run `make all cross`, `cargo package --locked`
   and `cargo publish --dry-run --locked`. Linux CI must exercise USB/IP with full coverage; local
   tests do not substitute for that run. Audit `cargo package --list --locked` for private files.
   Publish **once**, only after final approval and secure token injection. Revoke the token.
5. The publishing account becomes the initial crate owner. Verify `cargo owner --list tethercam`.
   Retain a direct individual owner with verified email and a linked GitHub account: the current
   trusted-publisher configuration endpoint checks direct user ownership. Do not add an Actions
   bot as an owner. Optional individual co-owners receive full ownership powers. Optional real
   organization teams use `github:ORG:TEAM`, publish/yank rights only; no team is needed here.
6. In the new crate's **Settings -> Trusted Publishing -> Add -> GitHub**, set:
   - Repository owner: `nredd`
   - Repository name: `tethercam`
   - Workflow filename: `release.yml`, not `.github/workflows/release.yml`
   - Environment: `crates-io`, not blank

Configure the deployment environment before any release tags. For this manually bootstrapped
first version, do not push a tag expecting a second successful Cargo publication: it would fail
closed on the existing version. After initial publication, create its tag only at the admitted
commit and use the recovery procedure below to stage its GitHub release. Subsequent versions use
OIDC end to end. If the owner publishes the already-reviewed current `0.0.1` before #65/#66
merge, its tag MUST point to that original admitted commit, not the later helper/release code.
Bump the later code to `0.0.2` (or another unused version) before its first automated release.
Never retag/reuse `0.0.1` or skip publication of different bytes because its version already
exists. Name ownership is only established after crates.io accepts the real first release.

## GitHub setup, by a repository administrator

In **Settings -> Environments**, create `crates-io` before creating tags. Add trusted required
reviewer(s), disable administrator bypass, and enable Prevent self-review when a separate approver
is available. With only one maintainer, GitHub cannot provide independent human approval: choose
an explicit reviewer arrangement rather than silently bypassing it. Under **Deployment branches
and tags**, choose **Selected branches and tags** and add a **Tag** rule `v*` only, no branch rule.
Review and save these controls; referencing a missing environment can create an unprotected one.
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
