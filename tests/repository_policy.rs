//! Validate the repository ruleset against the owned PR workflow inventory.
//!
//! This deliberately supports only our static jobs and single-axis matrices. New workflow
//! shapes must teach this validator their status names rather than silently escaping enforcement.
//!
//! References:
//! - <https://docs.github.com/en/rest/repos/rules#create-a-repository-ruleset>
//! - <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax>

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

type Workflows = Vec<(String, Value)>;

/// Read actual workflow YAML, including any newly added workflow files.
fn workflows() -> Result<Workflows, Box<dyn Error>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    let mut workflows = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("yml" | "yaml")
        ) {
            let name = path.file_name().ok_or("workflow has no filename")?;
            workflows.push((
                name.to_string_lossy().into_owned(),
                serde_saphyr::from_str(&fs::read_to_string(&path)?)?,
            ));
        }
    }
    Ok(workflows)
}

/// Turn a failed policy invariant into a useful validation error.
fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

/// Expand GitHub's default job names; reject unsupported or skippable shapes.
fn inventory(workflows: &Workflows) -> Result<BTreeSet<String>, String> {
    let mut contexts = BTreeSet::new();
    for (file, workflow) in workflows {
        let events = &workflow["on"];
        let events = events
            .as_object()
            .ok_or("workflow events must be a mapping")?;
        let Some(pr) = events.get("pull_request") else {
            require(
                file == "release.yml" && workflow.pointer("/on/push/tags") == Some(&json!(["v*"])),
                "only the tag-only release workflow is exempt from PR enforcement",
            )?;
            continue;
        };
        require(
            pr.is_null() || pr == &json!({}),
            "PR triggers must be unfiltered",
        )?;
        let jobs = workflow["jobs"].as_object().ok_or("missing jobs")?;
        require(!jobs.is_empty(), "workflow has no jobs")?;
        for (id, job) in jobs {
            require(
                job.get("if").is_none(),
                "required jobs must not be conditional",
            )?;
            require(
                job.get("needs").is_none(),
                "required jobs must run independently",
            )?;
            require(
                job.get("continue-on-error").is_none_or(|v| v == false),
                "required jobs must not tolerate failure",
            )?;
            require(
                job.get("uses").is_none(),
                "reusable workflow jobs need explicit inventory support",
            )?;
            require(
                job.get("name").is_none(),
                "explicit job names need explicit inventory support",
            )?;
            let name = id;
            let names = if let Some(strategy) = job.get("strategy") {
                require(
                    strategy["fail-fast"] == false,
                    "every matrix cell must complete",
                )?;
                let matrix = strategy["matrix"]
                    .as_object()
                    .ok_or("matrix must be static")?;
                require(matrix.len() == 1, "only single-axis matrices are supported")?;
                let (axis, values) = matrix.iter().next().ok_or("empty matrix")?;
                require(
                    axis != "include" && axis != "exclude",
                    "matrix include/exclude is unsupported",
                )?;
                let values = values.as_array().ok_or("matrix axis must be an array")?;
                require(!values.is_empty(), "matrix axis must not be empty")?;
                values
                    .iter()
                    .map(|value| {
                        let value = value.as_str().ok_or("matrix values must be strings")?;
                        require(!value.contains("${{"), "matrix values must be static")?;
                        Ok(format!("{name} ({value})"))
                    })
                    .collect::<Result<Vec<_>, String>>()?
            } else {
                vec![name.to_owned()]
            };
            for context in names {
                require(contexts.insert(context), "duplicate status context")?;
            }
        }
    }
    require(!contexts.is_empty(), "no PR checks found")?;
    Ok(contexts)
}

/// Validate the full request body, not just its context strings (also an executable schema).
fn validate(policy: &Value, workflows: &Workflows) -> Result<(), String> {
    let contexts = inventory(workflows)?;
    let mut expected_checks = contexts
        .iter()
        .map(|context| json!({"context": context, "integration_id": 15368}))
        .collect::<Vec<_>>();
    let mut normalized = policy.clone();
    let checks = normalized
        .pointer_mut("/rules/4/parameters/required_status_checks")
        .and_then(Value::as_array_mut)
        .ok_or("missing required status checks")?;
    checks.sort_by_key(|check| check["context"].as_str().unwrap_or_default().to_owned());
    expected_checks.sort_by_key(|check| check["context"].as_str().unwrap_or_default().to_owned());
    let expected = json!({
        "name": "Main branch gates", "target": "branch", "enforcement": "active",
        "bypass_actors": [],
        "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
        "rules": [
            {"type": "deletion"}, {"type": "non_fast_forward"},
            {"type": "required_linear_history"},
            {"type": "pull_request", "parameters": {
                "dismiss_stale_reviews_on_push": false, "require_code_owner_review": false,
                "require_last_push_approval": false, "required_approving_review_count": 0,
                "required_review_thread_resolution": true
            }},
            {"type": "required_status_checks", "parameters": {
                "strict_required_status_checks_policy": false,
                "do_not_enforce_on_create": false, "required_status_checks": expected_checks
            }}
        ]
    });
    require(
        normalized == expected,
        "ruleset differs from the exact owned workflow/restriction contract",
    )
}

/// Fail closed when release authentication, source checks or publication order drift.
fn validate_release(workflow: &Value) -> Result<(), String> {
    for (pointer, expected) in [
        ("/on/push/tags", json!(["v*"])),
        ("/permissions", json!({"contents": "read"})),
        ("/concurrency/cancel-in-progress", json!(false)),
        (
            "/jobs/gate/if",
            json!(
                "github.repository == 'nredd/tethercam' && startsWith(github.ref, 'refs/tags/v')"
            ),
        ),
        ("/jobs/gate/env/TETHERCAM_USBIP", json!(1)),
        ("/jobs/publish/needs", json!("gate")),
        ("/jobs/publish/environment", json!("crates-io")),
        (
            "/jobs/publish/permissions",
            json!({"contents": "read", "id-token": "write"}),
        ),
        ("/jobs/github-release/needs", json!("publish")),
        ("/jobs/github-release/environment", json!("crates-io")),
        (
            "/jobs/github-release/permissions",
            json!({"contents": "write"}),
        ),
    ] {
        require(
            workflow.pointer(pointer) == Some(&expected),
            "release security boundary changed",
        )?;
    }
    let events = workflow
        .get("on")
        .and_then(Value::as_object)
        .ok_or("missing events")?;
    require(events.len() == 1, "only release tag pushes may publish")?;
    let jobs = workflow
        .get("jobs")
        .and_then(Value::as_object)
        .ok_or("missing release jobs")?;
    require(jobs.len() == 3, "unexpected release privilege boundary")?;
    for job in jobs.values() {
        for step in job["steps"].as_array().ok_or("missing release steps")? {
            require(
                step.get("continue-on-error").is_none(),
                "release steps must fail closed",
            )?;
            if let Some(action) = step["uses"].as_str() {
                let (_, sha) = action.rsplit_once('@').ok_or("unpinned release action")?;
                require(
                    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "release actions must use full commit SHAs",
                )?;
            }
        }
    }
    for (job, commands) in [
        (
            "gate",
            vec![
                "git merge-base --is-ancestor HEAD origin/main",
                "cargo run --locked --features repo-tools --bin tethercam-tools -- release-tag \"$TAG\"",
                "cargo run --features repo-tools --bin tethercam-tools -- usbip prepare",
                "make all cross",
                "cargo package --locked",
                "cargo publish --dry-run --locked",
            ],
        ),
        (
            "publish",
            vec![
                "cargo publish --dry-run --locked",
                "cmp \"$RUNNER_TEMP\"/gated-package/*.crate target/package/*.crate",
                "cargo publish --locked",
            ],
        ),
        (
            "github-release",
            vec![
                "gh release create \"$TAG\" \"$RUNNER_TEMP\"/gated-package/*.crate --draft --verify-tag --target \"$COMMIT\" --title \"$TAG\" --notes-file CHANGELOG.md",
                "gh release edit \"$TAG\" --draft=false",
            ],
        ),
    ] {
        let steps = jobs
            .get(job)
            .and_then(|job| job.get("steps"))
            .and_then(Value::as_array)
            .ok_or("missing steps")?;
        let mut previous = None;
        for command in commands {
            let index = steps
                .iter()
                .position(|step| step["run"] == command && step.get("if").is_none())
                .ok_or("missing unconditional release command")?;
            require(
                previous.is_none_or(|previous| previous < index),
                "release commands must preserve validation and staging order",
            )?;
            previous = Some(index);
        }
    }
    Ok(())
}

#[test]
fn release_policy_preserves_source_auth_and_immutable_staging() -> Result<(), Box<dyn Error>> {
    let workflow: Value = serde_saphyr::from_str(include_str!("../.github/workflows/release.yml"))?;
    validate_release(&workflow)?;
    let ruleset: Value = serde_json::from_str(include_str!("../.github/rulesets/releases.json"))?;
    assert_eq!(
        ruleset,
        json!({
            "name": "Release tag integrity", "target": "tag", "enforcement": "active",
            "bypass_actors": [],
            "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": []}},
            "rules": [{"type": "update"}, {"type": "deletion"}]
        })
    );
    Ok(())
}

#[test]
fn weakened_release_boundaries_and_reordered_staging_fail() -> Result<(), Box<dyn Error>> {
    let original: Value = serde_saphyr::from_str(include_str!("../.github/workflows/release.yml"))?;
    for pointer in [
        "/on/push/tags",
        "/permissions",
        "/concurrency/cancel-in-progress",
        "/jobs/gate/if",
        "/jobs/gate/env/TETHERCAM_USBIP",
        "/jobs/publish/needs",
        "/jobs/publish/environment",
        "/jobs/publish/permissions",
        "/jobs/github-release/needs",
        "/jobs/github-release/environment",
        "/jobs/github-release/permissions",
    ] {
        let mut workflow = original.clone();
        *workflow
            .pointer_mut(pointer)
            .ok_or("missing release field")? = Value::Null;
        assert!(validate_release(&workflow).is_err(), "accepted {pointer}");
    }
    let mut workflow = original;
    let steps = workflow["jobs"]["github-release"]["steps"]
        .as_array_mut()
        .ok_or("missing release steps")?;
    steps.reverse();
    assert!(validate_release(&workflow).is_err());
    Ok(())
}

/// Load the exact activation payload used by the administrator.
fn policy() -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_str(include_str!(
        "../.github/rulesets/main.json"
    ))?)
}

#[test]
fn linux_usb_tests_and_coverage_require_the_shared_emulator_setup() -> Result<(), Box<dyn Error>> {
    let workflows = workflows()?;
    let coverage = &workflows
        .iter()
        .find(|(name, _)| name == "coverage.yml")
        .ok_or("missing coverage")?
        .1["jobs"]["coverage"];
    assert_eq!(coverage["env"]["TETHERCAM_USBIP"], json!(1));
    let steps = coverage["steps"]
        .as_array()
        .ok_or("missing coverage steps")?;
    assert!(steps.iter().any(|step| step["run"]
        == "cargo run --features repo-tools --bin tethercam-tools -- usbip prepare"
        && step.get("if").is_none()));
    assert!(steps.iter().any(|step| step["run"]
        == "cargo run --features repo-tools --bin tethercam-tools -- usbip detach"
        && step["if"] == "always()"));
    let tests = &workflows
        .iter()
        .find(|(name, _)| name == "test.yml")
        .ok_or("missing tests")?
        .1["jobs"]["test"];
    assert_eq!(
        tests["env"]["TETHERCAM_USBIP"],
        "${{ matrix.os != 'macos-latest' && '1' || '0' }}"
    );
    let steps = tests["steps"].as_array().ok_or("missing test steps")?;
    assert!(steps.iter().any(|step| {
        step["if"] == "runner.os == 'Linux'"
            && step["run"]
                == "cargo run --features repo-tools --bin tethercam-tools -- usbip prepare"
    }));
    assert!(steps.iter().any(|step| step["run"]
        == "cargo run --features repo-tools --bin tethercam-tools -- usbip detach"
        && step["if"] == "always() && runner.os == 'Linux'"));
    // All-feature make test executes native coverage acceptance crates, not only nextest.
    for job in [coverage, tests] {
        let steps = job["steps"]
            .as_array()
            .ok_or("missing native acceptance steps")?;
        assert!(steps.iter().any(|step| {
            step["with"]["components"]
                .as_str()
                .is_some_and(|c| c.contains("llvm-tools-preview"))
                && step.get("if").is_none()
        }));
        assert!(steps.iter().any(|step| {
            step["with"]["tool"] == "cargo-llvm-cov,cargo-nextest" && step.get("if").is_none()
        }));
    }
    Ok(())
}

#[test]
fn release_gate_prepares_and_cleans_up_native_usbip() -> Result<(), Box<dyn Error>> {
    let workflows = workflows()?;
    let release = &workflows
        .iter()
        .find(|(name, _)| name == "release.yml")
        .ok_or("missing release")?
        .1["jobs"]["gate"];
    assert_eq!(release["runs-on"], "ubuntu-latest");
    assert_eq!(release["env"]["TETHERCAM_USBIP"], json!(1));
    let steps = release["steps"].as_array().ok_or("missing release steps")?;
    let position = |command: &str| {
        steps
            .iter()
            .position(|step| step["run"] == command)
            .ok_or("missing release command")
    };
    let prepare =
        position("cargo run --features repo-tools --bin tethercam-tools -- usbip prepare")?;
    let gate = position("make all cross")?;
    let package = position("cargo package --locked")?;
    let dry_run = position("cargo publish --dry-run --locked")?;
    let cleanup =
        position("cargo run --features repo-tools --bin tethercam-tools -- usbip detach")?;
    assert!(prepare < gate && gate < package && package < dry_run && dry_run < cleanup);
    for index in [prepare, gate, package, dry_run] {
        assert!(steps[index].get("if").is_none());
        assert!(steps[index].get("continue-on-error").is_none());
    }
    assert_eq!(steps[cleanup]["if"], "always()");
    assert!(steps.iter().any(|step| {
        step["with"]["components"] == "clippy, rustfmt, llvm-tools-preview"
            && step.get("if").is_none()
    }));
    assert!(steps.iter().any(|step| {
        step["with"]["tool"] == "cargo-nextest,cargo-deny,cargo-llvm-cov"
            && step.get("if").is_none()
    }));
    Ok(())
}

#[test]
fn checked_in_policy_matches_every_owned_pr_job() -> Result<(), Box<dyn Error>> {
    validate(&policy()?, &workflows()?)?;
    Ok(())
}

#[test]
fn gate_commands_and_platforms_remain_present() -> Result<(), Box<dyn Error>> {
    let workflows = workflows()?;
    // These are the make-all gates plus the extra portability/MSRV/policy gates.
    let gates = [
        ("fmt.yml", "fmt", "make fmt-check", None),
        (
            "lint.yml",
            "clippy",
            "make lint",
            Some(json!({"os": ["ubuntu-latest", "macos-latest"]})),
        ),
        (
            "check.yml",
            "native-check",
            "make check",
            Some(json!({"os": ["ubuntu-latest", "macos-latest"]})),
        ),
        (
            "test.yml",
            "test",
            "make test",
            Some(json!({"os": ["ubuntu-latest", "ubuntu-24.04-arm", "macos-latest"]})),
        ),
        ("docs.yml", "rustdoc", "make doc", None),
        ("bench.yml", "bench", "make bench-check", None),
        ("coverage.yml", "coverage", "make coverage", None),
        ("deny.yml", "deny", "make deny", None),
        (
            "cross.yml",
            "check",
            "cargo check --all-features --target ${{ matrix.target }}",
            Some(json!({"target": ["aarch64-unknown-linux-gnu", "armv7-unknown-linux-gnueabihf"]})),
        ),
        (
            "test.yml",
            "msrv",
            "cargo check --all-features --lib --bins",
            None,
        ),
        (
            "test.yml",
            "policy",
            "cargo test --test repository_policy",
            None,
        ),
    ];
    for (file, id, command, matrix) in gates {
        let (_, workflow) = workflows
            .iter()
            .find(|(name, _)| name == file)
            .ok_or("missing gate workflow")?;
        let job = &workflow["jobs"][id];
        assert_eq!(
            job["strategy"]["matrix"],
            matrix.unwrap_or(Value::Null),
            "{file}/{id}"
        );
        let steps = job["steps"].as_array().ok_or("missing steps")?;
        let step = steps
            .iter()
            .find(|step| step["run"] == command)
            .ok_or("missing gate command")?;
        if id == "bench" {
            assert_eq!(step["if"], "github.event_name == 'pull_request'");
        } else {
            assert!(step.get("if").is_none(), "gate step must be unconditional");
        }
        assert!(
            step.get("continue-on-error").is_none(),
            "gate step must not tolerate failure"
        );
    }
    let makefile = include_str!("../Makefile");
    assert!(makefile.lines().any(|line| {
        line.starts_with("all: fmt-check lint check test doc bench-check deny coverage ")
    }));
    Ok(())
}

#[test]
fn repository_only_test_is_excluded_from_publication() -> Result<(), Box<dyn Error>> {
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml"))?;
    let excludes = manifest["package"]["exclude"]
        .as_array()
        .ok_or("excludes")?;
    assert!(
        excludes
            .iter()
            .any(|path| path.as_str() == Some("tests/repository_policy.rs"))
    );
    assert!(
        excludes
            .iter()
            .any(|path| path.as_str() == Some("rust-toolchain.toml")),
        "the development pin must not ship in the crate"
    );
    for path in ["tests/usb_emulated.rs", "tests/usbip/", "src/test_support/"] {
        assert!(!excludes.iter().any(|value| value.as_str() == Some(path)));
    }
    assert!(manifest["features"].get("test-support").is_some());
    assert_eq!(
        manifest["package"]["default-run"].as_str(),
        Some("tethercam")
    );
    Ok(())
}

#[test]
fn bad_missing_extra_duplicate_and_unbound_contexts_fail() -> Result<(), Box<dyn Error>> {
    let workflows = workflows()?;
    for mutation in ["bad", "missing", "extra", "duplicate", "unbound"] {
        let mut policy = policy()?;
        let checks = policy["rules"][4]["parameters"]["required_status_checks"]
            .as_array_mut()
            .ok_or("checks")?;
        match mutation {
            "bad" => checks[0]["context"] = json!("format-typo"),
            "missing" => {
                checks.remove(0);
            }
            "extra" => checks.push(json!({"context": "publish", "integration_id": 15368})),
            "duplicate" => checks.push(checks[0].clone()),
            "unbound" => checks[0]["integration_id"] = Value::Null,
            _ => unreachable!(),
        }
        assert!(
            validate(&policy, &workflows).is_err(),
            "accepted {mutation}"
        );
    }
    Ok(())
}

#[test]
fn weakened_or_malformed_policy_fails() -> Result<(), Box<dyn Error>> {
    let workflows = workflows()?;
    for pointer in [
        "/enforcement",
        "/target",
        "/bypass_actors",
        "/conditions/ref_name/include",
        "/conditions/ref_name/exclude",
        "/rules/0/type",
        "/rules/1/type",
        "/rules/2/type",
        "/rules/3/parameters/required_review_thread_resolution",
        "/rules/3/parameters/required_approving_review_count",
        "/rules/4/parameters/strict_required_status_checks_policy",
        "/rules/4/parameters/do_not_enforce_on_create",
    ] {
        let mut policy = policy()?;
        *policy.pointer_mut(pointer).ok_or("missing policy field")? = Value::Null;
        assert!(validate(&policy, &workflows).is_err(), "accepted {pointer}");
    }
    assert!(validate(&json!({}), &workflows).is_err());
    assert!(serde_json::from_str::<Value>("{invalid json}").is_err());
    Ok(())
}

#[test]
fn workflow_drift_and_skippable_checks_fail() -> Result<(), Box<dyn Error>> {
    let original = workflows()?;
    for replacement in [
        json!({"on": {"pull_request": {"paths": ["src/**"]}}, "jobs": {"new": {}}}),
        json!({"on": {"workflow_dispatch": null}, "jobs": {"new": {}}}),
        json!({"on": {"pull_request": null}, "jobs": {"new": {}}}),
        json!({"on": {"pull_request": null}, "jobs": {"fmt": {}}}),
        json!({"on": {"pull_request": null}, "jobs": {"new": {"if": "false"}}}),
        json!({"on": {"pull_request": null}, "jobs": {"new": {"continue-on-error": true}}}),
        json!({"on": {"pull_request": null}, "jobs": {"new": {"needs": "fmt"}}}),
        json!({"on": {"pull_request": null}, "jobs": {"new": {"name": "custom"}}}),
        json!({"on": {"pull_request": null}, "jobs": {"new": {"strategy": {"matrix": {"os": ["linux"], "arch": ["arm"]}}}}}),
    ] {
        let mut workflows = original.clone();
        workflows.push(("new.yml".into(), replacement));
        assert!(validate(&policy()?, &workflows).is_err());
    }
    let mut workflows = original;
    let (_, lint) = workflows
        .iter_mut()
        .find(|(name, _)| name == "lint.yml")
        .ok_or("lint")?;
    lint["jobs"]["clippy"]["strategy"]["matrix"]["os"][0] = json!("ubuntu-renamed");
    assert!(validate(&policy()?, &workflows).is_err());
    Ok(())
}

const TOOLCHAIN_ACTION: &str = "dtolnay/rust-toolchain@";

/// Whether a step is the toolchain installer.
fn is_toolchain_step(step: &Value) -> bool {
    step["uses"]
        .as_str()
        .is_some_and(|uses| uses.starts_with(TOOLCHAIN_ACTION))
}

/// JSON pointer to a job's `run` step containing `needle`.
fn run_pointer(
    workflows: &Workflows, file: &str, job: &str, needle: &str,
) -> Result<String, String> {
    let (_, workflow) = workflows
        .iter()
        .find(|(n, _)| n == file)
        .ok_or("workflow")?;
    let steps = workflow["jobs"][job]["steps"].as_array().ok_or("steps")?;
    let index = steps
        .iter()
        .position(|s| s["run"].as_str().is_some_and(|r| r.contains(needle)))
        .ok_or("run step")?;
    Ok(format!("/jobs/{job}/steps/{index}/run"))
}

/// JSON pointer to a job's toolchain step, so mutations survive step reordering.
fn toolchain_pointer(workflows: &Workflows, file: &str, job: &str) -> Result<String, String> {
    let (_, workflow) = workflows
        .iter()
        .find(|(n, _)| n == file)
        .ok_or("workflow")?;
    let steps = workflow["jobs"][job]["steps"].as_array().ok_or("steps")?;
    let index = steps
        .iter()
        .position(is_toolchain_step)
        .ok_or("toolchain step")?;
    Ok(format!("/jobs/{job}/steps/{index}"))
}

/// Split a comma-separated action input into trimmed non-empty items.
fn csv(value: &str) -> BTreeSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Words of a shell script, split on whitespace and the usual command separators.
fn shell_words(script: &str) -> Vec<&str> {
    script
        .split(|c: char| c.is_whitespace() || ";&|()`".contains(c))
        .filter(|word| !word.is_empty())
        .collect()
}

/// Whether a script runs `rustup` (by any path), sets `RUSTUP_TOOLCHAIN`, or passes a
/// `+toolchain` override to `cargo`/`rustc`. Not a shell parser; it catches the plausible drift.
fn selects_toolchain(script: &str) -> bool {
    let words = shell_words(script);
    let base = |word: &str| word.rsplit('/').next().unwrap_or(word).to_owned();
    words
        .iter()
        .any(|w| base(w) == "rustup" || w.starts_with("RUSTUP_TOOLCHAIN="))
        || words.windows(2).any(|w| {
            matches!(base(w[0]).as_str(), "cargo" | "rustc" | "rustdoc") && w[1].starts_with('+')
        })
}

/// Reject toolchain selection inside any Makefile recipe line.
fn validate_recipes(makefile: &str) -> Result<(), String> {
    for line in makefile.lines().filter(|line| line.starts_with('\t')) {
        require(
            !selects_toolchain(line.trim_start().trim_start_matches(['@', '-', '+'])),
            &format!(
                "Makefile recipe selects a toolchain by hand: {}",
                line.trim()
            ),
        )?;
    }
    Ok(())
}

/// Actions whose names suggest they set up a Rust toolchain.
fn is_other_toolchain_setup(step: &Value) -> bool {
    step["uses"].as_str().is_some_and(|uses| {
        let uses = uses.to_ascii_lowercase();
        !is_toolchain_step(step)
            && ["toolchain", "rustup", "setup-rust", "rust-install"]
                .iter()
                .any(|hint| uses.contains(hint))
    })
}

/// A job that builds must install the pin exactly once, with no other setup action and no
/// hand-picked toolchain.
fn check_job_hygiene(at: &str, steps: &[Value]) -> Result<(), String> {
    let runs: Vec<&str> = steps.iter().filter_map(|s| s["run"].as_str()).collect();
    let installs = steps.iter().filter(|s| is_toolchain_step(s)).count();
    let builds = runs
        .iter()
        .any(|r| r.contains("cargo ") || r.contains("make "));
    require(
        installs > 0 || !builds,
        &format!("{at}: runs cargo without installing the pinned toolchain"),
    )?;
    require(
        installs <= 1,
        &format!("{at}: more than one toolchain setup step"),
    )?;
    require(
        !steps.iter().any(is_other_toolchain_setup),
        &format!("{at}: a different toolchain setup action"),
    )?;
    require(
        runs.iter().all(|run| !selects_toolchain(run)),
        &format!("{at}: a run step selects a toolchain by hand"),
    )?;
    require(
        steps
            .iter()
            .all(|s| s["with"].get("component").is_none() && s["with"].get("target").is_none()),
        &format!("{at}: use the plural `components`/`targets` inputs"),
    )
}

/// The pin from `rust-toolchain.toml` plus the MSRV from `Cargo.toml`.
struct Pin {
    channel: String,
    msrv: String,
    components: BTreeSet<String>,
    targets: BTreeSet<String>,
}

/// Parse and sanity-check the pin: an exact release that is not the MSRV.
fn parse_pin(toolchain_file: &str, manifest: &str) -> Result<Pin, String> {
    let file: toml::Value = toml::from_str(toolchain_file).map_err(|e| e.to_string())?;
    let table = file.get("toolchain").ok_or("missing [toolchain]")?;
    let channel = table
        .get("channel")
        .and_then(toml::Value::as_str)
        .ok_or("missing channel")?;
    let is_version =
        |v: &str| v.split('.').count() == 3 && v.split('.').all(|p| p.parse::<u32>().is_ok());
    require(
        is_version(channel),
        "channel must be an exact x.y.z release",
    )?;
    let listed = |key: &str| -> BTreeSet<String> {
        table
            .get(key)
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let manifest: toml::Value = toml::from_str(manifest).map_err(|e| e.to_string())?;
    let msrv = manifest
        .get("package")
        .and_then(|package| package.get("rust-version"))
        .and_then(toml::Value::as_str)
        .ok_or("missing rust-version")?;
    require(
        channel != msrv && !channel.starts_with(&format!("{msrv}.")),
        "the pin must not be the MSRV",
    )?;
    Ok(Pin {
        channel: channel.to_owned(),
        msrv: msrv.to_owned(),
        components: listed("components"),
        targets: listed("targets"),
    })
}

/// One toolchain step. Components and targets must equal the file's lists exactly so rustup
/// never fetches anything lazily on the first `cargo` call.
fn check_toolchain_step(at: &str, job: &Value, step: &Value, pin: &Pin) -> Result<(), String> {
    let wanted = step["with"]["toolchain"]
        .as_str()
        .ok_or(format!("{at}: no toolchain"))?;
    if at == "test.yml/msrv" {
        require(wanted == pin.msrv, "msrv job must install rust-version")?;
        return require(
            job["env"]["RUSTUP_TOOLCHAIN"].as_str() == Some(&pin.msrv),
            "msrv job must set RUSTUP_TOOLCHAIN to rust-version",
        );
    }
    require(
        wanted == pin.channel,
        &format!("{at}: toolchain {wanted} != pin {}", pin.channel),
    )?;
    require(
        csv(step["with"]["components"].as_str().unwrap_or_default()) == pin.components,
        &format!("{at}: components differ from rust-toolchain.toml"),
    )?;
    require(
        csv(step["with"]["targets"].as_str().unwrap_or_default()) == pin.targets,
        &format!("{at}: targets differ from rust-toolchain.toml"),
    )
}

/// Every toolchain step must install the pin from `rust-toolchain.toml`, except the `msrv` job,
/// which installs `rust-version` and must also override rustup with `RUSTUP_TOOLCHAIN`.
fn validate_toolchains(
    workflows: &Workflows, toolchain_file: &str, manifest: &str,
) -> Result<(), String> {
    let pin = parse_pin(toolchain_file, manifest)?;
    let mut seen = 0;
    for (file, workflow) in workflows {
        for (id, job) in workflow["jobs"].as_object().ok_or("missing jobs")? {
            let steps = job["steps"].as_array().map_or(&[][..], Vec::as_slice);
            let at = format!("{file}/{id}");
            check_job_hygiene(&at, steps)?;
            for step in steps.iter().filter(|step| is_toolchain_step(step)) {
                seen += 1;
                check_toolchain_step(&at, job, step, &pin)?;
            }
        }
        let overrides = workflow.to_string().matches("RUSTUP_TOOLCHAIN").count();
        require(
            overrides == usize::from(file == "test.yml"),
            &format!("{file}: RUSTUP_TOOLCHAIN is only for the msrv job"),
        )?;
    }
    require(seen > 0, "no toolchain steps found")
}

#[test]
fn workflow_toolchains_match_rust_toolchain_file() -> Result<(), Box<dyn Error>> {
    validate_toolchains(
        &workflows()?,
        include_str!("../rust-toolchain.toml"),
        include_str!("../Cargo.toml"),
    )?;
    validate_recipes(include_str!("../Makefile"))?;
    Ok(())
}

#[test]
fn toolchain_drift_fails() -> Result<(), Box<dyn Error>> {
    let file = include_str!("../rust-toolchain.toml");
    let manifest = include_str!("../Cargo.toml");
    let original = workflows()?;
    let mutations: [(&str, &str, &str, Value); 5] = [
        (
            "coverage.yml",
            "coverage",
            "/with/toolchain",
            json!("stable"),
        ),
        ("release.yml", "gate", "/with/toolchain", json!("1.99.1")),
        ("lint.yml", "clippy", "/with/components", json!("miri")),
        (
            "cross.yml",
            "check",
            "/with/targets",
            json!("wasm32-unknown-unknown"),
        ),
        ("fmt.yml", "fmt", "/with/components", json!("clippy")),
    ];
    for (name, job, tail, value) in mutations {
        let mut workflows = original.clone();
        let pointer = format!("{}{tail}", toolchain_pointer(&workflows, name, job)?);
        let (_, workflow) = workflows
            .iter_mut()
            .find(|(n, _)| n == name)
            .ok_or("workflow")?;
        *workflow.pointer_mut(&pointer).ok_or(pointer.clone())? = value;
        assert!(
            validate_toolchains(&workflows, file, manifest).is_err(),
            "accepted {name} {pointer}"
        );
    }
    Ok(())
}

#[test]
fn toolchain_overrides_and_missing_installs_fail() -> Result<(), Box<dyn Error>> {
    let file = include_str!("../rust-toolchain.toml");
    let manifest = include_str!("../Cargo.toml");
    let original = workflows()?;
    let plain: [(&str, String, Value); 8] = [
        (
            "test.yml",
            "/jobs/msrv/env/RUSTUP_TOOLCHAIN".into(),
            json!("1.100.0"),
        ),
        ("test.yml", "/jobs/msrv/env".into(), json!({})),
        ("lint.yml", "/env/RUSTUP_TOOLCHAIN".into(), json!("stable")),
        (
            "docs.yml",
            "/jobs/rustdoc/env".into(),
            json!({"RUSTUP_TOOLCHAIN": "stable"}),
        ),
        (
            "docs.yml",
            run_pointer(&original, "docs.yml", "rustdoc", "make doc")?,
            json!("cargo +stable doc"),
        ),
        (
            "bench.yml",
            run_pointer(&original, "bench.yml", "bench", "make bench-check")?,
            json!("rustup default stable"),
        ),
        (
            "check.yml",
            run_pointer(&original, "check.yml", "native-check", "make check")?,
            json!("cargo  +nightly check"),
        ),
        (
            "release.yml",
            format!(
                "{}/with/targets",
                toolchain_pointer(&original, "release.yml", "gate")?
            ),
            json!(""),
        ),
    ];
    for (name, pointer, value) in plain {
        let mut workflows = original.clone();
        let (_, workflow) = workflows
            .iter_mut()
            .find(|(n, _)| n == name)
            .ok_or("workflow")?;
        if let Some(slot) = workflow.pointer_mut(&pointer) {
            *slot = value;
        } else {
            let (parent, key) = pointer.rsplit_once('/').ok_or("pointer")?;
            workflow
                .pointer_mut(parent)
                .and_then(Value::as_object_mut)
                .ok_or(pointer.clone())?
                .insert(key.to_owned(), value);
        }
        assert!(
            validate_toolchains(&workflows, file, manifest).is_err(),
            "accepted {name} {pointer}"
        );
    }
    // A job that builds without installing the pin, and an MSRV job without its override.
    let mut workflows = original.clone();
    let pointer = toolchain_pointer(&workflows, "docs.yml", "rustdoc")?;
    let (_, docs) = workflows
        .iter_mut()
        .find(|(n, _)| n == "docs.yml")
        .ok_or("docs")?;
    let index: usize = pointer.rsplit('/').next().ok_or("index")?.parse()?;
    docs["jobs"]["rustdoc"]["steps"]
        .as_array_mut()
        .ok_or("steps")?
        .remove(index);
    assert!(validate_toolchains(&workflows, file, manifest).is_err());
    Ok(())
}

/// Point every non-msrv toolchain step at `channel`, as a consistent bump would.
fn retarget(workflows: &mut Workflows, channel: &str) {
    for (_, workflow) in workflows.iter_mut() {
        let Some(jobs) = workflow["jobs"].as_object_mut() else {
            continue;
        };
        for job in jobs.values_mut() {
            if job["env"].get("RUSTUP_TOOLCHAIN").is_some() {
                continue;
            }
            for step in job["steps"].as_array_mut().into_iter().flatten() {
                if is_toolchain_step(step) {
                    step["with"]["toolchain"] = json!(channel);
                }
            }
        }
    }
}

/// Assert that validation fails with a message containing `expected`.
fn assert_rejected(result: Result<(), String>, expected: &str) {
    match result {
        Err(message) => assert!(message.contains(expected), "wrong error: {message}"),
        Ok(()) => panic!("accepted, wanted: {expected}"),
    }
}

#[test]
fn toolchain_file_drift_fails() -> Result<(), Box<dyn Error>> {
    let file = include_str!("../rust-toolchain.toml");
    let manifest = include_str!("../Cargo.toml");
    let original = workflows()?;
    let channel = file
        .lines()
        .find_map(|line| line.strip_prefix("channel = \"")?.strip_suffix('"'))
        .ok_or("channel")?;
    let msrv = toml::from_str::<toml::Value>(manifest)?["package"]["rust-version"]
        .as_str()
        .ok_or("rust-version")?
        .to_owned();
    // The file moves alone: workflows still name the old pin.
    let moved = file.replace(channel, "9.9.9");
    assert_rejected(
        validate_toolchains(&original, &moved, manifest),
        "!= pin 9.9.9",
    );
    // The file and every workflow move together, so only the pin guards can reject it.
    for (bad, expected) in [
        ("stable", "channel must be an exact x.y.z release"),
        (&format!("{msrv}.0"), "the pin must not be the MSRV"),
    ] {
        let mut workflows = original.clone();
        retarget(&mut workflows, bad);
        assert_rejected(
            validate_toolchains(&workflows, &file.replace(channel, bad), manifest),
            expected,
        );
    }
    Ok(())
}

/// Append `step` to a job of a workflow.
fn push_step(workflows: &mut Workflows, name: &str, job: &str, step: Value) -> Result<(), String> {
    let (_, workflow) = workflows
        .iter_mut()
        .find(|(n, _)| n == name)
        .ok_or("workflow")?;
    workflow["jobs"][job]["steps"]
        .as_array_mut()
        .ok_or("steps")?
        .push(step);
    Ok(())
}

#[test]
fn hand_selected_and_extra_toolchain_setup_fails() -> Result<(), Box<dyn Error>> {
    let file = include_str!("../rust-toolchain.toml");
    let manifest = include_str!("../Cargo.toml");
    let original = workflows()?;
    let steps = [
        (
            "lint.yml",
            "clippy",
            json!({"run": "x;rustup default stable"}),
        ),
        (
            "lint.yml",
            "clippy",
            json!({"run": "$HOME/.cargo/bin/rustup default stable"}),
        ),
        ("fmt.yml", "fmt", json!({"run": "(cargo +nightly fmt)"})),
        (
            "fmt.yml",
            "fmt",
            json!({"run": "RUSTUP_TOOLCHAIN=stable cargo fmt"}),
        ),
        (
            "docs.yml",
            "rustdoc",
            json!({"uses": "actions-rs/toolchain@v1", "with": {"toolchain": "stable"}}),
        ),
        (
            "docs.yml",
            "rustdoc",
            json!({"uses": "moonrepo/setup-rust@v1"}),
        ),
    ];
    for (name, job, step) in steps {
        let mut workflows = original.clone();
        push_step(&mut workflows, name, job, step.clone())?;
        assert!(
            validate_toolchains(&workflows, file, manifest).is_err(),
            "accepted {step}"
        );
    }
    // A second copy of the pinned installer is still a second setup step.
    let mut workflows = original.clone();
    let pointer = toolchain_pointer(&workflows, "fmt.yml", "fmt")?;
    let installer = original
        .iter()
        .find(|(n, _)| n == "fmt.yml")
        .and_then(|(_, w)| w.pointer(&pointer))
        .ok_or("installer")?
        .clone();
    push_step(&mut workflows, "fmt.yml", "fmt", installer)?;
    assert_rejected(
        validate_toolchains(&workflows, file, manifest),
        "more than one toolchain setup step",
    );
    // Makefile recipes are scanned too.
    let makefile = include_str!("../Makefile");
    validate_recipes(makefile)?;
    for bad in [
        "\tcargo +nightly check",
        "\t@rustup default stable",
        "\t-/usr/bin/rustup run stable cargo check",
    ] {
        assert!(
            validate_recipes(&format!("{makefile}\n{bad}\n")).is_err(),
            "accepted {bad}"
        );
    }
    Ok(())
}
