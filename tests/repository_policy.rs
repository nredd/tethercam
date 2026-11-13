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
            step["with"]["components"] == "llvm-tools-preview" && step.get("if").is_none()
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
    let gate = position("make all")?;
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
        step["with"]["components"] == "rustfmt, clippy, llvm-tools-preview"
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
    let (_, deny) = workflows
        .iter()
        .find(|(name, _)| name == "deny.yml")
        .ok_or("missing deny")?;
    assert!(
        deny["jobs"]["deny"]["steps"]
            .as_array()
            .ok_or("missing deny steps")?
            .iter()
            .any(|step| {
                step["uses"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("EmbarkStudios/cargo-deny-action@"))
                    && step["with"]["command"] == "check"
                    && step.get("if").is_none()
                    && step.get("continue-on-error").is_none()
            })
    );
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
