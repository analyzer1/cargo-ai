//! Runtime behavior for `cargo ai add guidance`.
use clap::ArgMatches;
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::ui;

mod lifecycle;
#[cfg(feature = "developer-tools")]
pub(crate) mod packaging;
mod transaction;

const CANONICAL_GUIDANCE_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/cargo-ai.md.tmpl"
));
const CODEX_ENTRY_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/codex-agents.md.tmpl"
));
const CLAUDE_ENTRY_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/claude.md.tmpl"
));
const ACTION_RULES_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/action-rules.md"
));
const AGENT_DEFINITION_CONTRACT_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/agent-definition-contract.md"
));
const AUTHORING_PATTERNS_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/authoring-patterns.md"
));
const EXAMPLES_README_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/examples/README.md"
));
const START_HERE_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/start-here.md"
));
const PACKAGE_WORKFLOW_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/package-workflow.md"
));
const USAGE_LEDGER_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/usage-ledger.md"
));
const TOOL_AUTHORING_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/tool-authoring.md"
));
const TOOL_CONTRACT_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/tool-contract.md"
));
const TOOL_CHILD_AGENTS_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/tool-child-agents.md"
));
const TOOL_HARDENING_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/tool-hardening.md"
));
const PATTERN_SELECTION_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/pattern-selection.md"
));
const TROUBLESHOOTING_TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/troubleshooting.md"
));
const EXAMPLE_BASIC_AGENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/examples/basic-agent.json"
));
const EXAMPLE_SCHEMA_FEATURES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/examples/schema-features.json"
));
const EXAMPLE_RUNTIME_FILE_LOCAL_EXEC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/examples/runtime-file-local-exec.json"
));
const EXAMPLE_CHILD_AGENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/examples/child-agent.json"
));
const EXAMPLE_STOP_BY_DEFAULT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/examples/stop-by-default.json"
));
const EXAMPLE_CONTINUE_ON_FAILURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/examples/continue-on-failure.json"
));
const EXAMPLE_CONDITIONAL_WHEN: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/examples/conditional-when.json"
));
const EXAMPLE_RUNTIME_VARS_IMAGE_GATING: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/guidance/examples/runtime-vars-image-gating.json"
));

const ROOT_AGENTS_PATH: &str = "AGENTS.md";
const ROOT_CLAUDE_PATH: &str = "CLAUDE.md";
const BUNDLE_ENTRY_PATH: &str = ".cargo-ai/guidance/cargo-ai.md";

#[derive(Clone, Copy)]
struct GuidanceArtifact {
    relative_path: &'static str,
    contents: &'static str,
}

const GUIDANCE_ARTIFACTS: [GuidanceArtifact; 22] = [
    GuidanceArtifact {
        relative_path: BUNDLE_ENTRY_PATH,
        contents: CANONICAL_GUIDANCE_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/agent-definition-contract.md",
        contents: AGENT_DEFINITION_CONTRACT_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/action-rules.md",
        contents: ACTION_RULES_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/authoring-patterns.md",
        contents: AUTHORING_PATTERNS_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/examples/README.md",
        contents: EXAMPLES_README_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/start-here.md",
        contents: START_HERE_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/package-workflow.md",
        contents: PACKAGE_WORKFLOW_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/usage-ledger.md",
        contents: USAGE_LEDGER_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/tool-authoring.md",
        contents: TOOL_AUTHORING_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/tool-contract.md",
        contents: TOOL_CONTRACT_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/tool-child-agents.md",
        contents: TOOL_CHILD_AGENTS_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/tool-hardening.md",
        contents: TOOL_HARDENING_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/pattern-selection.md",
        contents: PATTERN_SELECTION_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/troubleshooting.md",
        contents: TROUBLESHOOTING_TEMPLATE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/examples/basic-agent.json",
        contents: EXAMPLE_BASIC_AGENT,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/examples/schema-features.json",
        contents: EXAMPLE_SCHEMA_FEATURES,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/examples/runtime-file-local-exec.json",
        contents: EXAMPLE_RUNTIME_FILE_LOCAL_EXEC,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/examples/child-agent.json",
        contents: EXAMPLE_CHILD_AGENT,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/examples/stop-by-default.json",
        contents: EXAMPLE_STOP_BY_DEFAULT,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/examples/continue-on-failure.json",
        contents: EXAMPLE_CONTINUE_ON_FAILURE,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/examples/conditional-when.json",
        contents: EXAMPLE_CONDITIONAL_WHEN,
    },
    GuidanceArtifact {
        relative_path: ".cargo-ai/guidance/examples/runtime-vars-image-gating.json",
        contents: EXAMPLE_RUNTIME_VARS_IMAGE_GATING,
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntrypointStatus {
    Written,
    Reused,
    Preserved,
}

#[derive(Debug)]
struct GuidanceEntrypointReport {
    style: GuidanceStyle,
    root_output_path: PathBuf,
    status: EntrypointStatus,
}

#[derive(Debug)]
struct GuidanceBundleReport {
    entrypoints: Vec<GuidanceEntrypointReport>,
    guidance_entry_path: PathBuf,
    guidance_root: PathBuf,
    written_paths: Vec<PathBuf>,
    reused_paths: Vec<PathBuf>,
}

fn display_path(path: &Path) -> String {
    if path.is_relative() {
        return path.display().to_string();
    }

    match std::env::current_dir() {
        Ok(current_dir) => match path.strip_prefix(&current_dir) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
            Ok(relative) => format!("./{}", relative.display()),
            Err(_) => path.display().to_string(),
        },
        Err(_) => path.display().to_string(),
    }
}

fn guidance_success_ui_response(report: &GuidanceBundleReport) -> serde_json::Value {
    let entry_files = report
        .entrypoints
        .iter()
        .map(|entrypoint| format!("`{}`", display_path(&entrypoint.root_output_path)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sections = vec![json!({
        "type": "kv",
        "title": "Output",
        "title_style": "plain",
        "layout": "aligned",
        "items": [
            {
                "label": "Entry files",
                "value": entry_files
            },
            {
                "label": "Bundle",
                "value": format!("`{}`", display_path(report.guidance_root.as_path()))
            },
            {
                "label": "Files written",
                "value": report.written_paths.len()
            },
            {
                "label": "Files reused",
                "value": report.reused_paths.len()
            }
        ]
    })];

    for entrypoint in report
        .entrypoints
        .iter()
        .filter(|entrypoint| entrypoint.status == EntrypointStatus::Preserved)
    {
        sections.push(json!({
            "type": "notice",
            "title": format!("Existing {}", entrypoint.style.root_filename()),
            "title_style": "plain",
            "message": format!(
                "Existing `{}` was left unchanged. Add this section when you want {} to load Cargo AI guidance:\n\n{}",
                display_path(&entrypoint.root_output_path),
                entrypoint.style.display_name(),
                entrypoint.style.merge_snippet(display_path(&report.guidance_entry_path).as_str())
            )
        }));
    }

    let style_names = report
        .entrypoints
        .iter()
        .map(|entrypoint| entrypoint.style.display_name())
        .collect::<Vec<_>>()
        .join(" and ");
    let preserved_count = report
        .entrypoints
        .iter()
        .filter(|entrypoint| entrypoint.status == EntrypointStatus::Preserved)
        .count();
    let summary = if preserved_count == 0 {
        format!("Installed Cargo AI guidance for {style_names}.")
    } else {
        format!(
            "Installed Cargo AI guidance for {style_names} and left {preserved_count} existing entry file(s) unchanged."
        )
    };

    json!({
        "ui": {
            "schema": "1.0",
            "kind": "success",
            "icon": "✓",
            "title": "Guidance added",
            "summary": summary,
            "sections": sections
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuidanceStyle {
    Codex,
    Claude,
}

impl GuidanceStyle {
    fn from_cli(value: &str) -> Result<Self, String> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            other => Err(format!(
                "Unsupported guidance style '{}'. Use `--style codex` or `--style claude`.",
                other
            )),
        }
    }

    fn root_artifact(self) -> GuidanceArtifact {
        match self {
            Self::Codex => GuidanceArtifact {
                relative_path: ROOT_AGENTS_PATH,
                contents: CODEX_ENTRY_TEMPLATE,
            },
            Self::Claude => GuidanceArtifact {
                relative_path: ROOT_CLAUDE_PATH,
                contents: CLAUDE_ENTRY_TEMPLATE,
            },
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    fn root_filename(self) -> &'static str {
        self.root_artifact().relative_path
    }

    fn merge_snippet(self, bundle_entry: &str) -> String {
        match self {
            Self::Codex => format!(
                "## Cargo AI Guidance\n\nWhen creating or editing Cargo AI agents in this repository, read `{bundle_entry}` first and follow its linked local guidance.\n\nValidate agent definitions with:\n\n`cargo ai hatch <agent-name> --config <config.json> --check`"
            ),
            Self::Claude => format!(
                "## Cargo AI Guidance\n\n@{bundle_entry}\n\nValidate agent definitions with:\n\n`cargo ai hatch <agent-name> --config <config.json> --check`"
            ),
        }
    }
}

fn write_guidance_bundle(
    target_dir: &Path,
    styles: &[GuidanceStyle],
) -> Result<GuidanceBundleReport, String> {
    lifecycle::add(target_dir, styles)
}

/// Runs offline status/update without initializing user-level Cargo AI state.
pub(crate) fn run_lifecycle(sub_m: &ArgMatches) -> bool {
    let result = (|| {
        let root = std::env::current_dir()
            .map_err(|error| format!("Failed to resolve current directory: {error}"))?;
        if sub_m.subcommand_matches("status").is_some() {
            lifecycle::print_status(&root)
        } else if sub_m.subcommand_matches("update").is_some() {
            lifecycle::update(&root)
        } else {
            Err("Use `cargo ai guidance status` or `cargo ai guidance update`.".to_string())
        }
    })();
    if let Err(error) = result {
        eprintln!("x {error}");
        false
    } else {
        true
    }
}

/// Operational guidance state must never become an application asset.
pub(crate) fn is_reserved_guidance_path(relative: &Path) -> bool {
    let parts = relative
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect::<Vec<_>>();
    parts.windows(2).any(|parts| {
        parts[0].eq_ignore_ascii_case(".cargo-ai")
            && (parts[1].eq_ignore_ascii_case("guidance.lock")
                || parts[1].eq_ignore_ascii_case("guidance-transaction"))
    })
}

/// Classifies generated loaders without claiming ownership of user instructions.
pub(crate) fn is_managed_entrypoint(path: &Path) -> Result<bool, String> {
    lifecycle::is_managed_entrypoint(path)
}

/// Executes the `guidance` subcommand flow from parsed CLI arguments.
pub fn run(sub_m: &ArgMatches) -> bool {
    if let Err(error) = run_impl(sub_m) {
        eprintln!("x {}", error);
        return false;
    }

    true
}

fn run_impl(sub_m: &ArgMatches) -> Result<(), String> {
    let mut styles = Vec::new();
    for value in sub_m.get_many::<String>("style").ok_or_else(|| {
        "Missing guidance style. Use `cargo ai add guidance --style codex` or `--style claude`."
            .to_string()
    })? {
        let style = GuidanceStyle::from_cli(value)?;
        if !styles.contains(&style) {
            styles.push(style);
        }
    }
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("Failed to resolve current directory: {error}"))?;
    let report = write_guidance_bundle(&current_dir, &styles)?;
    ui::account_status::render_backend_ui(&guidance_success_ui_response(&report));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        guidance_success_ui_response, write_guidance_bundle, EntrypointStatus,
        GuidanceBundleReport, GuidanceEntrypointReport, GuidanceStyle, PACKAGE_WORKFLOW_TEMPLATE,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir_path(stem: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cargo-ai-add-guidance-test-{}-{}", stem, nanos))
    }

    #[test]
    fn write_guidance_bundle_writes_agents_md_and_offline_assets_for_codex() {
        let dir = temp_dir_path("codex");
        fs::create_dir_all(&dir).expect("test dir should be created");

        let report = write_guidance_bundle(&dir, &[GuidanceStyle::Codex])
            .expect("guidance write should work");
        assert_eq!(
            report.entrypoints[0]
                .root_output_path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("AGENTS.md")
        );
        assert_eq!(report.entrypoints[0].status, EntrypointStatus::Written);
        assert_eq!(report.written_paths.len(), 24);
        assert!(dir.join("AGENTS.md").exists());
        assert!(dir.join(".cargo-ai/guidance/cargo-ai.md").exists());
        assert!(dir
            .join(".cargo-ai/guidance/agent-definition-contract.md")
            .exists());
        assert!(dir.join(".cargo-ai/guidance/action-rules.md").exists());
        assert!(dir
            .join(".cargo-ai/guidance/authoring-patterns.md")
            .exists());
        assert!(dir.join(".cargo-ai/guidance/examples/README.md").exists());
        assert!(dir.join(".cargo-ai/guidance/start-here.md").exists());
        assert!(dir.join(".cargo-ai/guidance/package-workflow.md").exists());
        assert!(dir.join(".cargo-ai/guidance/usage-ledger.md").exists());
        assert!(dir.join(".cargo-ai/guidance/tool-authoring.md").exists());
        assert!(dir.join(".cargo-ai/guidance/tool-contract.md").exists());
        assert!(dir.join(".cargo-ai/guidance/tool-child-agents.md").exists());
        assert!(dir.join(".cargo-ai/guidance/tool-hardening.md").exists());
        assert!(dir.join(".cargo-ai/guidance/pattern-selection.md").exists());
        assert!(dir.join(".cargo-ai/guidance/troubleshooting.md").exists());
        assert!(dir
            .join(".cargo-ai/guidance/examples/basic-agent.json")
            .exists());
        assert!(dir
            .join(".cargo-ai/guidance/examples/schema-features.json")
            .exists());
        assert!(dir
            .join(".cargo-ai/guidance/examples/runtime-file-local-exec.json")
            .exists());
        assert!(dir
            .join(".cargo-ai/guidance/examples/child-agent.json")
            .exists());
        assert!(dir
            .join(".cargo-ai/guidance/examples/stop-by-default.json")
            .exists());
        assert!(dir
            .join(".cargo-ai/guidance/examples/continue-on-failure.json")
            .exists());
        assert!(dir
            .join(".cargo-ai/guidance/examples/conditional-when.json")
            .exists());
        assert!(dir
            .join(".cargo-ai/guidance/examples/runtime-vars-image-gating.json")
            .exists());

        let guidance = fs::read_to_string(&report.entrypoints[0].root_output_path)
            .expect("guidance output should be readable");
        assert!(guidance.contains(".cargo-ai/guidance/cargo-ai.md"));
        assert!(guidance.contains("cargo ai hatch <agent-name> --config <config.json> --check"));

        let bundle_entry = fs::read_to_string(&report.guidance_entry_path)
            .expect("bundle entry guidance should be readable");
        assert!(bundle_entry.contains("Cargo AI Agent Authoring"));
        assert!(!bundle_entry.contains("Codex"));
        assert!(bundle_entry.contains(".cargo-ai/guidance/start-here.md"));
        assert!(bundle_entry.contains(".cargo-ai/guidance/package-workflow.md"));

        let package_workflow =
            fs::read_to_string(dir.join(".cargo-ai/guidance/package-workflow.md"))
                .expect("package workflow guidance should be readable");
        assert_eq!(package_workflow, PACKAGE_WORKFLOW_TEMPLATE);
        assert!(package_workflow.contains("cargo ai packages publish"));
        assert!(package_workflow.contains("cargo ai packages update"));
        assert!(package_workflow.contains("cargo ai packages rollback"));
        assert!(package_workflow.contains("--accept-permissions"));
        assert!(package_workflow.contains("runtime/"));
        assert!(package_workflow.contains("data/"));
        assert!(package_workflow.contains("[package_dependencies"));

        let rules = fs::read_to_string(dir.join(".cargo-ai/guidance/action-rules.md"))
            .expect("action rules should be readable");
        assert!(rules.contains("failure_mode"));
        assert!(rules.contains("status_variable"));
        assert!(rules.contains("when"));
        assert!(rules.contains("runtime.*"));
        assert!(rules.contains("agent-definition-contract.md"));

        let start_here = fs::read_to_string(dir.join(".cargo-ai/guidance/start-here.md"))
            .expect("start-here guidance should be readable");
        assert!(start_here.contains("What do you want this agent to do?"));
        assert!(start_here.contains("current machine"));
        assert!(start_here.contains("portable across macOS, Windows, and Linux"));
        assert!(start_here.contains("usage-ledger.md"));
        assert!(start_here.contains("`package-workflow.md`"));

        let usage_ledger = fs::read_to_string(dir.join(".cargo-ai/guidance/usage-ledger.md"))
            .expect("usage ledger guidance should be readable");
        assert!(usage_ledger.contains("--usage-log <path>"));
        assert!(usage_ledger.contains("CARGO_AI_USAGE_LOG=<path>"));
        assert!(usage_ledger.contains("provider_request_completed"));
        assert!(usage_ledger.contains("definition_sha256"));

        let tool_authoring = fs::read_to_string(dir.join(".cargo-ai/guidance/tool-authoring.md"))
            .expect("tool authoring guidance should be readable");
        assert!(tool_authoring.contains("cargo ai add tool <tool_name>"));
        assert!(tool_authoring.contains("cargo ai tools build <tool_name> --target <triple>"));
        assert!(tool_authoring.contains("tool-contract.md"));
        assert!(tool_authoring.contains("`package-workflow.md`"));

        let tool_contract = fs::read_to_string(dir.join(".cargo-ai/guidance/tool-contract.md"))
            .expect("tool contract guidance should be readable");
        assert!(tool_contract.contains("\"kind\": \"tool\""));
        assert!(tool_contract.contains("describe"));
        assert!(tool_contract.contains("invoke"));

        let tool_child_agents =
            fs::read_to_string(dir.join(".cargo-ai/guidance/tool-child-agents.md"))
                .expect("tool child-agents guidance should be readable");
        assert!(tool_child_agents.contains("InvocationContext"));
        assert!(tool_child_agents.contains("ChildAgentRequest"));

        let tool_hardening = fs::read_to_string(dir.join(".cargo-ai/guidance/tool-hardening.md"))
            .expect("tool hardening guidance should be readable");
        assert!(tool_hardening.contains("cargo audit"));
        assert!(tool_hardening.contains("resource_profile"));

        let patterns = fs::read_to_string(dir.join(".cargo-ai/guidance/pattern-selection.md"))
            .expect("pattern selection guidance should be readable");
        assert!(patterns.contains("basic-agent.json"));
        assert!(patterns.contains("schema-features.json"));
        assert!(patterns.contains("runtime-file-local-exec.json"));
        assert!(patterns.contains("continue-on-failure.json"));
        assert!(patterns.contains("runtime-vars-image-gating.json"));

        let authoring = fs::read_to_string(dir.join(".cargo-ai/guidance/authoring-patterns.md"))
            .expect("authoring patterns guidance should be readable");
        assert!(authoring.contains("version"));
        assert!(authoring.contains("boxed ASCII"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_guidance_bundle_preserves_existing_agents_md_and_writes_bundle() {
        let dir = temp_dir_path("conflict");
        fs::create_dir_all(&dir).expect("test dir should be created");
        fs::write(dir.join("AGENTS.md"), "existing guidance\n")
            .expect("existing guidance file should be written");

        let report = write_guidance_bundle(&dir, &[GuidanceStyle::Codex])
            .expect("guidance write should work");
        assert_eq!(report.entrypoints[0].status, EntrypointStatus::Preserved);
        assert_eq!(report.written_paths.len(), 23);
        assert_eq!(
            fs::read_to_string(dir.join("AGENTS.md")).expect("existing AGENTS should be readable"),
            "existing guidance\n"
        );
        assert!(dir.join(".cargo-ai/guidance/cargo-ai.md").exists());
        assert!(dir.join(".cargo-ai/guidance/action-rules.md").exists());
        assert!(dir.join(".cargo-ai/guidance/package-workflow.md").exists());
        assert_eq!(
            fs::read_to_string(dir.join(".cargo-ai/guidance/package-workflow.md"))
                .expect("package workflow guidance should be readable"),
            PACKAGE_WORKFLOW_TEMPLATE
        );

        let response = guidance_success_ui_response(&report);
        assert_eq!(
            response["ui"]["summary"].as_str(),
            Some("Installed Cargo AI guidance for Codex and left 1 existing entry file(s) unchanged.")
        );
        assert_eq!(
            response["ui"]["sections"][0]["items"][0]["label"].as_str(),
            Some("Entry files")
        );
        assert!(response["ui"]["sections"][0]["items"][0]["value"]
            .as_str()
            .expect("entry file value should be rendered")
            .contains("AGENTS.md"));
        assert!(response["ui"]["sections"][1]["message"]
            .as_str()
            .expect("merge guidance should be present")
            .contains(".cargo-ai/guidance/cargo-ai.md"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_guidance_bundle_fails_when_companion_asset_exists() {
        let dir = temp_dir_path("companion-conflict");
        let conflict_path = dir.join(".cargo-ai/guidance/action-rules.md");
        fs::create_dir_all(
            conflict_path
                .parent()
                .expect("conflict parent should be available"),
        )
        .expect("conflict parent should be created");
        fs::write(&conflict_path, "existing guidance rules\n")
            .expect("existing companion file should be written");

        let error = write_guidance_bundle(&dir, &[GuidanceStyle::Codex])
            .expect_err("existing companion file should fail");
        assert!(error.contains(".cargo-ai/guidance/action-rules.md"));
        assert!(error.contains("legacy/unmanaged"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn guidance_success_ui_uses_compact_output_section() {
        let report = GuidanceBundleReport {
            entrypoints: vec![GuidanceEntrypointReport {
                style: GuidanceStyle::Codex,
                root_output_path: PathBuf::from("./AGENTS.md"),
                status: EntrypointStatus::Written,
            }],
            guidance_entry_path: PathBuf::from("./.cargo-ai/guidance/cargo-ai.md"),
            guidance_root: PathBuf::from("./.cargo-ai/guidance"),
            written_paths: vec![
                PathBuf::from("./AGENTS.md"),
                PathBuf::from("./.cargo-ai/guidance/start-here.md"),
            ],
            reused_paths: vec![],
        };

        let response = guidance_success_ui_response(&report);

        assert_eq!(response["ui"]["title"].as_str(), Some("Guidance added"));
        assert_eq!(
            response["ui"]["sections"][0]["items"][0]["value"].as_str(),
            Some("`./AGENTS.md`")
        );
        assert_eq!(
            response["ui"]["sections"][0]["items"][1]["value"].as_str(),
            Some("`./.cargo-ai/guidance`")
        );
        assert_eq!(response["ui"]["sections"][0]["items"][2]["value"], 2);
    }

    #[test]
    fn guidance_success_ui_includes_merge_snippet_when_agents_md_is_preserved() {
        let report = GuidanceBundleReport {
            entrypoints: vec![GuidanceEntrypointReport {
                style: GuidanceStyle::Codex,
                root_output_path: PathBuf::from("./AGENTS.md"),
                status: EntrypointStatus::Preserved,
            }],
            guidance_entry_path: PathBuf::from("./.cargo-ai/guidance/cargo-ai.md"),
            guidance_root: PathBuf::from("./.cargo-ai/guidance"),
            written_paths: vec![PathBuf::from("./.cargo-ai/guidance/cargo-ai.md")],
            reused_paths: vec![],
        };

        let response = guidance_success_ui_response(&report);

        assert_eq!(
            response["ui"]["sections"][0]["items"][0]["value"].as_str(),
            Some("`./AGENTS.md`")
        );
        assert_eq!(response["ui"]["sections"][0]["items"][2]["value"], 1);
        let message = response["ui"]["sections"][1]["message"]
            .as_str()
            .expect("merge snippet should be rendered");
        assert!(message.contains("Existing `./AGENTS.md` was left unchanged"));
        assert!(message.contains("## Cargo AI Guidance"));
        assert!(message.contains("cargo ai hatch <agent-name> --config <config.json> --check"));
    }

    #[test]
    fn write_guidance_bundle_installs_codex_and_claude_with_one_shared_bundle() {
        let dir = temp_dir_path("combined");
        fs::create_dir_all(&dir).expect("test dir should be created");

        let report = write_guidance_bundle(&dir, &[GuidanceStyle::Codex, GuidanceStyle::Claude])
            .expect("combined guidance write should work");

        assert_eq!(report.entrypoints.len(), 2);
        assert_eq!(report.written_paths.len(), 25);
        assert!(dir.join("AGENTS.md").exists());
        assert!(dir.join("CLAUDE.md").exists());
        assert!(dir.join(".cargo-ai/guidance/cargo-ai.md").exists());
        assert!(!dir.join(".cargo-ai/guidance/codex-agents.md").exists());
        let claude = fs::read_to_string(dir.join("CLAUDE.md"))
            .expect("Claude entrypoint should be readable");
        assert!(claude.contains("@.cargo-ai/guidance/cargo-ai.md"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_guidance_bundle_reuses_identical_bundle_for_later_style() {
        let dir = temp_dir_path("later-style");
        fs::create_dir_all(&dir).expect("test dir should be created");
        let dir = fs::canonicalize(dir).expect("test directory should resolve");
        write_guidance_bundle(&dir, &[GuidanceStyle::Codex])
            .expect("first guidance write should work");

        let report = write_guidance_bundle(&dir, &[GuidanceStyle::Claude])
            .expect("later style should reuse the shared bundle");

        assert_eq!(
            report.written_paths,
            vec![
                dir.join(".cargo-ai/guidance/manifest.json"),
                dir.join("CLAUDE.md")
            ]
        );
        assert_eq!(report.reused_paths.len(), 22);
        assert_eq!(report.entrypoints[0].status, EntrypointStatus::Written);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_guidance_bundle_reuses_identical_entrypoint_without_rewriting() {
        let dir = temp_dir_path("repeat-style");
        fs::create_dir_all(&dir).expect("test dir should be created");
        write_guidance_bundle(&dir, &[GuidanceStyle::Codex])
            .expect("first guidance write should work");

        let report = write_guidance_bundle(&dir, &[GuidanceStyle::Codex])
            .expect("repeated style should be idempotent");

        assert!(report.written_paths.is_empty());
        assert_eq!(report.reused_paths.len(), 24);
        assert_eq!(report.entrypoints[0].status, EntrypointStatus::Reused);

        let _ = fs::remove_dir_all(dir);
    }
}
