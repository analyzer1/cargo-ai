//! Combine actual workflow jobs, probe records and the candidate's package catalog.

use super::qualification_catalog::{repository, Catalog};
use super::qualification_policy::{self as policy, Identity, Record, Result, Status, PROVIDERS};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};

#[derive(Deserialize)]
struct Jobs {
    jobs: Vec<Job>,
}
#[derive(Deserialize)]
struct Job {
    name: String,
    head_sha: String,
    run_attempt: u64,
    status: String,
    conclusion: Option<String>,
    completed_at: Option<String>,
    html_url: Option<String>,
}
#[derive(Deserialize)]
struct Need {
    result: String,
    outputs: BTreeMap<String, String>,
}

pub struct Inputs {
    pub env: BTreeMap<String, String>,
    pub jobs: String,
    pub catalog: String,
}
impl Inputs {
    fn get(&self, key: &str) -> &str {
        self.env.get(key).map(String::as_str).unwrap_or("")
    }
}

struct Evidence {
    status: Status,
    completed: String,
    links: String,
    attempt: u64,
    absent: bool,
}
impl Evidence {
    fn missing(url: &str, attempt: u64) -> Self {
        Self {
            status: Status::Missing,
            completed: "—".into(),
            links: format!("[Workflow run]({url})"),
            attempt,
            absent: false,
        }
    }
}

fn aggregate(
    jobs: &[Job],
    suffixes: &[(&str, String)],
    trigger: &str,
    attempt: u64,
    url: &str,
) -> Evidence {
    let mut selected = Vec::new();
    for (label, suffix) in suffixes {
        let matches: Vec<_> = jobs
            .iter()
            .filter(|job| {
                job.name.ends_with(suffix)
                    && job.head_sha == trigger
                    && (1..=attempt).contains(&job.run_attempt)
            })
            .collect();
        let Some(latest) = matches.iter().map(|j| j.run_attempt).max() else {
            let mut missing = Evidence::missing(url, attempt);
            missing.absent = !jobs.iter().any(|job| job.name.ends_with(suffix));
            return missing;
        };
        let latest: Vec<_> = matches
            .into_iter()
            .filter(|j| j.run_attempt == latest)
            .collect();
        if latest.len() != 1 || latest[0].status != "completed" {
            return Evidence::missing(url, attempt);
        }
        selected.push((*label, latest[0]));
    }
    let conclusions: Vec<_> = selected
        .iter()
        .map(|(_, j)| match j.conclusion.as_deref() {
            Some("success") => Status::Pass,
            Some("cancelled") => Status::Cancelled,
            Some("skipped") => Status::Skipped,
            Some(
                "action_required" | "failure" | "neutral" | "stale" | "startup_failure"
                | "timed_out",
            ) => Status::Fail,
            _ => Status::Missing,
        })
        .collect();
    let status = [
        Status::Missing,
        Status::Fail,
        Status::Cancelled,
        Status::Skipped,
    ]
    .into_iter()
    .find(|status| conclusions.contains(status))
    .unwrap_or(Status::Pass);
    let prefix = format!("{url}/job/");
    Evidence {
        status,
        completed: selected
            .iter()
            .filter_map(|(_, j)| j.completed_at.as_deref())
            .filter(|value| {
                !value.is_empty()
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b"T:.-+Z".contains(&b))
            })
            .max()
            .unwrap_or("—")
            .into(),
        links: selected
            .iter()
            .map(|(label, job)| {
                let link = job
                    .html_url
                    .as_deref()
                    .filter(|v| {
                        v.strip_prefix(&prefix)
                            .is_some_and(|id| policy::positive(id).is_ok())
                    })
                    .unwrap_or(url);
                format!("[{label}]({link})")
            })
            .collect::<Vec<_>>()
            .join(" · "),
        attempt: selected
            .iter()
            .map(|(_, j)| j.run_attempt)
            .max()
            .unwrap_or(attempt),
        absent: false,
    }
}

fn cell(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "\\|")
        .replace('\r', " ")
        .replace('\n', "<br>")
}
fn row(lines: &mut Vec<String>, values: &[&str]) {
    lines.push(format!(
        "| {} |",
        values
            .iter()
            .map(|v| cell(v))
            .collect::<Vec<_>>()
            .join(" | ")
    ));
}

pub const BLOCKED: &str = "> ❌ **BLOCKED** — at least one required result failed, was cancelled, skipped, or is missing.";

pub fn render(input: &Inputs) -> Result<(String, bool)> {
    let repo = input.get("GITHUB_REPOSITORY");
    let candidate = input.get("CARGO_AI_SHA");
    let trigger = input.get("TRUSTED_TRIGGER_SHA");
    let run_id = input.get("GITHUB_RUN_ID");
    let attempt = policy::positive(input.get("GITHUB_RUN_ATTEMPT"))?;
    policy::positive(run_id)?;
    policy::positive(input.get("GITHUB_RUN_NUMBER"))?;
    if !repository(repo) || !policy::hexadecimal(candidate, 40) || !policy::hexadecimal(trigger, 40)
    {
        return Err("invalid qualification identity");
    }
    let url = format!("https://github.com/{repo}/actions/runs/{run_id}");
    let parsed_jobs = serde_json::from_str::<Jobs>(&input.jobs);
    let jobs_ok = input.get("JOBS_API_STATUS") == "0" && parsed_jobs.is_ok();
    let jobs = parsed_jobs.map(|p| p.jobs).unwrap_or_default();
    let family = |name: &str| {
        aggregate(
            &jobs,
            &[
                ("Ubuntu", format!("{name} (ubuntu-latest)")),
                ("macOS", format!("{name} (macos-latest)")),
                ("Windows", format!("{name} (windows-latest)")),
            ],
            trigger,
            attempt,
            &url,
        )
    };
    let deterministic = family("Deterministic qualification");
    let mut package = family("Source package qualification");
    let catalog = Catalog::parse(&input.catalog);
    let canary = catalog.as_ref().ok().and_then(|c| c.canary().ok());
    let catalog_ok = input.get("CATALOG_CHECKOUT_OUTCOME") == "success" && canary.is_some();
    let canary_scope = if catalog_ok {
        let c = canary.unwrap();
        format!(
            "[{}@{}](https://github.com/{}/tree/{}); {} platforms",
            c.repository,
            &c.revision[..12],
            c.repository,
            c.revision,
            c.platforms.len()
        )
    } else {
        package.status = Status::Missing;
        "Qualification catalog unavailable or invalid".into()
    };
    let count = catalog
        .as_ref()
        .ok()
        .filter(|_| catalog_ok)
        .map(|c| c.official_package_count);
    let official_ok = count == Some(0);
    let official_scope = match count {
        Some(0) => "No registered official packages".into(),
        Some(n) => format!("{n} registered; aggregate results required"),
        None => "Registered count unavailable".into(),
    };
    let official = Evidence { status: if official_ok { Status::Skipped } else { Status::Missing },
        completed: "—".into(), links: format!("[Qualification catalog](https://github.com/{repo}/blob/{candidate}/.github/package-qualification-catalog.toml)"), attempt, absent: false };
    let needs = serde_json::from_str::<BTreeMap<String, Need>>(input.get("PROVIDER_PROBE_RECORDS"));
    let mut providers_ok = needs.is_ok();
    let needs = needs.unwrap_or_default();
    let mut seen = HashSet::new();
    let mut providers = Vec::new();
    for provider in PROVIDERS {
        let label = policy::label(provider);
        let mut evidence = aggregate(
            &jobs,
            &[(label, format!("Live {label} conformance"))],
            trigger,
            attempt,
            &url,
        );
        let enrollment = input.get(&format!("LIVE_{}_ENABLED", provider.to_uppercase()));
        let need = needs.get(&format!("live_{provider}"));
        let result = (|| {
            let need = need.ok_or("missing provider dependency")?;
            let raw = need
                .outputs
                .get("evidence")
                .map(String::as_str)
                .unwrap_or("");
            if !policy::required(provider)
                && matches!(enrollment, "" | "false")
                && evidence.absent
                && need.result == "skipped"
            {
                evidence.status = Status::Skipped;
            }
            if evidence.status.job_result() != need.result {
                return Err("job and dependency disagree");
            }
            if !raw.is_empty() && !seen.insert(Record::parse(raw.as_bytes())?.probe_id) {
                return Err("duplicate probe identity");
            }
            policy::evaluate(
                provider,
                raw.as_bytes(),
                &Identity {
                    candidate,
                    run_id,
                    run_attempt: &evidence.attempt.to_string(),
                    probe_id: None,
                },
                &need.result,
                enrollment,
            )
        })();
        match result {
            Ok(decision) => {
                evidence.status = decision.status;
                providers_ok &= decision.accepted;
            }
            Err(_) => {
                evidence.status = Status::Missing;
                providers_ok = false;
            }
        }
        providers.push((provider, evidence));
    }
    let qualified = jobs_ok
        && catalog_ok
        && official_ok
        && providers_ok
        && deterministic.status == Status::Pass
        && package.status == Status::Pass
        && input.get("DETERMINISTIC_RESULT") == "success"
        && input.get("PACKAGE_RESULT") == "success";
    let generated = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| "could not format qualification time")?;
    let mut lines = vec![
        "## Cargo AI Product Qualification".into(),
        "".into(),
        if qualified {
            "> ✅ **Product Qualification: Passed**".into()
        } else {
            BLOCKED.into()
        },
        "".into(),
        format!("- Candidate: [`{candidate}`](https://github.com/{repo}/commit/{candidate})"),
        format!(
            "- Workflow run: [#{} (attempt {attempt})]({url})",
            input.get("GITHUB_RUN_NUMBER")
        ),
        format!("- Generated: `{generated}`"),
        "".into(),
        "| Direction | Check | Policy | Status | Scope | Completed (UTC) | Evidence |".into(),
        "| --- | --- | --- | --- | --- | --- | --- |".into(),
    ];
    for (check, scope) in [
        (
            "CLI and workflow conformance",
            "Ubuntu, macOS, Windows; shared deterministic family",
        ),
        (
            "Deterministic provider adapters",
            "6 adapters on Ubuntu, macOS, Windows",
        ),
        (
            "Maintained content",
            "Checked-in examples and definitions on the deterministic family",
        ),
    ] {
        row(
            &mut lines,
            &[
                "Product",
                check,
                "required",
                deterministic.status.label(),
                scope,
                &deterministic.completed,
                &deterministic.links,
            ],
        );
    }
    row(
        &mut lines,
        &[
            "Package",
            "Public qualification canary lifecycle",
            "required",
            package.status.label(),
            &canary_scope,
            &package.completed,
            &package.links,
        ],
    );
    row(
        &mut lines,
        &[
            "Package",
            "Registered official packages",
            "conditional",
            official.status.label(),
            &official_scope,
            &official.completed,
            &official.links,
        ],
    );
    for (provider, evidence) in providers.iter().filter(|(p, _)| policy::required(p)) {
        row(
            &mut lines,
            &[
                "Live provider",
                policy::label(provider),
                "required",
                evidence.status.label(),
                "Protected representative hosted check",
                &evidence.completed,
                &evidence.links,
            ],
        );
    }
    lines.extend(
        [
            "",
            "<details>",
            "<summary>Supplemental provider details</summary>",
            "",
            "| Provider | Result | Evidence |",
            "| --- | --- | --- |",
        ]
        .map(str::to_string),
    );
    for (provider, evidence) in providers.iter().filter(|(p, _)| !policy::required(p)) {
        row(
            &mut lines,
            &[
                policy::label(provider),
                evidence.status.label(),
                &evidence.links,
            ],
        );
    }
    lines.extend(["", "</details>", "", "JUnit and provenance artifacts remain the durable evidence. This summary contains no provider credentials or raw provider responses.", ""].map(str::to_string));
    Ok((lines.join("\n"), qualified))
}
