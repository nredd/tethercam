# Main branch ruleset

`.github/rulesets/main.json` is the GitHub repository ruleset request body. It is proposed
configuration, not evidence of live enforcement. Repository administrators activate it only
after the new jobs have completed successfully and the current merge train is stable.

## Inventory

| Workflow | Required contexts | Gate |
| --- | --- | --- |
| `fmt.yml` | `fmt` | `make fmt-check` |
| `lint.yml` | `clippy (ubuntu-latest)`, `clippy (macos)` | `make lint` |
| `check.yml` | `native-check (ubuntu-latest)`, `native-check (macos)` | `make check`, all targets and features |
| `test.yml` | `test (ubuntu-latest)`, `test (ubuntu-24.04-arm)`, `test (macos)` | `make test`, nextest and doctests |
| `test.yml` | `msrv` | Library and binary check on Rust 1.90 |
| `test.yml` | `policy` | `cargo test --test repository_policy` |
| `docs.yml` | `rustdoc` | `make doc`, warnings denied |
| `bench.yml` | `bench` | PR: `make bench-check`, compile and smoke-run every benchmark |
| `cross.yml` | `check (aarch64-unknown-linux-gnu)`, `check (armv7-unknown-linux-gnueabihf)` | Embedded cross checks, all features |
| `deny.yml` | `deny` | `make deny` (`cargo deny check`) on the pinned toolchain |
| `coverage.yml` | `coverage`, `coverage-macos` | `make coverage`, exact 100% per-file lines/functions/regions across the actual `src/` root, including test modules; implicit filename exclusions disabled |

## macOS 27 legs

Every macOS leg runs on the `xcode-27` hosted label: Apple Silicon (arm64) only, macOS 27, Xcode
27.0 to 27.2 installed with 27.1 the default (image
[readme](https://github.com/actions/runner-images/blob/main/images/macos/xcode-27-arm64-Readme.md),
[GA changelog](https://github.blog/changelog/2026-10-15-xcode-27-runner-image-generally-available/)).
GitHub does not publish a `macos-27` label: Xcode-based images are named for the Xcode major, not
the OS. Contexts never embed the label or version: matrix legs use the value `macos` and map it to
the label in `runs-on`, and the standalone job is `coverage-macos`. The next image bump edits the
workflows and `.github/actions/assert-xcode-27`, never the ruleset.

The composite action `.github/actions/assert-xcode-27` runs right after checkout in each leg, prints
`sw_vers` and `xcodebuild -version`, and fails unless the arch is `arm64` and the OS and Xcode majors
are both 27, so a retargeted label cannot silently gate on another image. `tests/repository_policy.rs`
requires it in every leg and rejects any other `macos*` matrix value or label.

The image is a scarce preview pool, so only gates that add information on macOS get a leg.

| Gate | macOS 27 leg | Why |
| --- | --- | --- |
| `clippy`, `native-check`, `test` | `macos` replaces `macos-latest` in the existing matrices | Same gates, newer image; no duplicate macOS 26 leg |
| `coverage` | `coverage-macos`, `make coverage` with `TETHERCAM_USBIP=0` | Holds `cfg(target_os = "macos")` code and macOS-only branches to the 100% bar; the USB/IP setup steps are Linux-only |
| `rustdoc` | none | `clippy` and `native-check` already compile the macOS `cfg` code with all features; only a macOS-only intra-doc link could differ |
| `bench` | none | `clippy --all-targets` compiles every benchmark on macOS; the smoke run is platform independent |
| `msrv` | none | Known gap: macOS-only `cfg` code is not checked on Rust 1.90; add a leg when macOS-only code grows |
| `fmt`, `policy` | none | `rustfmt` and the policy test read repository files and are platform independent; a second run is pure duplication |
| `deny` | none | `deny.toml` sets no `targets`, so the Linux job already audits the all-platform dependency graph; the pinned action is a Docker action that does not run on macOS |
| `cross` | none | Cross-compiles to Linux targets only; a macOS host adds nothing |

`TETHERCAM_USBIP` is Linux-only: VHCI and the USB/IP fixture do not exist on macOS, so
`src/transport/usb/nusb_backend.rs` (compiled on macOS, exercised only through USB/IP on Linux)
cannot be executed by `coverage-macos`. Every other `src/` file is at 100% on macOS 27. Until the
adapter is resolved (an owner decision: no exclusion is allowed) `coverage-macos` is red.
`tests/repository_policy.rs` requires every PR job in the payload, so the context is listed, but the
parent must not activate it while it is red.

## Transition from `macos-latest`

The live ruleset requires `clippy (macos-latest)`, `native-check (macos-latest)` and
`test (macos-latest)`. Matrix contexts are named after their axis value, so replacing the value
renames the context and a PR that renames them cannot go green under the live ruleset. The parent
edits the live ruleset in place before merging, once the new contexts have completed green on the
PR head (the `PUT` below, with the payload from this PR):

- Add `clippy (macos)`, `native-check (macos)`, `test (macos)`
- Add `coverage-macos` only once it is green; until then it is the one payload context left out of
  the live ruleset, so the read-back diff below is expected to show exactly that context
- Remove `clippy (macos-latest)`, `native-check (macos-latest)`, `test (macos-latest)`
- Every other context keeps its name

Open PRs must be rebased onto the merged workflows to report the new contexts.

All 17 checks are individual GitHub Actions jobs, not an optional aggregate. Actions app ID
`15368` was read from this repository's real check runs; every context is bound to it. No
nullable integration IDs. Benchmark timings on shared runners are not a threshold: full
benchmarks run on `main` and retain artifacts. `release.yml` is tag-only; its `gate` and
`publish` jobs cannot be required PR checks. Publishing still requires its own release gate
and environment. No release/dispatch-only status is included.

`tests/repository_policy.rs` parses every owned workflow and expands the current static
single-axis job matrices. It rejects context drift, missing/extra/duplicate contexts, unbound
apps, filtered PR triggers, conditional/dependent/optional jobs and unsupported matrix shapes.
It also checks the gate commands and required platforms. Tests run in `make test` (therefore
`make all`) and the dedicated `policy` job. The dev-only `serde_json` and `serde-saphyr`
dependencies parse JSON/YAML in Rust; they add no runtime dependency or native code.
`Cargo.toml` excludes this repository-only test from the published crate because its workflow,
ruleset and Makefile inputs are repository administration assets, not package functionality.
The test remains active in source checkouts and CI. Packaging validation must inspect the
actual `.crate` archive and run its extracted test suite, not just `cargo package`'s library build.

## Semantics and existing protection

- Target `~DEFAULT_BRANCH`, currently `main`, not a wildcard over contributor branches. A
  default-branch rename intentionally follows the new default; update workflow push branches
  at the same time. PR triggers are unfiltered, including PRs from forks. A fork's independent
  repository does not inherit this ruleset, but its PR into this repository does. First-time
  fork workflow approval can delay checks; do not bypass it or substitute skipped checks.
- `enforcement: active`, no bypass actors, including administrators. Block deletion and
  non-fast-forward updates; require linear history and resolved review conversations.
- Require PRs with **zero** required approvals, no code-owner or last-push approval requirement.
  This enables conversation resolution without adding a human-review blocker. It also blocks
  direct pushes, including administrator fast-forwards; land through the protected PR path.
- `strict_required_status_checks_policy: false` preserves classic protection's non-strict
  behavior. Independent green heads do not rerun solely because `main` advanced. This is not
  a combined-tree guarantee; gate an integration/rebase explicitly when a dependency needs it.
- `do_not_enforce_on_create: false` applies status checks to creation too.
- The initial read-only baseline was 13 classic contexts, admin enforcement enabled, linear
  history and conversation resolution required, force pushes and deletion disabled, no human
  approval requirement, and no repository rulesets. The payload keeps these restrictions and
  adds native checks and the policy check. Keep classic protection unchanged during activation
  and verification; both policies apply. This PR does not modify either live mechanism.

## Activate with gh

Run from a clean checkout of the merged policy commit. Use an authenticated repository
administrator (`gh auth status`); never print tokens. Review the exact payload first. Do not
activate while the new checks are nonexistent, pending, skipped, neutral or failing.

Before merging this PR, the parent records its exact head and requires all owned jobs to
finish successfully, not merely the old required subset:

```sh
set -eu
repo=nredd/tethercam
pr=123 # substitute the policy PR number
head=$(gh pr view "$pr" -R "$repo" --json headRefOid --jq .headRefOid)
gh pr checks "$pr" -R "$repo" --json name,state,link
# Save the actual check runs; include pagination, never infer success from a missing check.
gh api --paginate --slurp "repos/$repo/commits/$head/check-runs?per_page=100" > check-runs.json
jq -e --slurpfile policy .github/rulesets/main.json '
  [.[].check_runs[]] | sort_by(.id) | group_by([.name,.app.id]) | map(last) as $runs |
  $policy[0].rules[] | select(.type == "required_status_checks") |
  all(.parameters.required_status_checks[];
    . as $required | any($runs[];
      .name == $required.context and .app.id == $required.integration_id and
      .status == "completed" and .conclusion == "success"))
' check-runs.json
# Reconfirm headRefOid still equals $head; record final reviews and CI run IDs.
```

The check-run comparison selects the latest attempt for each name/app pair; an older success
cannot mask a newer failed attempt. The parent also inspects the current PR run set. After the PR lands, require the
same inventory green on the shipped policy commit and confirm the train is stable. Then:

```sh
set -eu
repo=nredd/tethercam
payload=.github/rulesets/main.json
cargo test --test repository_policy || exit 1
# Read-only baseline; keep this classic protection in place.
gh api "repos/$repo/branches/main/protection"
gh api --paginate "repos/$repo/rulesets?per_page=100"
# Idempotency by unique repository ruleset name. Stop on duplicates, never pick an arbitrary ID.
ids=$(gh api --paginate "repos/$repo/rulesets?per_page=100" \
  --jq '.[] | select(.name == "Main branch gates" and .source_type == "Repository") | .id') || exit 1
case "$ids" in
  '') gh api --method POST "repos/$repo/rulesets" --input "$payload" --jq .id ;;
  *'
'*) printf '%s\n' 'Duplicate ruleset names; inspect before updating' >&2; exit 1 ;;
  *) gh api --method PUT "repos/$repo/rulesets/$ids" --input "$payload" --jq .id ;;
esac
```

The POST/PUT prints the ruleset ID, not a token. Rerunning updates the one named ruleset;
never create another overlapping copy. An API error is a stop, not permission to weaken the
payload. Do not delete or disable classic protection as part of these commands.

## Read back and verify

Use the returned ID. This compares every owned field, allowing only server-supplied optional
PR parameters; rules and contexts are order-independent. Extra rule types still fail.

```sh
set -eu
id=123 # substitute the returned repository ruleset ID
repo=nredd/tethercam
gh api "repos/$repo/rulesets/$id" > live-ruleset.json
# Normalize expected and live JSON with the same projection.
for input in .github/rulesets/main.json live-ruleset.json; do
  jq -S '{name,target,enforcement,bypass_actors,conditions,rules: [
    .rules[] | if .type == "pull_request" then
      {type,parameters:(.parameters | {dismiss_stale_reviews_on_push,
        require_code_owner_review,require_last_push_approval,
        required_approving_review_count,required_review_thread_resolution})}
    elif .type == "required_status_checks" then
      {type,parameters:(.parameters | {strict_required_status_checks_policy,
        do_not_enforce_on_create,required_status_checks:(.required_status_checks | sort_by(.context))})}
    else . end] | sort_by(.type)}' "$input" > "$input.normalized"
done
diff -u .github/rulesets/main.json.normalized live-ruleset.json.normalized
# Inspect effective rules on the actual branch, including inherited policies.
gh api "repos/$repo/rules/branches/main"
gh api "repos/$repo/branches/main/protection"
```

Require an empty diff: all 17 contexts and Actions IDs, active enforcement, default-branch
inclusion with no exclusions, no bypass actors, no force pushes/deletion, linear history,
conversation resolution, zero mandatory approvals, and non-strict checks. Confirm classic
protection still matches the baseline. Keep read-back JSON outside Git or remove the temporary
files; only the reviewed payload is checked in. Parent owns activation and this receipt.
