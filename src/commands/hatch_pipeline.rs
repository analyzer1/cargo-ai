//! Shared hatch pipeline helpers.
//!
//! This module centralizes scaffold/build/export/cleanup execution and config
//! source resolution for hatch-style flows.
use std::fs;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

use crate::agent_builder::build_target::{BuildTarget, CargoCompileProfile};

/// Execution mode for hatch pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HatchMode {
    Build,
    Check,
}

impl HatchMode {
    fn compile_profile(self) -> CargoCompileProfile {
        match self {
            Self::Build => CargoCompileProfile::Release,
            Self::Check => CargoCompileProfile::Dev,
        }
    }
}

#[cfg_attr(not(feature = "developer-tools"), allow(dead_code))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HatchSource {
    Registry {
        name: String,
    },
    LocalFile {
        path: String,
    },
    InlineJson,
    StdinJson,
    Account {
        owner: String,
        agent: String,
        path: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HatchPresentation {
    pub source: HatchSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HatchProgressStep {
    PreparingDefinition,
    PreparingBuildTemplate,
    BuildingLocalAgent,
    CheckingLocalAgent,
    ExportingBinary,
    CleaningWorkspace,
    FinalizingWorkspace,
}

/// Request used to run a hatch/check pipeline.
#[derive(Clone, Debug)]
pub(crate) struct HatchRequest {
    pub project_name: String,
    pub file_contents: String,
    pub mode: HatchMode,
    pub force_overwrite: bool,
    pub keep_project: bool,
    pub build_target: BuildTarget,
    pub output_dir: Option<PathBuf>,
    pub presentation: HatchPresentation,
}

impl HatchRequest {
    pub(crate) fn new(
        project_name: String,
        file_contents: String,
        mode: HatchMode,
        force_overwrite: bool,
        keep_project: bool,
        build_target: BuildTarget,
        output_dir: Option<PathBuf>,
        presentation: HatchPresentation,
    ) -> Self {
        Self {
            project_name,
            file_contents,
            mode,
            force_overwrite,
            keep_project,
            build_target,
            output_dir,
            presentation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TemplateSummaryStatus {
    Created,
    Reused,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkspaceSummaryStatus {
    Removed,
    Preserved,
    CleanupFailed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HatchRunSummary {
    project_name: String,
    mode: HatchMode,
    source: HatchSource,
    build_target: String,
    binary_path: Option<PathBuf>,
    template_status: TemplateSummaryStatus,
    workspace_status: WorkspaceSummaryStatus,
}

pub(crate) fn print_hatch_start(project_name: &str, mode: HatchMode) {
    println!(
        "{} agent `{}`...",
        match mode {
            HatchMode::Build => "Hatching",
            HatchMode::Check => "Checking",
        },
        project_name
    );
    println!();
}

pub(crate) fn print_hatch_progress(step: HatchProgressStep) {
    let message = match step {
        HatchProgressStep::PreparingDefinition => "Preparing definition",
        HatchProgressStep::PreparingBuildTemplate => "Preparing build template",
        HatchProgressStep::BuildingLocalAgent => "Building local agent",
        HatchProgressStep::CheckingLocalAgent => "Checking local agent",
        HatchProgressStep::ExportingBinary => "Exporting binary",
        HatchProgressStep::CleaningWorkspace => "Cleaning workspace",
        HatchProgressStep::FinalizingWorkspace => "Finalizing workspace",
    };

    println!("{message}");
}

/// Parses `--output-dir` into an optional directory path.
pub(crate) fn resolve_output_dir(raw_output_dir: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(raw_output_dir) = raw_output_dir else {
        return Ok(None);
    };

    let trimmed = raw_output_dir.trim();
    if trimmed.is_empty() {
        return Err("Output directory cannot be empty. Provide --output-dir <DIR>.".to_string());
    }

    Ok(Some(PathBuf::from(trimmed)))
}

/// Runs the hatch execution pipeline for a single agent definition.
pub(crate) fn run_hatch_pipeline(request: HatchRequest) -> bool {
    run_hatch_pipeline_with_lock(request, crate::agent_builder::lock::try_acquire_agent_lock)
}

fn run_hatch_pipeline_with_lock<F>(request: HatchRequest, acquire_lock: F) -> bool
where
    F: FnOnce(&str) -> std::io::Result<crate::agent_builder::lock::AgentLockGuard>,
{
    let HatchRequest {
        project_name,
        file_contents,
        mode,
        force_overwrite,
        keep_project,
        build_target,
        output_dir,
        presentation,
    } = request;

    let _agent_lock = match acquire_lock(project_name.as_str()) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            println!(
                "x Agent '{}' is already running a hatch/check operation in another process.",
                project_name
            );
            return false;
        }
        Err(error) => {
            println!(
                "x Failed to acquire lock for agent '{}' ({}): {}",
                project_name,
                crate::agent_builder::agent_workspace_path(project_name.as_str()).display(),
                error
            );
            return false;
        }
    };

    if let Err(message) = prepare_workspace_for_hatch(
        project_name.as_str(),
        force_overwrite,
        |agent_name| crate::agent_builder::agent_workspace_path(agent_name).exists(),
        crate::agent_builder::cleanup::delete_agent_workspace,
    ) {
        println!("x {message}");
        return false;
    }

    let profile = mode.compile_profile();
    let mut template_preparation_started = false;
    let warmed_template =
        match crate::agent_builder::template_cache::ensure_warmed_template_with_prepare_hook(
            &build_target,
            profile,
            || {
                template_preparation_started = true;
                println!("First run may take longer while the build template is prepared.");
                print_hatch_progress(HatchProgressStep::PreparingBuildTemplate);
            },
        ) {
            Ok(template) => template,
            Err(error) => {
                println!("x Failed to prepare warmed template: {error}");
                return false;
            }
        };

    if !template_preparation_started {
        print_hatch_progress(HatchProgressStep::PreparingBuildTemplate);
    }

    let template_status = if warmed_template.created {
        TemplateSummaryStatus::Created
    } else {
        TemplateSummaryStatus::Reused
    };

    match crate::agent_builder::project::create_new_agent_project(
        &warmed_template.path,
        project_name.as_str(),
        Ok(file_contents),
    ) {
        Ok(_) => {}
        Err(error) => {
            println!("x Failed to create project: {error}");
            print_hatch_progress(workspace_progress_step(keep_project));
            if let WorkspaceSummaryStatus::CleanupFailed(error) =
                finalize_workspace(project_name.as_str(), keep_project)
            {
                println!("! Failed to clean up workspace: {error}");
            }
            return false;
        }
    }

    let shared_target_dir = warmed_template.path.join("target");
    let mut binary_path = None;

    match mode {
        HatchMode::Build => {
            print_hatch_progress(HatchProgressStep::BuildingLocalAgent);
            match crate::agent_builder::build::build_agent_project(
                project_name.as_str(),
                &build_target,
                Some(shared_target_dir.as_path()),
            ) {
                Ok(_) => {}
                Err(error) => {
                    println!("x Build failed: {error}");
                    print_hatch_progress(workspace_progress_step(keep_project));
                    if let WorkspaceSummaryStatus::CleanupFailed(error) =
                        finalize_workspace(project_name.as_str(), keep_project)
                    {
                        println!("! Failed to clean up workspace: {error}");
                    }
                    return false;
                }
            }

            print_hatch_progress(HatchProgressStep::ExportingBinary);
            match crate::agent_builder::export::export_binary(
                project_name.as_str(),
                force_overwrite,
                &build_target,
                output_dir.as_deref(),
                Some(warmed_template.path.as_path()),
            ) {
                Ok(path) => binary_path = Some(path),
                Err(error) => {
                    println!("x Export failed: {error}");
                    print_hatch_progress(workspace_progress_step(keep_project));
                    if let WorkspaceSummaryStatus::CleanupFailed(error) =
                        finalize_workspace(project_name.as_str(), keep_project)
                    {
                        println!("! Failed to clean up workspace: {error}");
                    }
                    return false;
                }
            }
        }
        HatchMode::Check => {
            print_hatch_progress(HatchProgressStep::CheckingLocalAgent);
            match crate::agent_builder::build::check_agent_project(
                project_name.as_str(),
                &build_target,
                Some(shared_target_dir.as_path()),
            ) {
                Ok(_) => {}
                Err(error) => {
                    println!("x Check failed: {error}");
                    print_hatch_progress(workspace_progress_step(keep_project));
                    if let WorkspaceSummaryStatus::CleanupFailed(error) =
                        finalize_workspace(project_name.as_str(), keep_project)
                    {
                        println!("! Failed to clean up workspace: {error}");
                    }
                    return false;
                }
            }
        }
    }

    print_hatch_progress(workspace_progress_step(keep_project));
    let workspace_status = finalize_workspace(project_name.as_str(), keep_project);
    if let WorkspaceSummaryStatus::CleanupFailed(error) = &workspace_status {
        println!("! Failed to clean up workspace: {error}");
    }

    let summary = HatchRunSummary {
        project_name,
        mode,
        source: presentation.source,
        build_target: build_target.cache_key_target().to_string(),
        binary_path,
        template_status,
        workspace_status,
    };
    for line in render_hatch_success_lines(&summary) {
        println!("{line}");
    }

    true
}

fn workspace_progress_step(keep_project: bool) -> HatchProgressStep {
    if keep_project {
        HatchProgressStep::FinalizingWorkspace
    } else {
        HatchProgressStep::CleaningWorkspace
    }
}

fn finalize_workspace(new_project_name: &str, keep_project: bool) -> WorkspaceSummaryStatus {
    if keep_project {
        return WorkspaceSummaryStatus::Preserved;
    }

    match crate::agent_builder::cleanup::delete_agent_workspace(new_project_name) {
        Ok(_) => WorkspaceSummaryStatus::Removed,
        Err(error) => WorkspaceSummaryStatus::CleanupFailed(error.to_string()),
    }
}

fn prepare_workspace_for_hatch<FExists, FDelete>(
    new_project_name: &str,
    force_overwrite: bool,
    workspace_exists: FExists,
    delete_workspace: FDelete,
) -> Result<(), String>
where
    FExists: FnOnce(&str) -> bool,
    FDelete: FnOnce(&str) -> std::io::Result<()>,
{
    let workspace_path = crate::agent_builder::agent_workspace_path(new_project_name);
    if !workspace_exists(new_project_name) {
        return Ok(());
    }

    if !force_overwrite {
        return Err(format!(
            "Agent project already exists:\n{}\n\nRe-run with --force to replace it, or choose a different local agent name.",
            workspace_path.display()
        ));
    }

    delete_workspace(new_project_name).map_err(|error| {
        format!(
            "Failed to replace existing agent workspace '{}': {error}",
            workspace_path.display()
        )
    })?;

    Ok(())
}

fn render_hatch_success_lines(summary: &HatchRunSummary) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(String::new());
    lines.push(format!(
        "✓ {}",
        match summary.mode {
            HatchMode::Build => "Agent hatched",
            HatchMode::Check => "Agent checked",
        }
    ));
    lines.push(summary_line(summary));

    let source_items = source_items(summary);
    push_aligned_section(&mut lines, "Source", &source_items);

    if let Some(binary_path) = &summary.binary_path {
        let output_items = vec![
            (
                "Binary",
                format!("`{}`", display_path(binary_path.as_path())),
            ),
            ("Target", summary.build_target.clone()),
        ];
        push_aligned_section(&mut lines, "Output", &output_items);
    }

    let mut build_items = vec![
        (
            "Cargo profile",
            summary.mode.compile_profile().name().to_string(),
        ),
        (
            "Template",
            template_status_label(summary.template_status).to_string(),
        ),
        (
            "Workspace",
            workspace_status_label(&summary.workspace_status),
        ),
    ];
    if summary.binary_path.is_none() {
        build_items.insert(0, ("Target", summary.build_target.clone()));
    }
    push_aligned_section(&mut lines, "Build", &build_items);

    if let Some(binary_path) = &summary.binary_path {
        let next_step_items = vec![(
            "Run agent",
            format!("`{} --help`", display_path(binary_path.as_path())),
        )];
        push_aligned_section(&mut lines, "Next steps", &next_step_items);
    }

    lines
}

fn summary_line(summary: &HatchRunSummary) -> String {
    match (&summary.mode, &summary.source) {
        (HatchMode::Build, HatchSource::Registry { .. }) => {
            format!("Built local agent `{}`.", summary.project_name)
        }
        (HatchMode::Build, HatchSource::LocalFile { .. }) => {
            format!(
                "Built local agent `{}` from local definition.",
                summary.project_name
            )
        }
        (HatchMode::Build, HatchSource::InlineJson) => {
            format!(
                "Built local agent `{}` from inline JSON definition.",
                summary.project_name
            )
        }
        (HatchMode::Build, HatchSource::StdinJson) => {
            format!(
                "Built local agent `{}` from stdin definition.",
                summary.project_name
            )
        }
        (HatchMode::Build, HatchSource::Account { agent, .. }) => format!(
            "Built local agent `{}` from account definition `{}`.",
            summary.project_name, agent
        ),
        (HatchMode::Check, HatchSource::Registry { .. }) => {
            format!("Checked local agent `{}`.", summary.project_name)
        }
        (HatchMode::Check, HatchSource::LocalFile { .. }) => {
            format!(
                "Checked local agent `{}` from local definition.",
                summary.project_name
            )
        }
        (HatchMode::Check, HatchSource::InlineJson) => {
            format!(
                "Checked local agent `{}` from inline JSON definition.",
                summary.project_name
            )
        }
        (HatchMode::Check, HatchSource::StdinJson) => {
            format!(
                "Checked local agent `{}` from stdin definition.",
                summary.project_name
            )
        }
        (HatchMode::Check, HatchSource::Account { agent, .. }) => format!(
            "Checked local agent `{}` from account definition `{}`.",
            summary.project_name, agent
        ),
    }
}

fn source_items(summary: &HatchRunSummary) -> Vec<(&'static str, String)> {
    match &summary.source {
        HatchSource::Registry { name } => {
            vec![("Type", "Registry".to_string()), ("Name", name.clone())]
        }
        HatchSource::LocalFile { path } => vec![
            ("Type", "Local file".to_string()),
            ("File", path.clone()),
            ("Agent", summary.project_name.clone()),
        ],
        HatchSource::InlineJson => vec![
            ("Type", "Inline JSON".to_string()),
            ("Agent", summary.project_name.clone()),
        ],
        HatchSource::StdinJson => vec![
            ("Type", "stdin".to_string()),
            ("Agent", summary.project_name.clone()),
        ],
        HatchSource::Account { owner, agent, path } => vec![
            ("Type", "Account".to_string()),
            ("Owner", owner.clone()),
            ("Agent", agent.clone()),
            ("Path", path.clone()),
        ],
    }
}

fn template_status_label(status: TemplateSummaryStatus) -> &'static str {
    match status {
        TemplateSummaryStatus::Created => "Created warmed template",
        TemplateSummaryStatus::Reused => "Reused warmed template",
    }
}

fn workspace_status_label(status: &WorkspaceSummaryStatus) -> String {
    match status {
        WorkspaceSummaryStatus::Removed => "Removed".to_string(),
        WorkspaceSummaryStatus::Preserved => "Preserved".to_string(),
        WorkspaceSummaryStatus::CleanupFailed(_) => "Cleanup failed".to_string(),
    }
}

fn push_aligned_section(lines: &mut Vec<String>, title: &str, items: &[(&str, String)]) {
    if items.is_empty() {
        return;
    }

    lines.push(String::new());
    lines.push(title.to_string());

    let label_width = items
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0);
    for (label, value) in items {
        lines.push(format!("  {label:<width$}  {value}", width = label_width));
    }
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

/// Reads a local agent config file as UTF-8 text.
pub(crate) fn read_local_config(path: &str) -> Result<String, std::io::Error> {
    fs::read_to_string(path)
}

/// Fetches a named agent config template from the Cargo-AI public registry.
pub(crate) async fn fetch_from_registry(name: &str) -> Result<String, Error> {
    fetch_from_registry_at_url(name, "https://api.cargo-ai.org/public").await
}

async fn fetch_from_registry_at_url(name: &str, url: &str) -> Result<String, Error> {
    let client = reqwest::Client::new();

    let body = serde_json::json!({ "request": name });

    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::new(ErrorKind::Other, format!("network error: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::new(
            ErrorKind::Other,
            format!("HTTP {} for {url}", resp.status()),
        ));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| Error::new(ErrorKind::Other, e.to_string()))?;

    // If the registry returns a JSON object with an `error` field,
    // surface it cleanly instead of passing through opaque text.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(err_msg) = value.get("error").and_then(|v| v.as_str()) {
            return Err(Error::new(ErrorKind::Other, err_msg.to_string()));
        }

        // If registry wraps payload in `{ "response": "<json string>" }`, unwrap it.
        if let Some(response) = value.get("response").and_then(|v| v.as_str()) {
            return Ok(response.to_string());
        }
    }

    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::{
        display_path, fetch_from_registry_at_url, prepare_workspace_for_hatch,
        render_hatch_success_lines, resolve_output_dir, run_hatch_pipeline_with_lock, HatchMode,
        HatchPresentation, HatchRequest, HatchRunSummary, HatchSource, TemplateSummaryStatus,
        WorkspaceSummaryStatus,
    };
    use crate::agent_builder::build_target::BuildTarget;
    use mockito::Matcher;
    use std::cell::Cell;
    use std::io;
    use std::path::PathBuf;

    #[tokio::test]
    async fn registry_fetch_completes_inside_async_runtime() {
        let mut server = mockito::Server::new_async().await;
        let registry = server
            .mock("POST", "/public")
            .match_header("content-type", "application/json")
            .match_body(Matcher::PartialJson(serde_json::json!({
                "request": "adder_test"
            })))
            .with_status(200)
            .with_body(r#"{"response":"{\"agent_definition_schema_version\":\"2026-03-03.r1\"}"}"#)
            .create_async()
            .await;

        let registry_url = format!("{}/public", server.url());
        let definition = fetch_from_registry_at_url("adder_test", registry_url.as_str())
            .await
            .expect("async registry fetch should succeed");

        registry.assert_async().await;
        assert_eq!(
            definition,
            r#"{"agent_definition_schema_version":"2026-03-03.r1"}"#
        );
    }

    #[tokio::test]
    async fn registry_fetch_returns_http_failure_without_runtime_panic() {
        let mut server = mockito::Server::new_async().await;
        let registry = server
            .mock("POST", "/public")
            .match_body(Matcher::PartialJson(serde_json::json!({
                "request": "missing_agent"
            })))
            .with_status(404)
            .create_async()
            .await;

        let registry_url = format!("{}/public", server.url());
        let error = fetch_from_registry_at_url("missing_agent", registry_url.as_str())
            .await
            .expect_err("HTTP failures should remain errors");

        registry.assert_async().await;
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("HTTP 404"));
    }

    #[test]
    fn lock_conflict_fails_fast_before_project_mutation() {
        let result = run_hatch_pipeline_with_lock(
            HatchRequest::new(
                "agent_lock_conflict_test".to_string(),
                r#"{"agent_definition_schema_version":"2026-03-03.r1"}"#.to_string(),
                HatchMode::Check,
                false,
                false,
                BuildTarget::from_cli(None).expect("default target should resolve"),
                None,
                HatchPresentation {
                    source: HatchSource::Registry {
                        name: "agent_lock_conflict_test".to_string(),
                    },
                },
            ),
            |_| {
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "lock already exists",
                ))
            },
        );

        assert!(!result);
    }

    #[test]
    fn existing_workspace_requires_force() {
        let err = prepare_workspace_for_hatch("weather_test", false, |_| true, |_| Ok(()))
            .expect_err("existing workspace without force should fail");
        assert!(err.contains("Agent project already exists"));
        assert!(err.contains("--force"));
    }

    #[test]
    fn force_replaces_existing_workspace_before_build() {
        let deleted = Cell::new(false);
        prepare_workspace_for_hatch(
            "weather_test",
            true,
            |_| true,
            |_| {
                deleted.set(true);
                Ok(())
            },
        )
        .expect("force replacement should succeed");

        assert!(deleted.get());
    }

    #[test]
    fn output_dir_rejects_empty_value() {
        let err = resolve_output_dir(Some("   ")).expect_err("empty output dir should fail");
        assert!(err.contains("--output-dir"));
    }

    #[test]
    fn output_dir_accepts_non_empty_value() {
        let output_dir =
            resolve_output_dir(Some("./dist")).expect("non-empty output dir should parse");
        assert_eq!(output_dir, Some(PathBuf::from("./dist")));
    }

    #[test]
    fn renders_registry_build_summary_lines() {
        let lines = render_hatch_success_lines(&HatchRunSummary {
            project_name: "weather_test".to_string(),
            mode: HatchMode::Build,
            source: HatchSource::Registry {
                name: "weather_test".to_string(),
            },
            build_target: "aarch64-apple-darwin".to_string(),
            binary_path: Some(PathBuf::from("./weather_test")),
            template_status: TemplateSummaryStatus::Reused,
            workspace_status: WorkspaceSummaryStatus::Removed,
        });

        let rendered = lines.join("\n");
        assert!(rendered.contains("✓ Agent hatched"));
        assert!(rendered.contains("Cargo profile  release"));
        assert!(rendered.contains("Built local agent `weather_test`."));
        assert!(rendered.contains("Type  Registry"));
        assert!(rendered.contains("Name  weather_test"));
        assert!(rendered.contains("Binary  `./weather_test`"));
        assert!(rendered.contains("Target  aarch64-apple-darwin"));
        assert!(rendered.contains("Template       Reused warmed template"));
        assert!(rendered.contains("Workspace      Removed"));
        assert!(rendered.contains("Run agent  `./weather_test --help`"));
    }

    #[test]
    fn renders_account_check_summary_without_output_section() {
        let lines = render_hatch_success_lines(&HatchRunSummary {
            project_name: "weather_local".to_string(),
            mode: HatchMode::Check,
            source: HatchSource::Account {
                owner: "self".to_string(),
                agent: "weather_remote".to_string(),
                path: "/".to_string(),
            },
            build_target: "aarch64-apple-darwin".to_string(),
            binary_path: None,
            template_status: TemplateSummaryStatus::Created,
            workspace_status: WorkspaceSummaryStatus::Preserved,
        });

        let rendered = lines.join("\n");
        assert!(rendered.contains("✓ Agent checked"));
        assert!(rendered.contains("Cargo profile  dev"));
        assert!(rendered.contains(
            "Checked local agent `weather_local` from account definition `weather_remote`."
        ));
        assert!(rendered.contains("Owner  self"));
        assert!(rendered.contains("Agent  weather_remote"));
        assert!(rendered.contains("Path   /"));
        assert!(rendered.contains("Target         aarch64-apple-darwin"));
        assert!(rendered.contains("Template       Created warmed template"));
        assert!(rendered.contains("Workspace      Preserved"));
        assert!(!rendered.contains("\nOutput\n"));
        assert!(!rendered.contains("\nNext steps\n"));
    }

    #[test]
    fn renders_inline_json_build_summary_lines() {
        let lines = render_hatch_success_lines(&HatchRunSummary {
            project_name: "weather_test".to_string(),
            mode: HatchMode::Build,
            source: HatchSource::InlineJson,
            build_target: "aarch64-apple-darwin".to_string(),
            binary_path: Some(PathBuf::from("./weather_test")),
            template_status: TemplateSummaryStatus::Reused,
            workspace_status: WorkspaceSummaryStatus::Removed,
        });

        let rendered = lines.join("\n");
        assert!(rendered.contains("Built local agent `weather_test` from inline JSON definition."));
        assert!(rendered.contains("Type   Inline JSON"));
        assert!(rendered.contains("Agent  weather_test"));
    }

    #[test]
    fn displays_absolute_current_dir_children_as_dot_relative_paths() {
        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let rendered = display_path(current_dir.join("weather_test").as_path());
        assert_eq!(rendered, "./weather_test");
    }
}
