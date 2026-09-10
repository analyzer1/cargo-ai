//! Offline metadata and acquisition boundaries. Live service privacy/races are separate proof.
#[path = "../templates/package_metadata.rs"]
mod metadata;
#[path = "support/qualification_paths.rs"]
mod qualification_paths;
#[allow(dead_code)]
mod support;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use support::{assert_success, Fixture};

fn package(fixture: &Fixture) -> std::path::PathBuf {
    let project = fixture.root.join("project");
    fs::create_dir_all(project.join(".cargo-ai")).unwrap();
    fs::write(project.join(".cargo-ai/project.toml"), "format_version = 1\n[project]\nname = \"inspection_fixture\"\nversion = \"1.0.0\"\ndescription = \"Publisher description\"\nlicense = \"MIT\"\n[build.default]\nassets = [\"readme.txt\"]\n").unwrap();
    fs::write(project.join("readme.txt"), "reviewable source content\n").unwrap();
    fs::create_dir_all(project.join(".cargo-ai/publish-requests")).unwrap();
    fs::write(
        project.join(".cargo-ai/publish-requests/pending.json"),
        "private-receipt-sentinel",
    )
    .unwrap();
    let assembled = fixture.root.join("package");
    let output = fixture
        .cargo_ai_command(&project)
        .args(["--no-update-check", "package", "--output-dir"])
        .arg(&assembled)
        .output()
        .unwrap();
    assert_success(&output, "assemble inspection package");
    assembled
}

#[test]
fn assembled_inventory_binds_bytes_and_rejects_tampering_before_install() {
    let fixture = Fixture::new("metadata");
    let assembled = package(&fixture);
    let raw = fs::read_to_string(assembled.join("cargo-ai-package.toml")).unwrap();
    let manifest: Value = toml::from_str(&raw).unwrap();
    metadata::validate_manifest_metadata(&manifest).unwrap();
    assert_eq!(
        manifest["inspection"]["publisher"]["description"],
        "Publisher description"
    );
    for file in manifest["inspection"]["files"].as_array().unwrap() {
        let path = file["path"].as_str().unwrap();
        assert!(!path.contains("publish-requests"));
        let bytes = fs::read(assembled.join(path)).unwrap();
        assert_eq!(file["bytes"].as_u64(), Some(bytes.len() as u64));
        assert_eq!(file["sha256"], format!("{:x}", Sha256::digest(bytes)));
    }
    assert!(!assembled.join(".cargo-ai/publish-requests").exists());
    fs::write(assembled.join("readme.txt"), "replaced after inspection").unwrap();
    let output = fixture
        .cargo_ai_command(&fixture.root)
        .args(["--no-update-check", "packages", "install"])
        .arg(&assembled)
        .args(["--as", "verified"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(support::output_text(&output).contains("inspection inventory"));
    assert!(!fixture
        .cargo_ai_home
        .join("packages/verified/install.toml")
        .exists());
}

#[test]
fn versioned_metadata_rejects_ambiguous_unbounded_and_hostile_claims() {
    let fixture = Fixture::new("claims");
    let assembled = package(&fixture);
    let manifest: Value =
        toml::from_str(&fs::read_to_string(assembled.join("cargo-ai-package.toml")).unwrap())
            .unwrap();
    for (pointer, value) in [
        ("/inspection/schema_version", json!(2)),
        ("/inspection/publisher/description", json!("x".repeat(4097))),
        ("/inspection/publisher/license", json!("MIT\u{1b}[2J")),
        ("/inspection/files/0/path", json!("../private")),
        ("/inspection/files/0/path", json!("C:\\secret")),
        (
            "/inspection/files/0/path",
            json!(".cargo-ai/publish-requests/request.json"),
        ),
        ("/inspection/files/0/sha256", json!("invalid")),
        ("/inspection/files/0/bytes", json!(104857601)),
    ] {
        let mut changed = manifest.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(
            metadata::validate_manifest_metadata(&changed).is_err(),
            "{pointer}"
        );
    }
    let mut changed = manifest.clone();
    changed["inspection"]["trusted"] = json!(true);
    assert!(metadata::validate_manifest_metadata(&changed).is_err());
    let mut changed = manifest.clone();
    let file = changed["inspection"]["files"][0].clone();
    changed["inspection"]["files"]
        .as_array_mut()
        .unwrap()
        .push(file);
    assert!(metadata::validate_manifest_metadata(&changed).is_err());
    let mut legacy = manifest;
    legacy.as_object_mut().unwrap().remove("inspection");
    assert!(metadata::validate_manifest_metadata(&legacy).is_ok());
}

#[test]
fn nested_qualification_inputs_are_portable_bounded_real_files() {
    let fixture = Fixture::new("paths");
    fs::create_dir_all(fixture.root.join("nested/package")).unwrap();
    fs::write(
        fixture
            .root
            .join("nested/package/cargo-ai-qualification.toml"),
        "bounded",
    )
    .unwrap();
    assert!(qualification_paths::confined_file(
        &fixture.root,
        "nested/package/cargo-ai-qualification.toml",
        64
    )
    .is_ok());
    for path in [
        "",
        ".",
        "../outside",
        "/absolute",
        "C:relative",
        "nested\\package",
        "nested//package",
        "nested/./package",
        "nested/../package",
        "nested\n",
    ] {
        assert!(!qualification_paths::relative_file(path), "{path:?}");
    }
    assert!(qualification_paths::confined_file(
        &fixture.root,
        "nested/package/cargo-ai-qualification.toml",
        1
    )
    .is_err());
    assert!(qualification_paths::confined_file(&fixture.root, "nested/package", 64).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(fixture.root.join("nested"), fixture.root.join("link")).unwrap();
        assert!(qualification_paths::confined_file(
            &fixture.root,
            "link/package/cargo-ai-qualification.toml",
            64
        )
        .is_err());
    }
}

#[test]
fn capabilities_follow_declared_definitions_instead_of_asset_extensions() {
    let fixture = Fixture::new("capability-declarations");
    let project = fixture.root.join("project");
    fs::create_dir_all(project.join(".cargo-ai")).unwrap();
    let definition = json!({"agent_definition_schema_version":"2026-09-09.r1",
        "inputs":[{"type":"image","path":"photo.png"}],
        "agent_schema":{"type":"object","properties":{"animal":{"type":"string"}}},
        "actions":[]});
    fs::write(
        project.join("agent.JSON"),
        serde_json::to_vec(&definition).unwrap(),
    )
    .unwrap();
    fs::write(
        project.join("inert.json"),
        serde_json::to_vec(&definition).unwrap(),
    )
    .unwrap();
    let generation = json!({"agent_definition_schema_version":"2026-09-09.r1",
        "agent_schema":{"type":"object","properties":{}},
        "actions":[{"name":"render","run":[{"kind":"generate_image",
            "prompt":"A deer in a forest","path":"./artifacts/deer.png"}]}]});
    fs::write(
        project.join("generation.json"),
        serde_json::to_vec(&generation).unwrap(),
    )
    .unwrap();
    for (label, declarations, expected) in [
        ("assets", "", Vec::<Value>::new()),
        (
            "generation",
            "agent_definitions = [\"generation.json\"]",
            vec![json!("image_generation")],
        ),
        (
            "declared",
            "agent_definitions = [\"agent.JSON\"]",
            vec![json!("image"), json!("structured_output"), json!("text")],
        ),
        (
            "hatched",
            "hatched_agents = [\"agent.JSON\"]",
            vec![json!("image"), json!("structured_output"), json!("text")],
        ),
    ] {
        fs::write(project.join(".cargo-ai/project.toml"), format!("format_version = 1\n[project]\nname = \"capability_fixture\"\nversion = \"1.0.0\"\n[build.default]\nassets = [\"inert.json\"]\n{declarations}\n")).unwrap();
        let assembled = fixture.root.join(label);
        let output = fixture
            .cargo_ai_command(&project)
            .args(["--no-update-check", "package", "--output-dir"])
            .arg(&assembled)
            .output()
            .unwrap();
        assert_success(&output, label);
        let manifest: Value =
            toml::from_str(&fs::read_to_string(assembled.join("cargo-ai-package.toml")).unwrap())
                .unwrap();
        assert_eq!(
            manifest["inspection"]["required_provider_capabilities"],
            json!(expected)
        );
    }
}
