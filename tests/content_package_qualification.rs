//! Bounded source-package qualification contract and process runner.

#[path = "support/qualification_paths.rs"]
mod qualification_paths;
mod support;
use qualification_paths::confined_file;

use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use support::{
    assert_success, openai_success_response, output_text, repository_root, Fixture,
    OneShotHttpServer,
};

const DECLARATION_FILE: &str = "cargo-ai-qualification.toml";
const MAX_CHECKS: usize = 5;
const MAX_ENTRYPOINTS: usize = 2;
const MAX_TIMEOUT_SECONDS: u64 = 600;
const CATALOG_FILE: &str = ".github/package-qualification-catalog.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationDeclaration {
    format_version: u32,
    package_id: String,
    profile: String,
    platforms: Vec<String>,
    entrypoints: Vec<QualificationEntrypoint>,
    #[serde(default)]
    checks: Vec<QualificationCheck>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationEntrypoint {
    name: String,
    run: bool,
    hatch: bool,
    #[serde(default)]
    fixture: Option<ImageCsvFixture>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageCsvFixture {
    kind: String,
    response: String,
    image: String,
    expected_csv: String,
    spdx: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationCheck {
    name: String,
    program: String,
    args: Vec<String>,
    working_directory: String,
    timeout_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationCatalog {
    schema_version: u32,
    official_package_count: usize,
    #[serde(default)]
    qualification_canaries: Vec<CatalogEntry>,
    #[serde(default)]
    official_packages: Vec<CatalogEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogEntry {
    package_id: String,
    repository: String,
    declaration_path: String,
    platforms: Vec<String>,
    pr_canary: bool,
    release_required: bool,
    enabled: bool,
    #[serde(default)]
    revision: Option<String>,
}

fn load_declaration(path: &Path) -> Result<QualificationDeclaration, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Failed to read '{}': {error}", path.display()))?;
    let declaration: QualificationDeclaration = toml::from_str(&contents)
        .map_err(|error| format!("Invalid qualification declaration: {error}"))?;
    validate_declaration(&declaration)?;
    Ok(declaration)
}

fn validate_declaration(declaration: &QualificationDeclaration) -> Result<(), String> {
    if declaration.format_version != 1 {
        return Err("format_version must be 1".to_string());
    }
    validate_identifier(&declaration.package_id, "package_id")?;
    if declaration.profile.trim().is_empty() {
        return Err("profile must not be empty".to_string());
    }
    if declaration.platforms.is_empty() || declaration.platforms.len() > 3 {
        return Err("platforms must contain between one and three entries".to_string());
    }
    let allowed_platforms = BTreeSet::from(["macos", "ubuntu", "windows"]);
    let unique_platforms = declaration
        .platforms
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if unique_platforms.len() != declaration.platforms.len()
        || !unique_platforms.is_subset(&allowed_platforms)
    {
        return Err(
            "platforms must be unique logical values: macos, ubuntu, or windows".to_string(),
        );
    }
    if declaration.entrypoints.is_empty() || declaration.entrypoints.len() > MAX_ENTRYPOINTS {
        return Err(format!(
            "entrypoints must contain between one and {MAX_ENTRYPOINTS} entries"
        ));
    }
    for entrypoint in &declaration.entrypoints {
        validate_identifier(&entrypoint.name, "entrypoint name")?;
        if let Some(fixture) = &entrypoint.fixture {
            if fixture.kind != "image_findings_csv" || !entrypoint.run {
                return Err(
                    "Image CSV fixtures require a runnable image_findings_csv contract.".into(),
                );
            }
            for path in [
                &fixture.response,
                &fixture.image,
                &fixture.expected_csv,
                &fixture.spdx,
            ] {
                if !qualification_paths::relative_file(path) {
                    return Err("Fixture paths must be portable relative files.".into());
                }
            }
        }
        if !entrypoint.run && !entrypoint.hatch {
            return Err(format!(
                "entrypoint '{}' must enable run, hatch, or both",
                entrypoint.name
            ));
        }
    }
    if declaration.checks.len() > MAX_CHECKS {
        return Err(format!("checks may contain at most {MAX_CHECKS} entries"));
    }
    for check in &declaration.checks {
        if check.name.trim().is_empty() {
            return Err("check name must not be empty".to_string());
        }
        validate_program(&check.program)?;
        if check.args.len() > 32 || check.args.iter().any(|arg| arg.len() > 4096) {
            return Err(format!("check '{}' has unbounded arguments", check.name));
        }
        validate_relative_directory(&check.working_directory)?;
        if check.timeout_seconds == 0 || check.timeout_seconds > MAX_TIMEOUT_SECONDS {
            return Err(format!(
                "check '{}' timeout_seconds must be between 1 and {MAX_TIMEOUT_SECONDS}",
                check.name
            ));
        }
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    let mut characters = value.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        || !characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(format!(
            "{label} must start with an ASCII letter or number and contain only letters, numbers, '-' or '_'"
        ));
    }
    Ok(())
}

fn validate_program(program: &str) -> Result<(), String> {
    validate_identifier(program, "check program")?;
    if matches!(
        program.to_ascii_lowercase().as_str(),
        "bash" | "cmd" | "powershell" | "pwsh" | "sh" | "zsh"
    ) {
        return Err("shell interpreters are not allowed as check programs".to_string());
    }
    Ok(())
}

fn validate_relative_directory(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("working_directory must not be empty".to_string());
    }
    if Path::new(path).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("working_directory must stay within the package root".to_string());
    }
    Ok(())
}

fn load_catalog(path: &Path) -> Result<QualificationCatalog, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Failed to read catalog '{}': {error}", path.display()))?;
    let catalog: QualificationCatalog = toml::from_str(&contents)
        .map_err(|error| format!("Invalid qualification catalog: {error}"))?;
    if catalog.schema_version != 1 {
        return Err("catalog schema_version must be 1".to_string());
    }
    if catalog.qualification_canaries.len() != 1 {
        return Err("catalog must define exactly one qualification canary".to_string());
    }
    if catalog.official_package_count != catalog.official_packages.len() {
        return Err("official_package_count must match registered official packages".to_string());
    }
    let pr_canaries = catalog
        .qualification_canaries
        .iter()
        .chain(&catalog.official_packages)
        .filter(|entry| entry.enabled && entry.pr_canary)
        .count();
    if pr_canaries > 4 {
        return Err("catalog may enable at most four PR canary targets".to_string());
    }
    for entry in catalog
        .qualification_canaries
        .iter()
        .chain(&catalog.official_packages)
    {
        validate_identifier(&entry.package_id, "catalog package_id")?;
        if entry.repository.split('/').count() != 2
            || entry.repository.split('/').any(|part| part.is_empty())
        {
            return Err("catalog repository must use owner/name form".to_string());
        }
        validate_relative_directory(&entry.declaration_path)?;
        if entry.platforms.is_empty() || entry.platforms.len() > 3 {
            return Err("catalog platforms must contain between one and three entries".to_string());
        }
        if entry.enabled
            && !entry.revision.as_deref().is_some_and(|revision| {
                revision.len() == 40
                    && revision.chars().all(|character| {
                        character.is_ascii_hexdigit() && !character.is_ascii_uppercase()
                    })
            })
        {
            return Err("enabled catalog entries require an exact lowercase commit".to_string());
        }
        if !entry.release_required && !entry.pr_canary {
            return Err(
                "catalog entries must participate in PR or release qualification".to_string(),
            );
        }
    }
    Ok(catalog)
}

#[test]
fn checked_in_qualification_contract_is_bounded_and_fail_closed() {
    let fixture_root = repository_root().join("tests/fixtures/package_qualification");
    let valid = load_declaration(&fixture_root.join("valid").join(DECLARATION_FILE))
        .expect("checked-in declaration should be valid");
    assert_eq!(valid.package_id, "qualification_fixture");
    assert_eq!(valid.platforms.len(), 3);
    assert_eq!(valid.entrypoints.len(), 1);

    for (name, expected) in [
        ("shell-command.toml", "shell interpreters"),
        ("path-escape.toml", "within the package root"),
        ("secret-request.toml", "unknown field"),
    ] {
        let error = load_declaration(&fixture_root.join("invalid").join(name))
            .expect_err("invalid declaration must fail closed");
        assert!(
            error.contains(expected),
            "unexpected {name} diagnostic: {error}"
        );
    }

    let missing = load_declaration(&fixture_root.join("missing.toml"))
        .expect_err("missing declaration must fail closed");
    assert!(missing.contains("Failed to read"));
}

#[test]
fn package_catalog_preserves_canary_and_official_classification() {
    let catalog = load_catalog(&repository_root().join(CATALOG_FILE))
        .expect("checked-in package catalog should be valid");
    assert_eq!(
        catalog.qualification_canaries[0].package_id,
        "qualification_canary"
    );
    assert_eq!(catalog.official_package_count, 1);
    assert_eq!(catalog.official_packages.len(), 1);
    assert_eq!(catalog.official_packages[0].package_id, "animal_patrol");
}

#[test]
#[ignore = "run explicitly in the source-package qualification workflow"]
fn external_package_qualification_runs_mandatory_lifecycle() {
    let checkout = std::env::var_os("CARGO_AI_QUALIFICATION_PACKAGE_ROOT")
        .map(PathBuf::from)
        .expect("CARGO_AI_QUALIFICATION_PACKAGE_ROOT must identify the checked-out package");
    let declaration_path = std::env::var("CARGO_AI_QUALIFICATION_DECLARATION_PATH")
        .unwrap_or_else(|_| DECLARATION_FILE.into());
    let declaration_path = confined_file(&checkout, &declaration_path, 64 * 1024)
        .expect("declaration must stay inside anonymous checkout");
    let package_root = declaration_path.parent().unwrap().to_path_buf();
    let declaration = load_declaration(&declaration_path)
        .expect("external qualification declaration should be valid");
    let fixture = Fixture::new("external");

    let host_target = rustc_host_target(&fixture, &package_root);
    let build = fixture
        .cargo_ai_command(&package_root)
        .args([
            "--no-update-check",
            "build",
            &declaration.profile,
            "--target",
            &host_target,
        ])
        .output()
        .expect("external build command should start");
    assert_success(&build, "external package build");

    let assembled = fixture.root.join("assembled-package");
    let package = fixture
        .cargo_ai_command(&package_root)
        .args([
            "--no-update-check",
            "package",
            &declaration.profile,
            "--output-dir",
        ])
        .arg(&assembled)
        .arg("--force")
        .output()
        .expect("external package command should start");
    assert_success(&package, "external package command");

    for entrypoint in &declaration.entrypoints {
        if let Some(data) = &entrypoint.fixture {
            verify_spdx_inventory(&assembled, &data.spdx);
        }
    }

    let install = fixture
        .cargo_ai_command(&fixture.root)
        .args(["--no-update-check", "packages", "install"])
        .arg(&assembled)
        .args(["--as", &declaration.package_id])
        .output()
        .expect("external install should start");
    assert_success(&install, "external package install");

    let inspect = fixture
        .cargo_ai_command(&fixture.root)
        .args([
            "--no-update-check",
            "packages",
            "inspect",
            &declaration.package_id,
        ])
        .output()
        .expect("external inspect should start");
    assert_success(&inspect, "external package inspect");
    let inspect_text = output_text(&inspect);
    assert!(inspect_text.contains(&format!("Identity:  {}", declaration.package_id)));

    for entrypoint in &declaration.entrypoints {
        let reference = format!("{}::{}", declaration.package_id, entrypoint.name);
        if entrypoint.hatch {
            let hatch = fixture
                .cargo_ai_command(&fixture.root)
                .args([
                    "--no-update-check",
                    "hatch",
                    &reference,
                    "--check",
                    "--ignore-tools",
                ])
                .output()
                .expect("external hatch check should start");
            assert_success(&hatch, "external installed hatch check");
        }
        if entrypoint.run {
            let content = if let Some(data) = &entrypoint.fixture {
                let path = confined_file(&package_root, &data.response, 16 * 1024).unwrap();
                let value: serde_json::Value =
                    serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                assert!(value.is_object() && value["findings"].is_array());
                value.to_string()
            } else {
                "ready".into()
            };
            let mut provider_response = openai_success_response(&content);
            if entrypoint.fixture.is_some() {
                provider_response["choices"][0]["message"]["content"] = content.into();
            }
            let server = OneShotHttpServer::json("/v1/chat/completions", provider_response);
            let run = fixture
                .cargo_ai_command(&fixture.root)
                .args([
                    "--no-update-check",
                    "run",
                    &reference,
                    "--server",
                    "openai",
                    "--model",
                    "qualification-model",
                    "--url",
                    &server.url,
                    "--token",
                    "fixture-token",
                    "--render-mode",
                    "append-only",
                ])
                .output()
                .expect("external installed run should start");
            let request = server.finish();
            assert_success(&run, "external installed run");
            if let Some(data) = &entrypoint.fixture {
                use base64::Engine as _;
                let image =
                    fs::read(confined_file(&package_root, &data.image, 4 * 1024 * 1024).unwrap())
                        .unwrap();
                assert!(
                    request.contains(&base64::engine::general_purpose::STANDARD.encode(&image)),
                    "fixture image must actually reach the loopback provider"
                );
                let expected =
                    fs::read(confined_file(&package_root, &data.expected_csv, 64 * 1024).unwrap())
                        .unwrap();
                let csv = fixture
                    .cargo_ai_home
                    .join("packages")
                    .join(&declaration.package_id)
                    .join("data/findings.csv");
                assert_eq!(
                    fs::read(csv).expect("installed tool must materialize its CSV"),
                    expected
                );
            }
        }
    }

    let uninstall = fixture
        .cargo_ai_command(&fixture.root)
        .args([
            "--no-update-check",
            "packages",
            "uninstall",
            &declaration.package_id,
            "--delete-data",
        ])
        .output()
        .expect("external uninstall should start");
    assert_success(&uninstall, "external package uninstall");

    run_declared_checks(&fixture, &package_root, &declaration);
}

fn verify_spdx_inventory(assembled: &Path, relative: &str) {
    use sha2::{Digest, Sha256};
    let document: serde_json::Value = serde_json::from_slice(
        &fs::read(confined_file(assembled, relative, 512 * 1024).unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(document["spdxVersion"], "SPDX-2.3");
    let files = document["files"]
        .as_array()
        .expect("SPDX file inventory is required");
    assert!(!files.is_empty() && files.len() <= 10_000);
    let mut declared = BTreeSet::new();
    for file in files {
        let path = file["fileName"]
            .as_str()
            .unwrap()
            .strip_prefix("./")
            .unwrap();
        assert!(declared.insert(path.to_string()), "duplicate SPDX file");
        let bytes = fs::read(confined_file(assembled, path, 100 * 1024 * 1024).unwrap()).unwrap();
        let hashes: Vec<_> = file["checksums"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["algorithm"] == "SHA256")
            .collect();
        assert_eq!(hashes.len(), 1);
        assert_eq!(
            hashes[0]["checksumValue"],
            format!("{:x}", Sha256::digest(bytes)),
            "SPDX file bytes: {path}"
        );
    }
    fn walk(root: &Path, directory: &Path, files: &mut BTreeSet<String>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(root, &entry.path(), files);
            } else {
                assert!(kind.is_file());
                files.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut actual = BTreeSet::new();
    walk(assembled, assembled, &mut actual);
    actual.remove(relative);
    actual.remove("cargo-ai-package.toml");
    assert_eq!(
        declared, actual,
        "SPDX must cover every distributed file except its own document and hashing manifest"
    );
}

fn rustc_host_target(fixture: &Fixture, package_root: &Path) -> String {
    let output = fixture
        .command("rustc", package_root)
        .arg("-vV")
        .output()
        .expect("rustc host query should start");
    assert_success(&output, "rustc host query");
    let text = output_text(&output);
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .filter(|target| !target.is_empty())
        .map(ToOwned::to_owned)
        .expect("rustc verbose version should report a host target")
}

fn run_declared_checks(
    fixture: &Fixture,
    package_root: &Path,
    declaration: &QualificationDeclaration,
) {
    for check in &declaration.checks {
        let working_directory = package_root.join(&check.working_directory);
        let mut command = if check.program == "cargo-ai" {
            fixture.cargo_ai_command(&working_directory)
        } else {
            fixture.command(&check.program, &working_directory)
        };
        command.args(&check.args);
        let output = run_bounded_command(
            command,
            Duration::from_secs(check.timeout_seconds),
            check.name.as_str(),
        );
        assert_success(
            &output,
            format!("package-owned check '{}'", check.name).as_str(),
        );
    }
}

fn run_bounded_command(mut command: Command, timeout: Duration, label: &str) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("check '{label}' should start: {error}"));
    let mut stdout = child.stdout.take().expect("check stdout should be piped");
    let mut stderr = child.stderr.take().expect("check stderr should be piped");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .read_to_end(&mut bytes)
            .expect("check stdout should remain readable");
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .read_to_end(&mut bytes)
            .expect("check stderr should remain readable");
        bytes
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait().expect("check status should be readable") {
            Some(status) => break status,
            None if started.elapsed() < timeout => thread::sleep(Duration::from_millis(50)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = stdout_reader.join().expect("stdout reader should finish");
                let stderr = stderr_reader.join().expect("stderr reader should finish");
                panic!(
                    "check '{label}' exceeded {} seconds\n{}\n{}",
                    timeout.as_secs(),
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
            }
        }
    };
    Output {
        status,
        stdout: stdout_reader.join().expect("stdout reader should finish"),
        stderr: stderr_reader.join().expect("stderr reader should finish"),
    }
}
