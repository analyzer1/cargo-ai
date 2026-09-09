//! Real-CLI ownership round trip with deterministic image-provider responses.
#[allow(dead_code)]
mod support;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use support::{assert_success, copy_tree, output_text, Fixture, OneShotHttpServer};

const DEFINITION: &str = include_str!("fixtures/ownership_conformance/animal_patrol.json");
const FINDINGS: &str = include_str!("fixtures/ownership_conformance/findings.json");
const TOOL: &str = include_str!("fixtures/ownership_conformance/csv_writer.rs");
const CSV: &str = include_str!("fixtures/ownership_conformance/expected.csv");
const PHOTO: &str = include_str!("fixtures/ownership_conformance/patrol.png.base64");
const USER_WORK: &str = "User-owned Animal Patrol notes; preserve these bytes.\n";
const PRIVATE_DATA: &str = "private-runtime-sentinel-not-a-package-asset";
const PRIVATE_GUIDANCE: &str = "incidental-guidance-sentinel-not-a-package-asset";

fn command(fixture: &Fixture, program: impl AsRef<std::ffi::OsStr>, root: &Path) -> Command {
    let mut paths = vec![Path::new(env!("CARGO_BIN_EXE_cargo-ai"))
        .parent()
        .unwrap()
        .to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut command = fixture.command(program, root);
    command
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("CARGO_NET_OFFLINE", "true");
    command
}

fn cli(fixture: &Fixture, root: &Path, args: &[&str]) -> Output {
    command(fixture, env!("CARGO_BIN_EXE_cargo-ai"), root)
        .arg("--no-update-check")
        .args(args)
        .output()
        .expect("isolated CLI should start")
}

fn success(fixture: &Fixture, root: &Path, args: &[&str]) -> Output {
    let output = cli(fixture, root, args);
    assert_success(&output, &args.join(" "));
    output
}

fn executable(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn json_file(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn toml_file(path: impl AsRef<Path>) -> toml::Value {
    toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn add_profiles(fixture: &Fixture, root: &Path) {
    for provider in ["openai", "gemini", "anthropic"] {
        success(
            fixture,
            root,
            &[
                "profile",
                "add",
                provider,
                "--server",
                provider,
                "--model",
                &format!("animal-{provider}"),
                "--auth",
                "api_key",
            ],
        );
        let mut child = command(fixture, env!("CARGO_BIN_EXE_cargo-ai"), root)
            .args(["--no-update-check", "profile", "set", provider, "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("ownership-{provider}-fake-token").as_bytes())
            .unwrap();
        assert_success(
            &child.wait_with_output().unwrap(),
            "set isolated fake profile token",
        );
    }
}

fn run_profile(
    fixture: &Fixture,
    root: &Path,
    source: &str,
    generated: Option<&Path>,
    provider: &str,
) {
    let findings: Value = serde_json::from_str(FINDINGS).unwrap();
    let (endpoint, response) = if provider == "openai" {
        let mut response = support::openai_success_response("unused");
        response["model"] = json!("animal-openai");
        response["choices"][0]["message"]["content"] = Value::String(findings.to_string());
        ("/v1/chat/completions", response)
    } else {
        (
            "/v1beta/interactions",
            json!({"status":"completed","steps":[{"type":"model_output","content":[{"type":"text","text":findings.to_string()}]}],"usage":{"total_input_tokens":12,"total_output_tokens":5,"total_tokens":17}}),
        )
    };
    eprintln!(
        "Ownership: running {provider} (generated={})",
        generated.is_some()
    );
    let server = OneShotHttpServer::json(endpoint, response);
    let mut command = match generated {
        Some(program) => command(fixture, program, root),
        None => {
            let mut command = command(fixture, env!("CARGO_BIN_EXE_cargo-ai"), root);
            command.args(["--no-update-check", "run", source]);
            command
        }
    };
    let output = command
        .args([
            "--profile",
            provider,
            "--url",
            &server.url,
            "--render-mode",
            "append-only",
            "--inference-timeout-in-sec",
            "5",
        ])
        .output()
        .unwrap();
    assert_success(
        &output,
        &format!(
            "{provider} Animal Patrol run (generated={})",
            generated.is_some()
        ),
    );
    let request = server.finish();
    assert!(output_text(&output).contains("Recorded 2 findings"));
    let token = format!("ownership-{provider}-fake-token");
    assert!(request
        .to_ascii_lowercase()
        .contains(&if provider == "openai" {
            format!("authorization: bearer {token}")
        } else {
            format!("x-goog-api-key: {token}")
        }));
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["model"], format!("animal-{provider}"));
    if provider == "openai" {
        let image_parts = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter(|part| part["type"] == "image_url")
            .collect::<Vec<_>>();
        assert_eq!(image_parts.len(), 1);
        assert_eq!(
            image_parts[0]["image_url"]["url"],
            format!("data:image/png;base64,{}", PHOTO.trim())
        );
        assert_eq!(body["response_format"]["type"], "json_schema");
    } else {
        let image_parts = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part["type"] == "image")
            .collect::<Vec<_>>();
        assert_eq!(image_parts.len(), 1);
        assert_eq!(image_parts[0]["data"], PHOTO.trim());
        assert_eq!(image_parts[0]["mime_type"], "image/png");
        assert_eq!(
            body["response_format"]["schema"]["properties"]["findings"]["items"]["properties"]
                ["confidence"]["maximum"]
                .as_f64(),
            Some(1.0)
        );
        assert_eq!(body["response_format"]["mime_type"], "application/json");
        assert_eq!(body["store"], false);
    }
    assert!(!output_text(&output).contains(&token));
}

fn incompatible_profile_is_rejected(fixture: &Fixture, project: &Path) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let output = cli(
        fixture,
        project,
        &[
            "run",
            "patrol.json",
            "--profile",
            "anthropic",
            "--url",
            &url,
            "--render-mode",
            "append-only",
        ],
    );
    assert!(!output.status.success());
    let text = output_text(&output);
    assert!(
        text.contains("Anthropic structured output does not support JSON Schema keyword `maximum`"),
        "{text}"
    );
    assert!(text.contains("will not remove or weaken the authored schema"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        fs::read(project.join("patrol.json")).unwrap(),
        DEFINITION.as_bytes()
    );
    assert!(!project.join("findings.csv").exists());
    assert_empty_data(&project.join(".cargo-ai/data"));
    eprintln!("Ownership: incompatible Anthropic bounds rejected before network or CSV writes; definition unchanged.");
}

fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn assert_not_link(metadata: &fs::Metadata, path: &Path) {
        assert!(
            !metadata.file_type().is_symlink(),
            "fixture inventory must not follow links: {}",
            path.display()
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            assert_eq!(
                metadata.file_attributes() & 0x0000_0400,
                0,
                "fixture inventory must not follow reparse points: {}",
                path.display()
            );
        }
    }
    fn visit(root: &Path, directory: &Path, result: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert_not_link(&metadata, &path);
            if metadata.is_dir() {
                visit(root, &path, result);
            } else {
                assert!(metadata.is_file());
                result.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace('\\', "/"),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let metadata = fs::symlink_metadata(root).unwrap();
    assert_not_link(&metadata, root);
    assert!(metadata.is_dir());
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

fn exact_inventory(actual: &BTreeMap<String, Vec<u8>>, expected: &[String]) {
    assert_eq!(
        actual.keys().cloned().collect::<BTreeSet<_>>(),
        expected.iter().cloned().collect::<BTreeSet<_>>()
    );
}

fn assert_empty_data(root: &Path) {
    assert!(
        !root.exists() || fs::read_dir(root).unwrap().next().is_none(),
        "new data root must be empty: {}",
        root.display()
    );
}

fn assert_csv(root: &Path, expected: &str, mode: &str) {
    assert_eq!(
        fs::read_to_string(root.join("findings.csv"))
            .unwrap_or_else(|error| panic!("missing CSV under {}: {error}", root.display())),
        expected.replace("\r\n", "\n")
    );
    assert_eq!(
        fs::read_to_string(root.join("compile-mode.txt")).unwrap(),
        mode
    );
}

fn verify_provenance(fixture: &Fixture, root: &Path, program: &Path, target: &str) {
    let output = command(fixture, program, root)
        .args(["inspect", "--json"])
        .output()
        .unwrap();
    assert_success(&output, "generated provenance inspection");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let definition: Value = serde_json::from_str(DEFINITION).unwrap();
    assert_eq!(value["embedded_definition_json"], definition);
    assert_eq!(
        value["definition_sha256"],
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&definition).unwrap())
        )
    );
    assert_eq!(value["target_triple"], target);
    assert_eq!(
        value["generated_by_cargo_ai_version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        value["generated_with_template_schema_version"],
        "2026-09-09.r1"
    );
    assert!(!value["agent_build_id"].as_str().unwrap().is_empty());
    assert!(!value["build_timestamp_utc"].as_str().unwrap().is_empty());
}

#[test]
fn animal_patrol_keeps_editable_source_and_independent_owned_results() {
    let author = Fixture::new("animal-author");
    let fresh = author.root.join("fresh");
    success(
        &author,
        &author.root,
        &["new", fresh.to_str().unwrap(), "--vcs", "none"],
    );
    assert_empty_data(&fresh.join(".cargo-ai/data"));
    assert_eq!(
        toml_file(fresh.join(".cargo-ai/project.toml"))["runtime"]["data_root"].as_str(),
        Some(".cargo-ai/data")
    );
    let project = author.root.join("source");
    write(project.join("AGENTS.md"), USER_WORK);
    write(project.join("notes/field-notes.md"), USER_WORK);
    success(
        &author,
        &author.root,
        &["init", project.to_str().unwrap(), "--vcs", "none"],
    );
    success(&author, &project, &["add", "guidance", "--style", "codex"]);
    assert_eq!(
        fs::read_to_string(project.join("AGENTS.md")).unwrap(),
        USER_WORK
    );
    assert!(project.join(".cargo-ai/guidance/manifest.json").is_file());
    assert_empty_data(&project.join(".cargo-ai/data"));
    write(project.join("patrol.json"), DEFINITION);
    write(
        project.join("assets/patrol.png"),
        BASE64.decode(PHOTO.trim()).unwrap(),
    );
    let metadata_path = project.join(".cargo-ai/project.toml");
    let mut metadata = toml_file(&metadata_path);
    assert!(
        metadata
            .get("runtime")
            .and_then(|runtime| runtime.get("data_root"))
            .is_none(),
        "init must preserve the existing working-directory contract"
    );
    metadata["project"]["name"] = "animal_patrol".into();
    metadata["project"]["version"] = "0.1.0".into();
    metadata["tools"]["allow_global_fallback"] = false.into();
    metadata.as_table_mut().unwrap().insert("build".into(), toml::from_str::<toml::Value>("[default]\nagent_definitions=['patrol.json']\nhatched_agents=['patrol.json']\ntools=['csv_writer']\nassets=['assets/patrol.png']\n").unwrap());
    write(&metadata_path, toml::to_string_pretty(&metadata).unwrap());
    success(&author, &project, &["add", "tool", "csv_writer"]);
    write(project.join("tools/csv_writer/src/main.rs"), TOOL);
    success(&author, &project, &["tools", "build", "csv_writer"]);
    let author_cargo = fs::read(project.join("tools/csv_writer/Cargo.toml")).unwrap();
    let author_lock = fs::read(project.join("tools/csv_writer/Cargo.lock")).unwrap();
    let tool_manifest = json_file(project.join(".cargo-ai/tools/csv_writer/tool.json"));
    let target = tool_manifest["artifacts"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    add_profiles(&author, &project);
    let check = success(
        &author,
        &project,
        &["hatch", "patrol", "--config", "patrol.json", "--check"],
    );
    assert!(output_text(&check).contains("Cargo profile  dev"));
    assert!(!project.join(executable("patrol")).exists());
    incompatible_profile_is_rejected(&author, &project);
    // Adapting existing work retains its old write location until explicit adoption.
    run_profile(&author, &project, "patrol.json", None, "openai");
    assert_csv(&project, CSV, "dev");
    assert_empty_data(&project.join(".cargo-ai/data"));
    fs::remove_file(project.join("findings.csv")).unwrap();
    fs::remove_file(project.join("compile-mode.txt")).unwrap();
    metadata.as_table_mut().unwrap().insert(
        "runtime".into(),
        toml::from_str::<toml::Value>("data_root = '.cargo-ai/data'\n").unwrap(),
    );
    write(&metadata_path, toml::to_string_pretty(&metadata).unwrap());
    eprintln!("Ownership: legacy write location preserved; explicitly adopted project data root.");
    for provider in ["openai", "gemini"] {
        run_profile(&author, &project, "patrol.json", None, provider);
        assert_csv(&project.join(".cargo-ai/data"), CSV, "dev");
        fs::remove_file(project.join(".cargo-ai/data/findings.csv")).unwrap();
    }
    let hatch = success(
        &author,
        &project,
        &["hatch", "patrol", "--config", "patrol.json"],
    );
    assert!(output_text(&hatch).contains("Cargo profile  release"));
    verify_provenance(
        &author,
        &project,
        &project.join(executable("patrol")),
        &target,
    );
    write(project.join(".cargo-ai/data/private.txt"), PRIVATE_DATA);
    write(
        project.join("tools/csv_writer/.cargo-ai/data/private.txt"),
        PRIVATE_DATA,
    );
    write(
        project.join("tools/csv_writer/.cargo-ai/guidance/private.md"),
        PRIVATE_GUIDANCE,
    );
    let built = author.root.join("built");
    let build = success(
        &author,
        &project,
        &["build", "default", "--output-dir", built.to_str().unwrap()],
    );
    assert!(output_text(&build).contains("Cargo profile: release"));
    let build_manifest = toml_file(built.join("cargo-ai-build.toml"));
    assert_eq!(build_manifest["profile"].as_str(), Some("default"));
    assert_eq!(
        build_manifest["cargo_compile_profile"].as_str(),
        Some("release")
    );
    assert_eq!(build_manifest["target"].as_str(), Some(target.as_str()));
    assert_eq!(
        build_manifest["hatched_agents"][0]["source"].as_str(),
        Some("patrol.json")
    );
    assert_eq!(
        build_manifest["hatched_agents"][0]["binary"].as_str(),
        Some(executable("patrol").as_str())
    );
    let built_tool = format!(
        ".cargo-ai/tools/csv_writer/bin/{target}/release/{}",
        executable("csv_writer")
    );
    exact_inventory(
        &tree(&built),
        &[
            ".cargo-ai/project.toml".into(),
            ".cargo-ai/tools/csv_writer/tool.json".into(),
            built_tool,
            "assets/patrol.png".into(),
            "patrol.json".into(),
            executable("patrol"),
            "cargo-ai-build.toml".into(),
        ],
    );
    assert_empty_data(&built.join(".cargo-ai/data"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            fs::metadata(built.join(executable("patrol")))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
    verify_provenance(&author, &built, &built.join(executable("patrol")), &target);
    let detached_source = author.root.join("detached-source");
    fs::rename(&project, &detached_source).unwrap();
    for provider in ["openai", "gemini"] {
        run_profile(
            &author,
            &built,
            "",
            Some(&built.join(executable("patrol"))),
            provider,
        );
        assert_csv(&built.join(".cargo-ai/data"), CSV, "release");
        fs::remove_file(built.join(".cargo-ai/data/findings.csv")).unwrap();
    }
    fs::rename(&detached_source, &project).unwrap();
    eprintln!("Ownership: interpreted and detached generated runs passed both image profiles.");
    let package = author.root.join("package");
    success(
        &author,
        &project,
        &[
            "package",
            "default",
            "--output-dir",
            package.to_str().unwrap(),
        ],
    );
    // Review the entire portable source payload and permission record before installation.
    let payload = tree(&package);
    for excluded in [
        ".cargo-ai/data",
        ".cargo-ai/guidance",
        "tools/csv_writer/target",
        "tools/csv_writer/.cargo-ai/data",
        "tools/csv_writer/.cargo-ai/guidance",
        ".cargo-ai/tools/csv_writer/bin",
    ] {
        assert!(
            !package.join(excluded).exists(),
            "excluded package directory: {excluded}"
        );
    }
    exact_inventory(
        &payload,
        &[
            ".cargo-ai/project.toml",
            ".cargo-ai/tools/csv_writer/tool.json",
            "cargo-ai-package.toml",
            "patrol.json",
            "assets/patrol.png",
            "tools/csv_writer/Cargo.toml",
            "tools/csv_writer/Cargo.lock",
            "tools/csv_writer/src/main.rs",
            "tools/csv_writer/src/lib.rs",
            "tools/csv_writer/src/tool.rs",
            "tools/csv_writer/src/agent_bridge.rs",
        ]
        .map(String::from),
    );
    let package_manifest = toml_file(package.join("cargo-ai-package.toml"));
    assert_eq!(package_manifest["format_version"].as_integer(), Some(1));
    assert_eq!(
        package_manifest["project_name"].as_str(),
        Some("animal_patrol")
    );
    assert_eq!(package_manifest["project_version"].as_str(), Some("0.1.0"));
    assert_eq!(package_manifest["profile"].as_str(), Some("default"));
    for (key, expected) in [
        ("agent_definitions", "patrol.json"),
        ("hatched_agents", "patrol.json"),
        ("tools", "csv_writer"),
        ("assets", "assets/patrol.png"),
    ] {
        assert_eq!(
            package_manifest[key].as_array().unwrap(),
            &vec![toml::Value::String(expected.into())]
        );
    }
    for (key, value) in [
        ("package_payload", "read"),
        ("package_data", "read_write"),
        ("project_workspace", "explicit_grant_required"),
        ("subprocess", "blocked_without_explicit_grant"),
    ] {
        assert_eq!(package_manifest["permissions"][key].as_str(), Some(value));
    }
    assert_eq!(payload["patrol.json"], DEFINITION.as_bytes());
    assert_eq!(payload["tools/csv_writer/src/main.rs"], TOOL.as_bytes());
    assert_eq!(payload["tools/csv_writer/Cargo.toml"], author_cargo);
    assert_eq!(payload["tools/csv_writer/Cargo.lock"], author_lock);
    assert_eq!(
        json_file(package.join(".cargo-ai/tools/csv_writer/tool.json"))["artifacts"],
        json!({})
    );
    for (path, bytes) in &payload {
        let text = String::from_utf8_lossy(bytes);
        for forbidden in [
            PRIVATE_DATA,
            PRIVATE_GUIDANCE,
            "ownership-openai-fake-token",
            "ownership-gemini-fake-token",
            "ownership-anthropic-fake-token",
            author.root.to_str().unwrap(),
        ] {
            assert!(!text.contains(forbidden), "{path} includes excluded state");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(package.join(path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0,
                "source package unexpectedly contains executable content"
            );
        }
    }
    eprintln!("Ownership: exact source package inventory, permissions and exclusions passed.");
    let recipient = Fixture::new("animal-recipient");
    let incoming = recipient.root.join("incoming");
    copy_tree(&package, &incoming);
    assert_eq!(tree(&incoming), payload);
    add_profiles(&recipient, &recipient.root);
    success(
        &recipient,
        &recipient.root,
        &[
            "packages",
            "install",
            incoming.to_str().unwrap(),
            "--as",
            "patrol",
        ],
    );
    let alias = recipient.cargo_ai_home.join("packages/patrol");
    let installed_payload = alias.join("package");
    assert_eq!(tree(&installed_payload), payload);
    assert_empty_data(&alias.join("data"));
    let runtime = alias.join("runtime");
    let runtime_artifact = format!("bin/{target}/release/{}", executable("csv_writer"));
    let runtime_files = [
        "tools/csv_writer/tool.json".to_string(),
        format!("tools/csv_writer/{runtime_artifact}"),
    ];
    let runtime_snapshot = tree(&runtime);
    exact_inventory(&runtime_snapshot, &runtime_files);
    assert_eq!(
        json_file(runtime.join("tools/csv_writer/tool.json")),
        json!({
            "schema_version": 1,
            "tool_id": "csv_writer",
            "binary": {"default_name": "csv_writer"},
            "artifacts": {(target.clone()): {"path": &runtime_artifact}}
        })
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            fs::metadata(runtime.join(&runtime_files[1]))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
    let mut alias_files = payload
        .keys()
        .map(|path| format!("package/{path}"))
        .collect::<Vec<_>>();
    alias_files.push("install.toml".to_string());
    alias_files.extend(runtime_files.iter().map(|path| format!("runtime/{path}")));
    exact_inventory(&tree(&alias), &alias_files);
    let install = toml_file(alias.join("install.toml"));
    assert_eq!(install["alias"].as_str(), Some("patrol"));
    assert_eq!(install["package_name"].as_str(), Some("animal_patrol"));
    assert_eq!(install["package_version"].as_str(), Some("0.1.0"));
    assert_eq!(install["profile"].as_str(), Some("default"));
    assert_eq!(install["source"]["kind"].as_str(), Some("local_root"));
    assert_eq!(install["source"]["path"].as_str(), incoming.to_str());
    assert_eq!(install["permissions"], package_manifest["permissions"]);
    let hash = install["content_sha256"].as_str().unwrap();
    assert_eq!(hash.len(), 64);
    assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(install["entrypoints"].as_array().unwrap().len(), 1);
    assert_eq!(install["entrypoints"][0]["name"].as_str(), Some("patrol"));
    assert_eq!(
        install["entrypoints"][0]["path"].as_str(),
        Some("patrol.json")
    );
    let inspection = success(
        &recipient,
        &recipient.root,
        &["packages", "inspect", "patrol"],
    );
    let inspection = output_text(&inspection);
    for expected in [
        "Identity:  animal_patrol",
        "patrol.json",
        "Permissions:",
        hash,
    ] {
        assert!(inspection.contains(expected));
    }
    assert_empty_data(&alias.join("data"));
    let parked = author.root.join("parked-source");
    fs::rename(&project, &parked).unwrap();
    fs::remove_dir_all(&incoming).unwrap();
    fs::remove_dir_all(&package).unwrap();
    for provider in ["openai", "gemini"] {
        run_profile(
            &recipient,
            &recipient.root,
            "patrol::patrol",
            None,
            provider,
        );
        assert_csv(&alias.join("data"), CSV, "release");
        fs::remove_file(alias.join("data/findings.csv")).unwrap();
    }
    assert_eq!(tree(&installed_payload), payload);
    assert_eq!(tree(&runtime), runtime_snapshot);
    assert!(!recipient.root.join("findings.csv").exists());
    assert!(!installed_payload.join("findings.csv").exists());
    fs::rename(&parked, &project).unwrap();
    let edited = TOOL.replace("\"original\".to_string()", "\"edited\".to_string()");
    assert_ne!(edited, TOOL);
    write(project.join("tools/csv_writer/src/main.rs"), edited);
    success(&author, &project, &["tools", "build", "csv_writer"]);
    run_profile(&author, &project, "patrol.json", None, "openai");
    assert_csv(
        &project.join(".cargo-ai/data"),
        &CSV.replace("original,", "edited,"),
        "dev",
    );
    run_profile(
        &recipient,
        &recipient.root,
        "patrol::patrol",
        None,
        "openai",
    );
    assert_csv(&alias.join("data"), CSV, "release");
    assert_eq!(tree(&installed_payload), payload);
    assert_eq!(tree(&runtime), runtime_snapshot);
    alias_files.extend([
        "data/findings.csv".to_string(),
        "data/compile-mode.txt".to_string(),
    ]);
    exact_inventory(&tree(&alias), &alias_files);
    assert_eq!(
        fs::read(project.join("tools/csv_writer/Cargo.toml")).unwrap(),
        author_cargo
    );
    assert_eq!(
        fs::read(project.join("tools/csv_writer/Cargo.lock")).unwrap(),
        author_lock
    );
    assert_eq!(
        fs::read_to_string(project.join("patrol.json")).unwrap(),
        DEFINITION
    );
    for path in ["AGENTS.md", "notes/field-notes.md"] {
        assert_eq!(fs::read_to_string(project.join(path)).unwrap(), USER_WORK);
    }
    assert_eq!(
        fs::read_to_string(project.join(".cargo-ai/data/private.txt")).unwrap(),
        PRIVATE_DATA
    );
    assert!(!project.join("findings.csv").exists());
    println!(
        "Ownership evidence: {}",
        json!({
            "target": target,
            "provider_runs": 9,
            "unsupported_profile_rejections": 1,
            "cargo_lock_sha256": format!("{:x}", Sha256::digest(&author_lock)),
            "install_content_sha256": hash,
            "package_file_sha256": payload.iter().map(|(path, bytes)|
                (path.clone(), format!("{:x}", Sha256::digest(bytes))))
                .collect::<BTreeMap<_, _>>(),
            "runtime_file_sha256": runtime_snapshot.iter().map(|(path, bytes)|
                (path.clone(), format!("{:x}", Sha256::digest(bytes))))
                .collect::<BTreeMap<_, _>>()
        })
    );
}
