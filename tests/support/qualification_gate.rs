//! Maintainer-only entry point for qualification orchestration.

mod qualification_catalog;
mod qualification_dashboard;
mod qualification_policy;

use qualification_policy::{Identity, Record, Result, Status};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

fn variable(name: &str) -> Result<String> {
    env::var(name).map_err(|_| "missing qualification input")
}

fn read(path: &Path, limit: u64) -> Result<String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "missing qualification input file")?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err("invalid qualification input file");
    }
    let mut value = String::new();
    fs::File::open(path)
        .map_err(|_| "cannot open qualification input")?
        .take(limit + 1)
        .read_to_string(&mut value)
        .map_err(|_| "qualification input must be UTF-8")?;
    if value.len() as u64 > limit {
        return Err("oversized qualification input");
    }
    Ok(value)
}

fn append(variable_name: &str, value: &str) -> Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(variable(variable_name)?)
        .and_then(|mut file| file.write_all(value.as_bytes()))
        .map_err(|_| "cannot write qualification output")
}

struct ProbeDirectory(PathBuf);
impl ProbeDirectory {
    fn new() -> Result<Self> {
        let parent = PathBuf::from(variable("RUNNER_TEMP")?);
        if !parent.is_absolute() {
            return Err("probe parent must be absolute");
        }
        let path = parent.join(format!(
            "provider-qualification-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|_| "cannot create private probe directory")?;
        Ok(Self(path))
    }
}
impl Drop for ProbeDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn probe(provider: &str) -> Result<()> {
    if !qualification_policy::PROVIDERS.contains(&provider) {
        return Err("unknown provider");
    }
    let candidate = variable("CARGO_AI_SHA")?;
    let run_id = variable("GITHUB_RUN_ID")?;
    let run_attempt = variable("GITHUB_RUN_ATTEMPT")?;
    if !qualification_policy::hexadecimal(&candidate, 40) {
        return Err("invalid candidate");
    }
    qualification_policy::positive(&run_id)?;
    qualification_policy::positive(&run_attempt)?;
    let checkout = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|_| "cannot verify checkout")?;
    if !checkout.status.success()
        || String::from_utf8(checkout.stdout)
            .map_err(|_| "invalid checkout identity")?
            .trim()
            != candidate
    {
        return Err("checkout does not match candidate");
    }
    let directory = ProbeDirectory::new()?;
    let report = directory.0.join("result.json");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let harness = Command::new("cargo")
        .args([
            "test",
            "--locked",
            "--test",
            "provider_smoke",
            &format!("live_{provider}_smoke_uses_isolated_stdin_credentials"),
            "--",
            "--ignored",
            "--exact",
        ])
        .env("CARGO_AI_QUALIFICATION_REPORT", &report)
        .env("CARGO_AI_QUALIFICATION_PROBE", &nonce)
        .status()
        .map_err(|_| "cannot execute probe harness")?;
    if !harness.success() {
        return Err("probe harness failed; no exemption applies");
    }
    let raw = read(&report, 2048)?;
    let decision = qualification_policy::evaluate(
        provider,
        raw.as_bytes(),
        &Identity {
            candidate: &candidate,
            run_id: &run_id,
            run_attempt: &run_attempt,
            probe_id: Some(&nonce),
        },
        "success",
        "true",
    )?;
    if !decision.accepted {
        return Err("probe did not satisfy qualification policy");
    }
    let record = Record::parse(raw.as_bytes())?;
    let encoded = serde_json::to_string(&record).map_err(|_| "cannot encode sanitized evidence")?;
    append("GITHUB_OUTPUT", &format!("evidence={encoded}\n"))?;
    if decision.status == Status::Unverified {
        println!("::warning title=Supplemental provider detail::{provider} live verification unavailable: rate limited");
    }
    Ok(())
}

fn aggregate() -> Result<()> {
    let rendered = (|| {
        let inputs = qualification_dashboard::Inputs {
            env: env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect::<BTreeMap<_, _>>(),
            jobs: read(Path::new(&variable("JOBS_JSON")?), 4 * 1024 * 1024).unwrap_or_default(),
            catalog: read(Path::new(&variable("CATALOG_PATH")?), 1024 * 1024).unwrap_or_default(),
        };
        qualification_dashboard::render(&inputs)
    })();
    let (summary, accepted) = rendered.unwrap_or_else(|_| {
        (
            format!(
                "## Cargo AI Product Qualification\n\n{}\n",
                qualification_dashboard::BLOCKED
            ),
            false,
        )
    });
    append("GITHUB_STEP_SUMMARY", &summary)?;
    if accepted {
        Ok(())
    } else {
        Err("product qualification is blocked")
    }
}

fn catalog() -> Result<()> {
    let raw = read(Path::new(&variable("CATALOG_PATH")?), 1024 * 1024)?;
    let catalog = qualification_catalog::Catalog::parse(&raw)?;
    let output = catalog.resolve(
        &env::var("REQUESTED_PACKAGE_REPOSITORY").unwrap_or_default(),
        &env::var("REQUESTED_PACKAGE_SHA").unwrap_or_default(),
        &env::var("REQUESTED_DECLARATION_PATH").unwrap_or_default(),
    )?;
    append("GITHUB_OUTPUT", &output)
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    let result = match args.as_slice() {
        [mode, provider] if mode == "probe" => probe(provider),
        [mode] if mode == "aggregate" => aggregate(),
        [mode] if mode == "catalog" => catalog(),
        _ => Err("usage: qualification-gate probe <provider> | aggregate | catalog"),
    };
    if let Err(message) = result {
        // Errors are fixed descriptions, never raw evidence, environment or service responses.
        eprintln!("Qualification failed: {message}.");
        std::process::exit(1);
    }
}
