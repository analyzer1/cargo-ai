//! Exercises installed guidance through the actual CLI and filesystem boundary.
#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use support::{assert_success, output_text, Fixture};

fn cli(fixture: &Fixture, root: &Path, args: &[&str]) -> Output {
    fixture
        .cargo_ai_command(root)
        .env("CARGO_NET_OFFLINE", "true")
        .args(args)
        .output()
        .expect("guidance CLI should start")
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut result = BTreeMap::new();
    if !root.exists() {
        return result;
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        let contents = if metadata.is_dir() {
            pending.extend(
                fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
            None
        } else {
            Some(fs::read(&path).unwrap())
        };
        result.insert(path.strip_prefix(root).unwrap().to_path_buf(), contents);
    }
    result
}

fn project(root: &Path) {
    fs::create_dir_all(root.join(".cargo-ai")).unwrap();
    fs::write(
        root.join(".cargo-ai/project.toml"),
        "format_version = 1\nvcs = \"none\"\n",
    )
    .unwrap();
}

fn add(fixture: &Fixture, root: &Path) {
    assert_success(
        &cli(
            fixture,
            root,
            &["add", "guidance", "--style", "codex", "--style", "claude"],
        ),
        "add both guidance entrypoints",
    );
}

fn assert_state(fixture: &Fixture, root: &Path, state: &str) {
    let before = snapshot(root);
    let home = snapshot(&fixture.cargo_ai_home);
    let result = cli(fixture, root, &["guidance", "status"]);
    assert_success(&result, "read-only guidance status");
    assert!(
        output_text(&result).contains(&format!("Guidance status: {state}")),
        "{}",
        output_text(&result)
    );
    assert_eq!(snapshot(root), before, "status must not repair the project");
    assert_eq!(
        snapshot(&fixture.cargo_ai_home),
        home,
        "status must bypass home startup writes"
    );
}

#[test]
fn installed_guidance_preserves_home_user_instructions_and_repeated_updates() {
    let fixture = Fixture::new("guidance-preservation");
    let root = fixture.root.join("project");
    project(&root);
    fs::write(root.join("AGENTS.md"), "User-owned instructions\n").unwrap();
    fs::write(root.join("notes.txt"), "Existing work\n").unwrap();
    fs::remove_dir(&fixture.cargo_ai_home).unwrap();
    assert_state(&fixture, &root, "missing");
    assert!(!fixture.cargo_ai_home.exists());
    add(&fixture, &root);
    assert!(
        !fixture.cargo_ai_home.exists(),
        "add must not initialize a home"
    );
    assert_eq!(
        fs::read(root.join("AGENTS.md")).unwrap(),
        b"User-owned instructions\n"
    );
    assert!(fs::read_to_string(root.join("CLAUDE.md"))
        .unwrap()
        .contains(".cargo-ai/guidance/"));
    assert!(!root.join(".gitignore").exists());
    assert_state(&fixture, &root, "current");

    fs::create_dir(&fixture.cargo_ai_home).unwrap();
    let legacy_config = "secret_store = \"file\"\nprofile = [{ name = \"fixture\", server = \"openai\", model = \"fixture-model\", token = \"fake-legacy-token-preserve\", timeout_in_sec = 60 }]\n";
    fs::write(fixture.cargo_ai_home.join("config.toml"), legacy_config).unwrap();
    fs::write(
        fixture.cargo_ai_home.join("credentials.toml"),
        "profile_tokens = { untouched = \"fake-existing-token\" }\n",
    )
    .unwrap();
    fs::write(fixture.fallback_home.join("sentinel"), "unchanged").unwrap();
    let home = snapshot(&fixture.cargo_ai_home);
    let fallback = snapshot(&fixture.fallback_home);
    let before = snapshot(&root);
    for _ in 0..2 {
        assert_state(&fixture, &root, "current");
        let update = cli(&fixture, &root, &["guidance", "update"]);
        assert_success(&update, "idempotent guidance update");
        assert!(!output_text(&update).contains("fake-legacy-token-preserve"));
        assert_eq!(snapshot(&root), before);
        assert_eq!(snapshot(&fixture.cargo_ai_home), home);
        assert_eq!(snapshot(&fixture.fallback_home), fallback);
    }
}

#[test]
fn guidance_blocked_states_leave_all_existing_bytes_unchanged() {
    for (case, state) in [
        ("modified", "locally modified"),
        ("missing", "incomplete"),
        ("manifest", "malformed"),
        ("legacy", "legacy/unmanaged"),
    ] {
        let fixture = Fixture::new("guidance-blocked");
        let root = fixture.root.join("project");
        project(&root);
        add(&fixture, &root);
        let bundle = root.join(".cargo-ai/guidance");
        match case {
            "modified" => fs::write(bundle.join("start-here.md"), "user changes\n").unwrap(),
            "missing" => fs::remove_file(bundle.join("start-here.md")).unwrap(),
            "manifest" => fs::write(bundle.join("manifest.json"), "{ broken").unwrap(),
            "legacy" => fs::remove_file(bundle.join("manifest.json")).unwrap(),
            _ => unreachable!(),
        }
        assert_state(&fixture, &root, state);
        let before = snapshot(&root);
        let update = cli(&fixture, &root, &["guidance", "update"]);
        assert!(!update.status.success(), "{case}: {}", output_text(&update));
        assert_eq!(
            snapshot(&root),
            before,
            "{case} must not mutate blocked state"
        );
    }
}

#[test]
fn build_and_package_exclude_incidental_guidance_and_keep_declared_assets() {
    let fixture = Fixture::new("guidance-assets");
    let root = fixture.root.join("project");
    let nested = root.join("assets/vendor");
    project(&root);
    project(&nested);
    add(&fixture, &nested);
    fs::write(
        nested.join("AGENTS.md"),
        "User changed these instructions\n",
    )
    .unwrap();
    fs::write(nested.join("README.md"), "Owned source\n").unwrap();
    fs::create_dir_all(nested.join(".cargo-ai/guidance-transaction")).unwrap();
    fs::write(
        nested.join(".cargo-ai/guidance-transaction/sentinel"),
        "recovery bytes",
    )
    .unwrap();
    let metadata = "format_version = 1\nvcs = \"none\"\n[project]\nname = \"guidance_assets\"\nversion = \"0.1.0\"\n[build.default]\nassets = [\"assets\"]\n[build.explicit]\nassets = [\"assets/vendor/.cargo-ai/guidance\", \"assets/vendor/CLAUDE.md\"]\n[build.forbidden]\nassets = [\"assets/vendor/.cargo-ai/guidance.lock\"]\n";
    fs::write(root.join(".cargo-ai/project.toml"), metadata).unwrap();
    for command in ["build", "package"] {
        let output_root = fixture.root.join(format!("{command}-ordinary"));
        assert_success(
            &cli(
                &fixture,
                &root,
                &[
                    "--no-update-check",
                    command,
                    "default",
                    "--output-dir",
                    output_root.to_str().unwrap(),
                ],
            ),
            "assemble ordinary declared assets",
        );
        let copied = output_root.join("assets/vendor");
        assert_eq!(
            fs::read(copied.join("README.md")).unwrap(),
            b"Owned source\n"
        );
        assert_eq!(
            fs::read(copied.join("AGENTS.md")).unwrap(),
            b"User changed these instructions\n"
        );
        assert!(!copied.join("CLAUDE.md").exists());
        for path in ["guidance", "guidance.lock", "guidance-transaction"] {
            assert!(!copied.join(".cargo-ai").join(path).exists());
        }
        let explicit = fixture.root.join(format!("{command}-explicit"));
        assert_success(
            &cli(
                &fixture,
                &root,
                &[
                    "--no-update-check",
                    command,
                    "explicit",
                    "--output-dir",
                    explicit.to_str().unwrap(),
                ],
            ),
            "assemble explicitly declared guidance",
        );
        assert!(explicit
            .join("assets/vendor/.cargo-ai/guidance/start-here.md")
            .is_file());
        assert!(explicit.join("assets/vendor/CLAUDE.md").is_file());
        let forbidden = fixture.root.join(format!("{command}-forbidden"));
        let result = cli(
            &fixture,
            &root,
            &[
                "--no-update-check",
                command,
                "forbidden",
                "--output-dir",
                forbidden.to_str().unwrap(),
            ],
        );
        assert!(!result.status.success());
        assert!(
            !forbidden.exists(),
            "reserved state must fail during preflight"
        );
    }

    assert_success(
        &cli(
            &fixture,
            &root,
            &["--no-update-check", "add", "tool", "helper"],
        ),
        "scaffold project-owned tool source",
    );
    let tool_root = root.join("tools/helper");
    add(&fixture, &tool_root);
    fs::write(
        tool_root.join("AGENTS.md"),
        "Retain user tool instructions\n",
    )
    .unwrap();
    fs::create_dir_all(tool_root.join(".cargo-ai/guidance-transaction")).unwrap();
    fs::write(
        tool_root.join(".cargo-ai/guidance-transaction/sentinel"),
        "recovery bytes",
    )
    .unwrap();
    fs::write(
        root.join(".cargo-ai/project.toml"),
        format!("{metadata}\n[build.tool_source]\ntools = [\"helper\"]\n"),
    )
    .unwrap();
    let output = fixture.root.join("tool-source-package");
    assert_success(
        &cli(
            &fixture,
            &root,
            &[
                "--no-update-check",
                "package",
                "tool_source",
                "--output-dir",
                output.to_str().unwrap(),
            ],
        ),
        "package tool source with incidental guidance",
    );
    let copied = output.join("tools/helper");
    assert!(copied.join("src/tool.rs").is_file());
    assert_eq!(
        fs::read(copied.join("AGENTS.md")).unwrap(),
        b"Retain user tool instructions\n"
    );
    assert!(!copied.join("CLAUDE.md").exists());
    for name in ["guidance", "guidance.lock", "guidance-transaction"] {
        assert!(!copied.join(".cargo-ai").join(name).exists());
    }
}
