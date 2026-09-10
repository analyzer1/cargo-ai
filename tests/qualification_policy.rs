//! Offline policy and actual maintainer-entrypoint qualification contracts.

#[path = "support/qualification_policy.rs"]
#[allow(dead_code)]
mod qualification_policy;

use qualification_policy::{evaluate, Identity, Outcome, Record, PROVIDERS};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::OnceLock,
};

fn record(provider: &str, outcome: &str) -> Value {
    json!({"schema_version":1,"candidate":"a".repeat(40),"provider":provider,
        "run_id":"123","run_attempt":"2","probe_id":format!("{:032x}", PROVIDERS.iter().position(|p| *p == provider).unwrap() + 1),"outcome":outcome})
}

#[test]
fn typed_records_and_primary_supplemental_contract() {
    let candidate = "a".repeat(40);
    let id = Identity {
        candidate: &candidate,
        run_id: "123",
        run_attempt: "2",
        probe_id: None,
    };
    for provider in PROVIDERS {
        for outcome in ["pass", "rate_limited", "failure"] {
            let raw = record(provider, outcome).to_string();
            let decision = evaluate(provider, raw.as_bytes(), &id, "success", "true").unwrap();
            assert_eq!(
                decision.accepted,
                outcome == "pass"
                    || (outcome == "rate_limited" && !qualification_policy::required(provider))
            );
            for job in ["failure", "cancelled", "skipped", "missing"] {
                assert!(evaluate(provider, raw.as_bytes(), &id, job, "true").is_err());
            }
        }
    }
    for (key, value) in [
        ("schema_version", json!(true)),
        ("schema_version", json!(2)),
        ("candidate", json!("a".repeat(39))),
        ("provider", json!("unknown")),
        ("run_id", json!(123)),
        ("run_attempt", json!("02")),
        ("probe_id", json!("b".repeat(31))),
        ("outcome", json!("unavailable")),
        ("unexpected", json!("secret-marker")),
    ] {
        let mut data = record("mistral", "pass");
        data[key] = value;
        assert!(Record::parse(data.to_string().as_bytes()).is_err(), "{key}");
    }
    let raw = record("mistral", "pass").to_string();
    let duplicate = raw.replacen('{', "{\"provider\":\"mistral\",", 1);
    for value in [
        duplicate,
        "{}".into(),
        "[]".into(),
        " ".repeat(2049),
        "{broken".into(),
    ] {
        assert!(Record::parse(value.as_bytes()).is_err());
    }
    assert!(Record::parse(&[0xff]).is_err());
    for (key, value) in [
        ("candidate", "c".repeat(40)),
        ("run_id", "124".into()),
        ("run_attempt", "1".into()),
        ("provider", "gemini".into()),
    ] {
        let mut data = record("mistral", "pass");
        data[key] = json!(value);
        assert!(evaluate(
            "mistral",
            data.to_string().as_bytes(),
            &id,
            "success",
            "true"
        )
        .is_err());
    }
    let nonce = Identity {
        probe_id: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ..id
    };
    assert!(evaluate("mistral", raw.as_bytes(), &nonce, "success", "true").is_err());
    for flag in ["", "false"] {
        assert!(
            evaluate("mistral", b"", &id, "skipped", flag)
                .unwrap()
                .accepted
        );
        assert!(evaluate("mistral", raw.as_bytes(), &id, "skipped", flag).is_err());
        assert!(evaluate("mistral", b"", &id, "success", flag).is_err());
    }
    assert!(evaluate("mistral", raw.as_bytes(), &id, "success", "TRUE").is_err());
    assert_eq!(
        Record::parse(raw.as_bytes()).unwrap().outcome,
        Outcome::Pass
    );
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cq-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn command(&self, mode: &str) -> Command {
        let mut command = Command::new(gate());
        command
            .arg(mode)
            .current_dir(&self.0)
            .env("CARGO_AI_HOME", self.path("isolated-home"))
            .env("CARGO_AI_DISABLE_KEYCHAIN", "1")
            .env("RUNNER_TEMP", &self.0)
            .env("GITHUB_OUTPUT", self.path("output"))
            .env("GITHUB_STEP_SUMMARY", self.path("summary"));
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn gate() -> &'static Path {
    static GATE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    GATE.get_or_init(|| {
        // Build the real example once, in this test's existing Cargo target tree.
        let exe = std::env::current_exe().unwrap();
        let profile = exe.parent().unwrap().parent().unwrap();
        let mut command = Command::new(env!("CARGO"));
        command.current_dir(env!("CARGO_MANIFEST_DIR")).args([
            "build",
            "--locked",
            "--example",
            "qualification-gate",
        ]);
        let rustc = Command::new("rustc").arg("-vV").output().unwrap();
        assert!(rustc.status.success());
        let version = String::from_utf8(rustc.stdout).unwrap();
        let host = version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .unwrap();
        // The local wrapper passes --target as an argument, not an environment variable.
        // Match that target tree; native CI without an explicit target keeps its layout.
        if profile
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == host)
        {
            command.args(["--target", host]);
        } else if let Ok(target) = std::env::var("CARGO_BUILD_TARGET") {
            command.args(["--target", &target]);
        }
        if !command.status().unwrap().success() {
            return Err("maintainer example failed to build; no retry in this test run".into());
        }
        let path = profile.join("examples").join(format!(
            "qualification-gate{}",
            std::env::consts::EXE_SUFFIX
        ));
        if !path.is_file() {
            return Err(format!("maintainer example missing at {}", path.display()));
        }
        Ok(path)
    })
    .as_ref()
    .expect("maintainer entrypoint must build")
}

fn catalog() -> String {
    include_str!("../.github/package-qualification-catalog.toml").to_string()
}

struct Dashboard {
    env: BTreeMap<String, String>,
    jobs: Value,
    needs: Value,
    catalog: String,
}
impl Dashboard {
    fn new() -> Self {
        let mut env = BTreeMap::new();
        for (key, value) in [
            ("GITHUB_REPOSITORY", "cargo-ai/cargo-ai"),
            ("GITHUB_RUN_ID", "123"),
            ("GITHUB_RUN_NUMBER", "7"),
            ("GITHUB_RUN_ATTEMPT", "2"),
            ("JOBS_API_STATUS", "0"),
            ("CATALOG_CHECKOUT_OUTCOME", "success"),
            ("DETERMINISTIC_RESULT", "success"),
            ("PACKAGE_RESULT", "success"),
            ("LIVE_GEMINI_ENABLED", "true"),
            ("LIVE_XAI_ENABLED", "true"),
            ("LIVE_MISTRAL_ENABLED", "true"),
        ] {
            env.insert(key.into(), value.into());
        }
        env.insert("CARGO_AI_SHA".into(), "a".repeat(40));
        env.insert("TRUSTED_TRIGGER_SHA".into(), "c".repeat(40));
        let mut jobs = Vec::new();
        let mut needs = json!({});
        for family in [
            "Deterministic qualification",
            "Source package qualification",
        ] {
            for os in ["ubuntu-latest", "macos-latest", "windows-latest"] {
                jobs.push(Self::job(&format!("{family} ({os})")));
            }
        }
        for provider in PROVIDERS {
            jobs.push(Self::job(&format!(
                "Live {} conformance",
                qualification_policy::label(provider)
            )));
            needs[format!("live_{provider}")] = json!({"result":"success","outputs":{"evidence":record(provider, "pass").to_string()}});
        }
        Self {
            env,
            jobs: json!({"jobs":jobs}),
            needs,
            catalog: catalog(),
        }
    }
    fn job(name: &str) -> Value {
        json!({"name":name,"head_sha":"c".repeat(40),"run_attempt":2,
        "status":"completed","conclusion":"success","completed_at":"2026-09-10T12:00:00Z",
        "html_url":"https://github.com/cargo-ai/cargo-ai/actions/runs/123/job/456"})
    }
    fn outcome(&mut self, provider: &str, outcome: &str) {
        self.needs[format!("live_{provider}")]["outputs"]["evidence"] =
            json!(record(provider, outcome).to_string());
    }
    fn run(&self) -> (Output, String) {
        let fixture = Fixture::new();
        fs::write(fixture.path("jobs.json"), self.jobs.to_string()).unwrap();
        fs::write(fixture.path("catalog.toml"), &self.catalog).unwrap();
        let output = fixture
            .command("aggregate")
            .envs(&self.env)
            .env("JOBS_JSON", fixture.path("jobs.json"))
            .env("CATALOG_PATH", fixture.path("catalog.toml"))
            .env("PROVIDER_PROBE_RECORDS", self.needs.to_string())
            .output()
            .unwrap();
        (output, fs::read_to_string(fixture.path("summary")).unwrap())
    }
}

#[test]
fn actual_dashboard_requires_complete_correlated_evidence_and_collapses_supplemental_details() {
    let mut happy = Dashboard::new();
    happy.outcome("mistral", "rate_limited");
    let (output, summary) = happy.run();
    assert!(output.status.success());
    let headline = summary.find("Product Qualification: Passed").unwrap();
    let details = summary.find("<details>").unwrap();
    assert!(headline < details && summary.find("not verified").unwrap() > details);
    assert!(summary.contains("✅ pass") && summary.contains("⚠️ not verified — rate limited"));
    for provider in ["openai", "anthropic"] {
        let mut data = Dashboard::new();
        data.outcome(provider, "rate_limited");
        assert!(!data.run().0.status.success());
    }
    for provider in PROVIDERS {
        let mut data = Dashboard::new();
        data.outcome(provider, "failure");
        assert!(!data.run().0.status.success());
    }
    for case in 0..22 {
        let mut data = Dashboard::new();
        data.outcome("mistral", "rate_limited");
        match case {
            0 => data.needs["live_mistral"]["outputs"] = json!({}),
            1 => data.needs["live_mistral"]["result"] = json!("failure"),
            2 => data.jobs["jobs"][10]["conclusion"] = json!("failure"),
            3 => {
                data.jobs["jobs"].as_array_mut().unwrap().pop();
            }
            4 => {
                let duplicate = data.jobs["jobs"][10].clone();
                data.jobs["jobs"].as_array_mut().unwrap().push(duplicate);
            }
            5 => data.jobs["jobs"][10]["run_attempt"] = json!(1),
            6 => data.jobs["jobs"][10]["head_sha"] = json!("d".repeat(40)),
            7 => data.jobs["jobs"][10]["status"] = json!("in_progress"),
            8 => data.jobs["jobs"][10]["conclusion"] = json!("unknown"),
            9 => data.jobs["jobs"][10]["run_attempt"] = json!(3),
            10 => {
                data.env.insert("JOBS_API_STATUS".into(), "1".into());
            }
            11 => {
                data.env
                    .insert("DETERMINISTIC_RESULT".into(), "failure".into());
            }
            12 => {
                data.env.insert("PACKAGE_RESULT".into(), "failure".into());
            }
            13 => data.jobs["jobs"][3]["conclusion"] = json!("failure"),
            14 => data.catalog = "schema_version=2".into(),
            15 => {
                data.catalog = data
                    .catalog
                    .replace("official_package_count = 0", "official_package_count = 1")
            }
            16 => data.catalog = data.catalog.replace("enabled = true", "enabled = false"),
            17 => data.catalog = data.catalog.replace("ubuntu-latest", "unknown-os"),
            18 => {
                data.env
                    .insert("CATALOG_CHECKOUT_OUTCOME".into(), "failure".into());
            }
            19 => {
                data.env
                    .insert("LIVE_MISTRAL_ENABLED".into(), "FALSE".into());
            }
            20 => {
                let mut value = record("mistral", "pass");
                value["probe_id"] = record("openai", "pass")["probe_id"].clone();
                data.needs["live_mistral"]["outputs"]["evidence"] = json!(value.to_string());
            }
            21 => {
                data.env.insert(
                    "GITHUB_REPOSITORY".into(),
                    "bad](https://evil.example)".into(),
                );
            }
            _ => unreachable!(),
        }
        let (output, summary) = data.run();
        assert!(!output.status.success(), "case {case}");
        assert!(
            summary.contains("BLOCKED") && !summary.contains("Product Qualification: Passed"),
            "case {case}"
        );
    }
    let mut skipped = Dashboard::new();
    skipped
        .env
        .insert("LIVE_MISTRAL_ENABLED".into(), "false".into());
    skipped.jobs["jobs"].as_array_mut().unwrap().pop();
    skipped.needs["live_mistral"] = json!({"result":"skipped","outputs":{}});
    let (output, summary) = skipped.run();
    assert!(output.status.success());
    assert!(summary.contains("not configured"));
    // Unenrollment may explain an absent job, never duplicate or contradictory jobs.
    for case in 0..3 {
        let mut bad = Dashboard::new();
        bad.env
            .insert("LIVE_MISTRAL_ENABLED".into(), "false".into());
        bad.needs["live_mistral"] = json!({"result":"skipped","outputs":{}});
        match case {
            0 => {
                let duplicate = bad.jobs["jobs"][10].clone();
                bad.jobs["jobs"].as_array_mut().unwrap().push(duplicate);
            }
            1 => bad.jobs["jobs"][10]["head_sha"] = json!("d".repeat(40)),
            2 => bad.jobs["jobs"][10]["run_attempt"] = json!(0),
            _ => unreachable!(),
        }
        assert!(!bad.run().0.status.success());
    }
    let mut reused = Dashboard::new();
    reused.jobs["jobs"][10]["run_attempt"] = json!(1);
    let mut old = record("mistral", "pass");
    old["run_attempt"] = json!("1");
    reused.needs["live_mistral"]["outputs"]["evidence"] = json!(old.to_string());
    assert!(reused.run().0.status.success());
    let mut injected = Dashboard::new();
    injected.jobs["jobs"][0]["completed_at"] = json!("<script>alert('x')</script>|\nline");
    injected.jobs["jobs"][0]["html_url"] =
        json!("https://github.com/cargo-ai/cargo-ai/actions/runs/123/job/1)evil");
    let (_, summary) = injected.run();
    assert!(!summary.contains("<script>") && !summary.contains(")evil"));
}

#[test]
fn actual_catalog_entrypoint_confines_identity_and_outputs() {
    let fixture = Fixture::new();
    let path = fixture.path("catalog-é.toml");
    fs::write(&path, catalog()).unwrap();
    let run = |repo: &str, sha: &str, declaration: &str| {
        let _ = fs::remove_file(fixture.path("output"));
        fixture
            .command("catalog")
            .env("CATALOG_PATH", &path)
            .env("REQUESTED_PACKAGE_REPOSITORY", repo)
            .env("REQUESTED_PACKAGE_SHA", sha)
            .env("REQUESTED_DECLARATION_PATH", declaration)
            .output()
            .unwrap()
    };
    assert!(run("", "", "").status.success());
    assert!(fs::read_to_string(fixture.path("output"))
        .unwrap()
        .contains("repository=cargo-ai/cargo-ai-qualification-canary\n"));
    assert!(run(
        "cargo-ai/cargo-ai-qualification-canary",
        &"b".repeat(40),
        "déclarations/check.toml"
    )
    .status
    .success());
    assert!(fs::read_to_string(fixture.path("output"))
        .unwrap()
        .contains("declaration=déclarations/check.toml\n"));
    for path in [
        "../escape",
        "a/../escape",
        "/absolute",
        "C:\\escape",
        "a\\..\\escape",
        "x\nsha=injected",
        "./bad",
        "a//b",
    ] {
        assert!(!run("", "", path).status.success(), "{path}");
        assert!(!fixture.path("output").exists());
    }
    assert!(!run("unknown/package", &"a".repeat(40), "test.toml")
        .status
        .success());
    assert!(
        !run("cargo-ai/cargo-ai-qualification-canary", "develop", "")
            .status
            .success()
    );
    fs::write(
        &path,
        format!(
            "{}\n{}",
            catalog(),
            catalog()
                .split("[[qualification_canaries]]")
                .nth(1)
                .map(|s| format!("[[qualification_canaries]]{s}"))
                .unwrap()
        ),
    )
    .unwrap();
    assert!(!run("", "", "").status.success());
    assert!(!run(
        "cargo-ai/cargo-ai-qualification-canary",
        &"a".repeat(40),
        ""
    )
    .status
    .success());
    fs::write(&path, [0xff]).unwrap();
    assert!(!run("", "", "").status.success());
    fs::write(&path, catalog()).unwrap();
    assert!(!fixture
        .command("catalog")
        .env("CATALOG_PATH", &path)
        .env("REQUESTED_PACKAGE_REPOSITORY", "")
        .env("REQUESTED_PACKAGE_SHA", "")
        .env("REQUESTED_DECLARATION_PATH", "")
        .env("GITHUB_OUTPUT", fixture.path("missing-parent/output"))
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn actual_probe_entrypoint_rejects_failed_harness_and_stale_or_unsafe_reports() {
    let fixture = Fixture::new();
    // A tiny Rust process fixture stands in for Cargo, not for the gate being tested.
    let source = fixture.path("fixture.rs");
    fs::write(&source, r###"
use std::{env,fs};
fn main() {
 let args:Vec<_>=env::args().skip(1).collect();
 assert_eq!(&args[..4], ["test","--locked","--test","provider_smoke"]);
 assert_eq!(&args[5..], ["--","--ignored","--exact"]);
 let provider=args[4].strip_prefix("live_").unwrap().strip_suffix("_smoke_uses_isolated_stdin_credentials").unwrap();
 let mode=env::var("FIXTURE_MODE").unwrap();
 if mode=="missing" {return;}
 let path=env::var("CARGO_AI_QUALIFICATION_REPORT").unwrap();
 let nonce=if mode=="stale" {"b".repeat(32)} else {env::var("CARGO_AI_QUALIFICATION_PROBE").unwrap()};
 let outcome=if mode=="rate_limited" {"rate_limited"} else if mode=="failure" {"failure"} else {"pass"};
 let raw=format!(r##"{{"schema_version":1,"candidate":"{}","provider":"{}","run_id":"123","run_attempt":"2","probe_id":"{}","outcome":"{}"}}"##,env::var("CARGO_AI_SHA").unwrap(),provider,nonce,outcome);
 fs::write(path,if mode=="malformed" {"secret-marker".into()} else {raw}).unwrap();
 if mode=="harness_failure" {std::process::exit(3);}
}
"###).unwrap();
    let bin_dir = fixture.path("bin");
    fs::create_dir(&bin_dir).unwrap();
    let stub = bin_dir.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
    assert!(Command::new("rustc")
        .arg(&source)
        .args(["--edition=2021", "-o"])
        .arg(&stub)
        .status()
        .unwrap()
        .success());
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(&fixture.0)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(git(&["init"]).status.success());
    assert!(git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-m",
        "fixture"
    ])
    .status
    .success());
    let candidate = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    let paths = std::iter::once(bin_dir)
        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))
        .collect::<Vec<_>>();
    for provider in ["openai", "anthropic", "mistral"] {
        for mode in [
            "pass",
            "rate_limited",
            "failure",
            "missing",
            "stale",
            "malformed",
            "harness_failure",
        ] {
            let _ = fs::remove_file(fixture.path("output"));
            let result = fixture
                .command("probe")
                .arg(provider)
                .env("PATH", std::env::join_paths(&paths).unwrap())
                .env("CARGO_AI_SHA", &candidate)
                .env("GITHUB_RUN_ID", "123")
                .env("GITHUB_RUN_ATTEMPT", "2")
                .env("FIXTURE_MODE", mode)
                .output()
                .unwrap();
            let accepted = mode == "pass" || (mode == "rate_limited" && provider == "mistral");
            assert_eq!(
                result.status.success(),
                accepted,
                "{provider}/{mode}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            if accepted {
                let raw = fs::read_to_string(fixture.path("output")).unwrap();
                assert_eq!(raw.lines().count(), 1);
                Record::parse(raw.trim().strip_prefix("evidence=").unwrap().as_bytes()).unwrap();
            } else {
                assert!(!fixture.path("output").exists());
            }
            assert!(!String::from_utf8_lossy(&result.stderr).contains("secret-marker"));
            assert!(!fs::read_dir(&fixture.0).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("provider-qualification-")));
        }
    }
    let result = fixture
        .command("probe")
        .arg("mistral")
        .env("CARGO_AI_SHA", "a".repeat(40))
        .env("GITHUB_RUN_ID", "123")
        .env("GITHUB_RUN_ATTEMPT", "2")
        .output()
        .unwrap();
    assert!(!result.status.success());
}
