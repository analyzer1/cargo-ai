//! Tool lifecycle and runtime support for Cargo AI-managed companion binaries.
use crate::agent_builder::build_target::CargoCompileProfile;
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

const PROJECT_METADATA_RELATIVE_PATH: &str = ".cargo-ai/project.toml";
const PROJECT_TOOLS_RELATIVE_PATH: &str = ".cargo-ai/tools";
const TOOL_MANIFEST_FILE_NAME: &str = "tool.json";
const TOOL_SCAFFOLD_CARGO_TOML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/tool-scaffold/Cargo.toml.tmpl"
));
const TOOL_SCAFFOLD_MAIN_RS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/tool-scaffold/src/main.rs.tmpl"
));
const TOOL_SCAFFOLD_LIB_RS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/tool-scaffold/src/lib.rs.tmpl"
));
const TOOL_SCAFFOLD_AGENT_BRIDGE_RS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/tool-scaffold/src/agent_bridge.rs.tmpl"
));
const TOOL_SCAFFOLD_TOOL_RS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/tool-scaffold/src/tool.rs.tmpl"
));
const TOOL_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToolScope {
    Bundled,
    Project,
    Machine,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ToolManifestBinary {
    #[serde(default)]
    default_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolManifestSource {
    manifest_path: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolManifestDistribution {
    #[allow(dead_code)]
    channel: Option<String>,
    #[allow(dead_code)]
    package_id: Option<String>,
    #[allow(dead_code)]
    package_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolManifestArtifact {
    path: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolManifest {
    schema_version: u32,
    tool_id: String,
    #[serde(default)]
    source: Option<ToolManifestSource>,
    #[serde(default)]
    binary: ToolManifestBinary,
    #[serde(default)]
    artifacts: BTreeMap<String, ToolManifestArtifact>,
    #[allow(dead_code)]
    #[serde(default)]
    distribution: Option<ToolManifestDistribution>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ProjectMetadataDocument {
    #[serde(default)]
    tools: Option<ProjectToolsPolicyDocument>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ProjectToolsPolicyDocument {
    #[serde(default)]
    allow_global_fallback: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ToolDescribeParam {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) required: bool,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) default: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ToolDescribeResult {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) nullable: bool,
    #[serde(default)]
    pub(crate) description: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ToolDescribeResourceProfile {
    pub(crate) network: String,
    pub(crate) filesystem_read: String,
    pub(crate) filesystem_write: String,
    pub(crate) subprocess: String,
    pub(crate) env_read: String,
    pub(crate) credential_access: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ToolDescribeSelfTest {
    pub(crate) supported: bool,
    pub(crate) safe: bool,
    #[serde(default)]
    pub(crate) description: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ToolDescribeExamples {
    pub(crate) minimal_invoke: Value,
    pub(crate) full_invoke: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ToolDescribeDocument {
    pub(crate) protocol_version: u32,
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) params: BTreeMap<String, ToolDescribeParam>,
    pub(crate) result: ToolDescribeResult,
    pub(crate) resource_profile: ToolDescribeResourceProfile,
    pub(crate) self_test: ToolDescribeSelfTest,
    pub(crate) examples: ToolDescribeExamples,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolInvokeResponse {
    protocol_version: u32,
    result: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedTool {
    pub(crate) tool_id: String,
    pub(crate) scope: ToolScope,
    #[allow(dead_code)]
    manifest_path: PathBuf,
    pub(crate) binary_name: String,
    pub(crate) target_triple: String,
    pub(crate) binary_path: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolContract {
    pub(crate) resolved: ResolvedTool,
    pub(crate) describe: ToolDescribeDocument,
}

#[derive(Clone, Debug, Default)]
struct ToolLintReport {
    tool_id: String,
    notes: Vec<String>,
    errors: Vec<String>,
}

#[derive(Clone, Debug)]
struct ProjectSourceToolContext {
    tool_id: String,
    tool_manifest_path: PathBuf,
    source_manifest_relative_path: String,
    source_manifest_path: PathBuf,
    source_root: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolResolver {
    bundled_root: Option<PathBuf>,
    project_root: Option<PathBuf>,
    target_triple: String,
    resolution_mode: ToolResolutionMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolResolutionMode {
    Standard,
    InstalledPackageRuntimeOnly,
}

impl ToolResolver {
    pub(crate) fn new(project_root: Option<PathBuf>, target_triple: impl Into<String>) -> Self {
        Self {
            bundled_root: None,
            project_root,
            target_triple: target_triple.into(),
            resolution_mode: ToolResolutionMode::Standard,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_bundled_root(mut self, bundled_root: Option<PathBuf>) -> Self {
        self.bundled_root = bundled_root;
        self
    }

    pub(crate) fn with_installed_package_runtime(mut self, bundled_root: Option<PathBuf>) -> Self {
        self.bundled_root = bundled_root;
        self.resolution_mode = ToolResolutionMode::InstalledPackageRuntimeOnly;
        self
    }

    pub(crate) fn resolve_contract(&self, tool_id: &str) -> Result<ToolContract, String> {
        let resolved = self.resolve_tool(tool_id)?;
        let describe = load_tool_describe_document(&resolved)?;
        validate_describe_document(&describe, &resolved)?;
        Ok(ToolContract { resolved, describe })
    }

    pub(crate) fn resolve_tool(&self, tool_id: &str) -> Result<ResolvedTool, String> {
        validate_tool_identifier(tool_id)?;
        let mut allow_machine_fallback = self.project_root.is_none();

        if let Some(bundled_root) = self.bundled_root.as_ref() {
            if let Some(resolved) = resolve_tool_from_scope_root(
                bundled_root,
                ToolScope::Bundled,
                tool_id,
                &self.target_triple,
            )? {
                return Ok(resolved);
            }
        }

        if self.resolution_mode == ToolResolutionMode::InstalledPackageRuntimeOnly {
            return Err(format!(
                "Tool '{}' was not found in the installed package's version-bound runtime tools.",
                tool_id
            ));
        }

        if let Some(project_root) = self.project_root.as_ref() {
            allow_machine_fallback = project_allows_global_fallback(project_root)?;
            let project_tools_root = project_tools_root(project_root);
            match resolve_tool_from_scope_root(
                &project_tools_root,
                ToolScope::Project,
                tool_id,
                &self.target_triple,
            ) {
                Ok(Some(resolved)) => return Ok(resolved),
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }

        if !allow_machine_fallback {
            return Err(format!(
                "Tool '{}' was not found in the current project, and project tool policy disallows Cargo AI Home fallback.",
                tool_id
            ));
        }

        let machine_tools_root = machine_tools_root();
        resolve_tool_from_scope_root(
            &machine_tools_root,
            ToolScope::Machine,
            tool_id,
            &self.target_triple,
        )?
        .ok_or_else(|| {
            format!(
                "Tool '{}' was not found in the current project or Cargo AI Home.",
                tool_id
            )
        })
    }
}

fn project_allows_global_fallback(project_root: &Path) -> Result<bool, String> {
    let metadata_path = project_root.join(PROJECT_METADATA_RELATIVE_PATH);
    let contents = fs::read_to_string(&metadata_path).map_err(|error| {
        format!(
            "Failed to read project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    let metadata: ProjectMetadataDocument = toml::from_str(&contents).map_err(|error| {
        format!(
            "Failed to parse project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    Ok(metadata
        .tools
        .and_then(|tools| tools.allow_global_fallback)
        .unwrap_or(false))
}

fn validate_project_relative_path(raw_path: &str, label: &str) -> Result<(), String> {
    if raw_path.trim().is_empty() {
        return Err(format!("{label} path must be a non-empty relative path."));
    }
    let candidate = Path::new(raw_path);
    if candidate.is_absolute() {
        return Err(format!(
            "{label} path must be relative and stay at the current level or below."
        ));
    }
    if path_uses_parent_traversal(candidate) {
        return Err(format!(
            "{label} path must stay at the current level or below; parent traversal (`..`) is not allowed."
        ));
    }
    Ok(())
}

fn path_uses_parent_traversal(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

pub(crate) fn maybe_find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };

    loop {
        if current.join(PROJECT_METADATA_RELATIVE_PATH).exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

#[allow(dead_code)]
pub(crate) fn bundled_tools_root_from_executable() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .map(|path| path.join(PROJECT_TOOLS_RELATIVE_PATH))
        .filter(|path| path.exists())
}

pub(crate) fn audit_actions_for_tools(
    actions: &[crate::Action],
    resolver: &ToolResolver,
    current_platform: Option<&str>,
) -> Result<Vec<ResolvedTool>, String> {
    let mut contracts = BTreeMap::<String, ToolContract>::new();

    for (action_index, action) in actions.iter().enumerate() {
        for (step_index, step) in action.run.iter().enumerate() {
            if !step_matches_platform(step.platforms.as_deref(), current_platform) {
                continue;
            }
            if !step.kind.eq_ignore_ascii_case("tool") {
                continue;
            }

            let tool_name = step.tool_name.as_deref().ok_or_else(|| {
                format!(
                    "Action '{}' tool step {} is missing required `name`.",
                    action.name,
                    step_index + 1
                )
            })?;
            let contract = if let Some(existing) = contracts.get(tool_name) {
                existing.clone()
            } else {
                let resolved = resolver.resolve_contract(tool_name)?;
                contracts.insert(tool_name.to_string(), resolved.clone());
                resolved
            };

            validate_tool_step_against_contract(
                step,
                &contract.describe,
                Some(action.name.as_str()),
                Some(step_index),
            )?;
            if contract.describe.name != tool_name
                && contract.describe.name != contract.resolved.binary_name
            {
                return Err(format!(
                    "Tool '{}' describe contract reported name '{}', which does not match the referenced tool id.",
                    tool_name, contract.describe.name
                ));
            }
            if action_index == usize::MAX {
                unreachable!();
            }
        }
    }

    Ok(contracts
        .into_values()
        .map(|contract| contract.resolved)
        .collect())
}

pub(crate) fn actions_have_applicable_tool_steps(
    actions: &[crate::Action],
    current_platform: Option<&str>,
) -> bool {
    actions.iter().any(|action| {
        action.run.iter().any(|step| {
            step.kind.eq_ignore_ascii_case("tool")
                && step_matches_platform(step.platforms.as_deref(), current_platform)
        })
    })
}

pub(crate) fn validate_tool_step_against_contract(
    step: &crate::RunStep,
    describe: &ToolDescribeDocument,
    action_name: Option<&str>,
    step_index: Option<usize>,
) -> Result<(), String> {
    let context = tool_step_context(action_name, step_index);
    let provided = &step.tool_params;

    for (name, value) in provided {
        let Some(param_spec) = describe.params.get(name) else {
            return Err(format!(
                "{context} references unknown tool param '{}'.",
                name
            ));
        };

        if let crate::ToolParamValue::Literal(literal) = value {
            if !json_value_matches_declared_type(literal, param_spec.kind.as_str()) {
                return Err(format!(
                    "{context} param '{}' must be {}, but the literal value is incompatible.",
                    name,
                    display_type_name(param_spec.kind.as_str())
                ));
            }
        }
    }

    for (name, param_spec) in &describe.params {
        if param_spec.required && param_spec.default.is_none() && !provided.contains_key(name) {
            return Err(format!(
                "{context} is missing required tool param '{}'.",
                name
            ));
        }
    }

    Ok(())
}

pub(crate) fn resolve_tool_invoke_params(
    step: &crate::RunStep,
    data: &Value,
    action_name: &str,
    describe: &ToolDescribeDocument,
) -> Result<Map<String, Value>, String> {
    let mut params = Map::new();
    for (name, value) in &step.tool_params {
        let resolved = match value {
            crate::ToolParamValue::Literal(literal) => literal.clone(),
            crate::ToolParamValue::Variable(variable) => {
                let Some(resolved) = lookup_action_variable(data, variable) else {
                    return Err(format!(
                        "Action '{}' tool param '{}' references missing variable '{}'.",
                        action_name, name, variable
                    ));
                };
                resolved.clone()
            }
        };

        let expected = describe.params.get(name).ok_or_else(|| {
            format!(
                "Action '{}' references unknown tool param '{}'.",
                action_name, name
            )
        })?;
        if !json_value_matches_declared_type(&resolved, expected.kind.as_str()) {
            return Err(format!(
                "Action '{}' tool param '{}' resolved to {}, but the tool expects {}.",
                action_name,
                name,
                describe_runtime_value(&resolved),
                display_type_name(expected.kind.as_str())
            ));
        }

        params.insert(name.clone(), resolved);
    }

    for (name, param_spec) in &describe.params {
        if param_spec.required && param_spec.default.is_none() && !params.contains_key(name) {
            return Err(format!(
                "Action '{}' tool step is missing required param '{}'.",
                action_name, name
            ));
        }
    }

    Ok(params)
}

pub(crate) fn project_tools_root(project_root: &Path) -> PathBuf {
    project_root.join(PROJECT_TOOLS_RELATIVE_PATH)
}

pub(crate) fn machine_tools_root() -> PathBuf {
    crate::config::paths::cargo_ai_root().join("tools")
}

pub(crate) fn scaffold_local_tool(project_root: &Path, tool_name: &str) -> Result<(), String> {
    validate_local_tool_name(tool_name)?;
    if !project_root.join(PROJECT_METADATA_RELATIVE_PATH).exists() {
        return Err(format!(
            "No Cargo AI project metadata found at '{}'. Run `cargo ai init` first.",
            project_root.join(PROJECT_METADATA_RELATIVE_PATH).display()
        ));
    }

    let source_dir = project_root.join("tools").join(tool_name);
    let tool_dir = project_tools_root(project_root).join(tool_name);
    let cargo_toml_path = source_dir.join("Cargo.toml");
    let main_rs_path = source_dir.join("src").join("main.rs");
    let lib_rs_path = source_dir.join("src").join("lib.rs");
    let agent_bridge_rs_path = source_dir.join("src").join("agent_bridge.rs");
    let tool_rs_path = source_dir.join("src").join("tool.rs");
    let manifest_path = tool_dir.join(TOOL_MANIFEST_FILE_NAME);

    for path in [
        &cargo_toml_path,
        &main_rs_path,
        &lib_rs_path,
        &agent_bridge_rs_path,
        &tool_rs_path,
        &manifest_path,
    ] {
        if path.exists() {
            return Err(format!(
                "Tool scaffold would overwrite existing managed file '{}'.",
                path.display()
            ));
        }
    }

    fs::create_dir_all(source_dir.join("src")).map_err(|error| {
        format!(
            "Failed to create tool source directory '{}': {}",
            source_dir.display(),
            error
        )
    })?;
    fs::create_dir_all(&tool_dir).map_err(|error| {
        format!(
            "Failed to create tool metadata directory '{}': {}",
            tool_dir.display(),
            error
        )
    })?;

    let module_name = tool_name.replace('-', "_");
    write_utf8_file(
        &cargo_toml_path,
        TOOL_SCAFFOLD_CARGO_TOML.replace("__TOOL_NAME__", tool_name),
    )?;
    write_utf8_file(
        &main_rs_path,
        TOOL_SCAFFOLD_MAIN_RS.replace("__TOOL_MODULE__", module_name.as_str()),
    )?;
    write_utf8_file(
        &lib_rs_path,
        TOOL_SCAFFOLD_LIB_RS.replace("__TOOL_NAME__", tool_name),
    )?;
    write_utf8_file(
        &agent_bridge_rs_path,
        TOOL_SCAFFOLD_AGENT_BRIDGE_RS.to_string(),
    )?;
    write_utf8_file(
        &tool_rs_path,
        TOOL_SCAFFOLD_TOOL_RS.replace("__TOOL_NAME__", tool_name),
    )?;
    write_utf8_file(
        &manifest_path,
        render_source_tool_manifest_json(
            tool_name,
            format!("tools/{tool_name}/Cargo.toml").as_str(),
            tool_name,
        ),
    )?;

    Ok(())
}

pub(crate) fn build_source_tool(
    tool_name: &str,
    build_target: &crate::agent_builder::build_target::BuildTarget,
    scope: ToolScope,
    project_root: &Path,
    profile: CargoCompileProfile,
) -> Result<ResolvedTool, String> {
    if scope == ToolScope::Bundled {
        return Err("Bundled scope is not supported for `cargo ai tools build`.".to_string());
    }

    let source_manifest_path = project_tools_root(project_root)
        .join(tool_name)
        .join(TOOL_MANIFEST_FILE_NAME);
    let source_manifest = load_tool_manifest(&source_manifest_path, tool_name)?;
    let source = source_manifest.source.as_ref().ok_or_else(|| {
        format!(
            "Tool '{}' is not source-backed and cannot be built with `cargo ai tools build`.",
            tool_name
        )
    })?;
    let manifest_path = project_root.join(source.manifest_path.as_str());
    if !manifest_path.exists() {
        return Err(format!(
            "Tool '{}' source manifest '{}' was not found.",
            tool_name,
            manifest_path.display()
        ));
    }

    let source_dir = manifest_path.parent().ok_or_else(|| {
        format!(
            "Tool '{}' manifest path has no parent directory.",
            tool_name
        )
    })?;
    let binary_name = default_binary_name_for_manifest(&source_manifest);

    let mut command = Command::new("cargo");
    command
        .args(build_target.cargo_args("build", profile))
        .arg("--manifest-path")
        .arg(&manifest_path)
        .current_dir(source_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command.status().map_err(|error| {
        format!(
            "Failed to run cargo build for tool '{}': {}",
            tool_name, error
        )
    })?;
    if !status.success() {
        return Err(format!("Cargo build failed for tool '{}'.", tool_name));
    }

    let built_binary_path =
        build_target.compiled_binary_path(source_dir, binary_name.as_str(), profile);
    if !built_binary_path.exists() {
        return Err(format!(
            "Expected built tool binary '{}' at '{}', but it was not produced.",
            binary_name,
            built_binary_path.display()
        ));
    }

    let scope_root = match scope {
        ToolScope::Project => project_tools_root(project_root),
        ToolScope::Machine => machine_tools_root(),
        ToolScope::Bundled => unreachable!(),
    };
    let tool_dir = scope_root.join(tool_name);
    let artifact_relative_path = PathBuf::from("bin")
        .join(build_target.cache_key_target())
        .join(profile.name())
        .join(&binary_name);
    let artifact_path = tool_dir.join(&artifact_relative_path);
    if let Some(parent) = artifact_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create tool artifact directory '{}': {}",
                parent.display(),
                error
            )
        })?;
    }
    fs::copy(&built_binary_path, &artifact_path).map_err(|error| {
        format!(
            "Failed to copy built tool binary into '{}': {}",
            artifact_path.display(),
            error
        )
    })?;

    let manifest_json = match scope {
        ToolScope::Project => render_project_built_tool_manifest_json(
            tool_name,
            source.manifest_path.as_str(),
            binary_name.as_str(),
            build_target.cache_key_target(),
            artifact_relative_path.to_string_lossy().as_ref(),
        ),
        ToolScope::Machine => render_binary_tool_manifest_json(
            tool_name,
            binary_name.as_str(),
            build_target.cache_key_target(),
            artifact_relative_path.to_string_lossy().as_ref(),
        ),
        ToolScope::Bundled => unreachable!(),
    };
    let manifest_output_path = tool_dir.join(TOOL_MANIFEST_FILE_NAME);
    if let Some(parent) = manifest_output_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create tool manifest directory '{}': {}",
                parent.display(),
                error
            )
        })?;
    }
    write_utf8_file(&manifest_output_path, manifest_json)?;

    ToolResolver::new(
        if scope == ToolScope::Project {
            Some(project_root.to_path_buf())
        } else {
            None
        },
        build_target.cache_key_target(),
    )
    .resolve_tool(tool_name)
}

pub(crate) fn materialize_source_tool_for_package_runtime(
    tool_name: &str,
    build_target: &crate::agent_builder::build_target::BuildTarget,
    package_root: &Path,
    runtime_tools_root: &Path,
    scratch_root: &Path,
) -> Result<ResolvedTool, String> {
    validate_tool_identifier(tool_name)?;

    let managed_manifest_relative = PathBuf::from(PROJECT_TOOLS_RELATIVE_PATH)
        .join(tool_name)
        .join(TOOL_MANIFEST_FILE_NAME);
    let managed_manifest_path = crate::commands::local_packages::resolve_existing_path_under_root(
        package_root,
        managed_manifest_relative.as_path(),
        format!("Packaged tool '{}' metadata", tool_name).as_str(),
    )?;
    if !managed_manifest_path.is_file() {
        return Err(format!(
            "Packaged tool '{}' metadata '{}' must be a regular file.",
            tool_name,
            managed_manifest_path.display()
        ));
    }
    let managed_manifest = load_tool_manifest(managed_manifest_path.as_path(), tool_name)?;
    let source = managed_manifest.source.as_ref().ok_or_else(|| {
        format!(
            "Packaged tool '{}' is not source-backed and cannot be materialized for an installed package runtime.",
            tool_name
        )
    })?;
    let source_manifest_relative =
        crate::commands::local_packages::normalize_portable_relative_path(
            source.manifest_path.as_str(),
            format!("Packaged tool '{}' source manifest", tool_name).as_str(),
        )?;
    let source_manifest_path = crate::commands::local_packages::resolve_existing_path_under_root(
        package_root,
        source_manifest_relative.as_path(),
        format!("Packaged tool '{}' source manifest", tool_name).as_str(),
    )?;
    if !source_manifest_path.is_file() {
        return Err(format!(
            "Packaged tool '{}' source manifest '{}' must be a regular file.",
            tool_name,
            source_manifest_path.display()
        ));
    }
    let source_dir = source_manifest_path.parent().ok_or_else(|| {
        format!(
            "Packaged tool '{}' source manifest '{}' has no parent directory.",
            tool_name,
            source_manifest_path.display()
        )
    })?;
    let binary_name = default_binary_name_for_manifest(&managed_manifest);

    crate::commands::local_packages::ensure_real_directory(
        scratch_root,
        "Installed package tool build scratch root",
    )?;
    let tool_scratch_root = crate::commands::local_packages::ensure_directory_path_under_root(
        scratch_root,
        Path::new(tool_name),
        format!("Installed package tool '{}' build scratch", tool_name).as_str(),
    )?;
    let cargo_target_root = tool_scratch_root.join("target");

    let mut command = Command::new("cargo");
    command
        .args(build_target.cargo_args("build", CargoCompileProfile::Release))
        .arg("--locked")
        .arg("--manifest-path")
        .arg(&source_manifest_path)
        .arg("--target-dir")
        .arg(&cargo_target_root)
        .current_dir(source_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command.status().map_err(|error| {
        format!(
            "Failed to run locked Cargo build for packaged tool '{}': {}",
            tool_name, error
        )
    })?;
    if !status.success() {
        return Err(format!(
            "Locked Cargo build failed for packaged tool '{}'.",
            tool_name
        ));
    }

    let built_binary_path = build_target.compiled_binary_path(
        &tool_scratch_root,
        binary_name.as_str(),
        CargoCompileProfile::Release,
    );
    let built_binary_relative = built_binary_path.strip_prefix(scratch_root).map_err(|_| {
        format!(
            "Built packaged tool '{}' artifact '{}' escaped its scratch root '{}'.",
            tool_name,
            built_binary_path.display(),
            scratch_root.display()
        )
    })?;
    let built_binary_path = crate::commands::local_packages::resolve_existing_path_under_root(
        scratch_root,
        built_binary_relative,
        format!("Built packaged tool '{}' artifact", tool_name).as_str(),
    )?;
    if !built_binary_path.is_file() {
        return Err(format!(
            "Expected built packaged tool '{}' artifact '{}' was not produced as a regular file.",
            tool_name,
            built_binary_path.display()
        ));
    }

    crate::commands::local_packages::ensure_real_directory(
        runtime_tools_root,
        "Installed package runtime tools root",
    )?;
    let runtime_tool_root = crate::commands::local_packages::ensure_directory_path_under_root(
        runtime_tools_root,
        Path::new(tool_name),
        format!("Installed package runtime tool '{}' root", tool_name).as_str(),
    )?;
    let artifact_parent_relative = PathBuf::from("bin")
        .join(build_target.cache_key_target())
        .join(CargoCompileProfile::Release.name());
    let artifact_relative_path =
        build_target.exported_binary_path(artifact_parent_relative.as_path(), binary_name.as_str());
    let artifact_relative_text = artifact_relative_path.to_string_lossy().replace('\\', "/");
    let artifact_relative_path = crate::commands::local_packages::normalize_portable_relative_path(
        artifact_relative_text.as_str(),
        format!("Installed package runtime tool '{}' artifact", tool_name).as_str(),
    )?;
    let artifact_parent_relative = artifact_relative_path.parent().ok_or_else(|| {
        format!(
            "Installed package runtime tool '{}' artifact path has no parent.",
            tool_name
        )
    })?;
    let artifact_parent = crate::commands::local_packages::ensure_directory_path_under_root(
        runtime_tool_root.as_path(),
        artifact_parent_relative,
        format!(
            "Installed package runtime tool '{}' artifact directory",
            tool_name
        )
        .as_str(),
    )?;
    let artifact_file_name = artifact_relative_path.file_name().ok_or_else(|| {
        format!(
            "Installed package runtime tool '{}' artifact path has no file name.",
            tool_name
        )
    })?;
    let artifact_path = artifact_parent.join(artifact_file_name);
    fs::copy(&built_binary_path, &artifact_path).map_err(|error| {
        format!(
            "Failed to copy packaged tool '{}' artifact into '{}': {}",
            tool_name,
            artifact_path.display(),
            error
        )
    })?;

    let manifest_path = runtime_tool_root.join(TOOL_MANIFEST_FILE_NAME);
    write_utf8_file(
        manifest_path.as_path(),
        render_binary_tool_manifest_json(
            tool_name,
            binary_name.as_str(),
            build_target.cache_key_target(),
            artifact_relative_text.as_str(),
        ),
    )?;

    resolve_tool_from_scope_root(
        runtime_tools_root,
        ToolScope::Bundled,
        tool_name,
        build_target.cache_key_target(),
    )?
    .ok_or_else(|| {
        format!(
            "Packaged tool '{}' was not materialized into the installed runtime tools root.",
            tool_name
        )
    })
}

pub(crate) fn validate_package_runtime_tools(
    runtime_tools_root: &Path,
    tool_names: &[String],
    target_triple: &str,
) -> Result<(), String> {
    crate::commands::local_packages::ensure_real_directory(
        runtime_tools_root,
        "Installed package runtime tools root",
    )?;
    let mut validated = BTreeMap::new();
    for tool_name in tool_names {
        if validated.insert(tool_name.as_str(), ()).is_some() {
            continue;
        }
        resolve_tool_from_scope_root(
            runtime_tools_root,
            ToolScope::Bundled,
            tool_name,
            target_triple,
        )?
        .ok_or_else(|| {
            format!(
                "Installed package runtime tool '{}' is missing for target '{}'.",
                tool_name, target_triple
            )
        })?;
    }
    Ok(())
}

fn load_project_source_tool_context(
    project_root: &Path,
    tool_name: &str,
) -> Result<ProjectSourceToolContext, String> {
    validate_tool_identifier(tool_name)?;

    let tool_manifest_path = project_tools_root(project_root)
        .join(tool_name)
        .join(TOOL_MANIFEST_FILE_NAME);
    if !tool_manifest_path.exists() {
        let machine_manifest_path = machine_tools_root()
            .join(tool_name)
            .join(TOOL_MANIFEST_FILE_NAME);
        if machine_manifest_path.exists() {
            return Err(format!(
                "Tool '{}' is only materialized in Cargo AI Home. `cargo ai tools lint` currently supports project-local source-backed tools only.",
                tool_name
            ));
        }
        return Err(format!(
            "Tool '{}' was not found in the current project's managed tool metadata.",
            tool_name
        ));
    }

    let manifest = load_tool_manifest(&tool_manifest_path, tool_name)?;
    let source = manifest.source.ok_or_else(|| {
        format!(
            "Tool '{}' is not source-backed. `cargo ai tools lint` currently supports project-local source-backed tools only.",
            tool_name
        )
    })?;
    validate_project_relative_path(source.manifest_path.as_str(), "Tool source manifest")?;

    let source_manifest_path = project_root.join(source.manifest_path.as_str());
    let source_root = source_manifest_path
        .parent()
        .ok_or_else(|| {
            format!(
                "Tool '{}' source manifest path '{}' has no parent directory.",
                tool_name, source.manifest_path
            )
        })?
        .to_path_buf();

    Ok(ProjectSourceToolContext {
        tool_id: tool_name.to_string(),
        tool_manifest_path,
        source_manifest_relative_path: source.manifest_path,
        source_manifest_path,
        source_root,
    })
}

fn lint_project_source_tool(
    project_root: &Path,
    tool_name: &str,
) -> Result<ToolLintReport, String> {
    let context = load_project_source_tool_context(project_root, tool_name)?;
    let mut report = ToolLintReport {
        tool_id: tool_name.to_string(),
        ..ToolLintReport::default()
    };

    lint_universal_source_checks(&context, &mut report);
    lint_scaffold_source_checks(&context, &mut report);

    Ok(report)
}

fn lint_universal_source_checks(context: &ProjectSourceToolContext, report: &mut ToolLintReport) {
    if Path::new(context.source_manifest_relative_path.as_str())
        .file_name()
        .and_then(|value| value.to_str())
        != Some("Cargo.toml")
    {
        report.errors.push(format!(
            "Managed tool manifest '{}' points to '{}', but source-backed tools must reference a Cargo.toml manifest.",
            context.tool_manifest_path.display(),
            context.source_manifest_relative_path
        ));
    }

    if !context.source_manifest_path.exists() {
        report.errors.push(format!(
            "Source manifest '{}' does not exist on disk.",
            context.source_manifest_path.display()
        ));
        return;
    }

    let cargo_toml = match fs::read_to_string(&context.source_manifest_path) {
        Ok(contents) => contents,
        Err(error) => {
            report.errors.push(format!(
                "Failed to read source manifest '{}': {}",
                context.source_manifest_path.display(),
                error
            ));
            return;
        }
    };
    let parsed: toml::Value = match toml::from_str(&cargo_toml) {
        Ok(parsed) => parsed,
        Err(error) => {
            report.errors.push(format!(
                "Source manifest '{}' is not valid TOML: {}",
                context.source_manifest_path.display(),
                error
            ));
            return;
        }
    };

    let package_name = parsed
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if package_name.is_none() {
        report.errors.push(format!(
            "Source manifest '{}' must define a non-empty [package].name for the tool crate.",
            context.source_manifest_path.display()
        ));
    }
}

fn lint_scaffold_source_checks(context: &ProjectSourceToolContext, report: &mut ToolLintReport) {
    let main_rs_path = context.source_root.join("src").join("main.rs");
    let lib_rs_path = context.source_root.join("src").join("lib.rs");
    let agent_bridge_rs_path = context.source_root.join("src").join("agent_bridge.rs");
    let tool_rs_path = context.source_root.join("src").join("tool.rs");
    let scaffold_paths = [
        (&main_rs_path, "src/main.rs"),
        (&lib_rs_path, "src/lib.rs"),
        (&agent_bridge_rs_path, "src/agent_bridge.rs"),
        (&tool_rs_path, "src/tool.rs"),
    ];
    let scaffold_layout_present = scaffold_paths.iter().any(|(path, _)| path.exists());

    if !scaffold_layout_present {
        report.notes.push(format!(
            "Skipped scaffold-specific checks for '{}' because the source crate does not use the standard `cargo ai add tool` layout.",
            context.tool_id
        ));
        return;
    }

    for (path, label) in &scaffold_paths {
        if !path.exists() {
            report.errors.push(format!(
                "Scaffold-specific lint expected '{}' for tool '{}' but it is missing.",
                path.display(),
                context.tool_id
            ));
        } else if path
            .strip_prefix(&context.source_root)
            .ok()
            .map(|relative| relative != Path::new(label))
            .unwrap_or(false)
        {
            report.errors.push(format!(
                "Scaffold-specific lint expected '{}' under the tool source root for '{}'.",
                label, context.tool_id
            ));
        }
    }

    if !report.errors.is_empty() {
        return;
    }

    require_file_contains(
        &lib_rs_path,
        "Cargo AI protocol adapter for this tool.",
        report,
        "src/lib.rs no longer advertises the Cargo AI protocol adapter seam",
    );
    require_file_contains(
        &lib_rs_path,
        "mod tool;",
        report,
        "src/lib.rs no longer declares the author-owned tool module",
    );
    require_file_contains(
        &lib_rs_path,
        "mod agent_bridge;",
        report,
        "src/lib.rs no longer declares the Cargo AI-owned agent bridge module",
    );
    require_file_contains(
        &agent_bridge_rs_path,
        "Cargo AI-owned helper layer for tool-authored child-agent calls.",
        report,
        "src/agent_bridge.rs no longer contains the Cargo AI-owned helper marker",
    );
    require_file_contains(
        &agent_bridge_rs_path,
        "ChildAgentRequest",
        report,
        "src/agent_bridge.rs no longer exposes the child-agent request helper surface",
    );
    require_file_contains(
        &tool_rs_path,
        "Author-owned implementation area for this Cargo AI tool.",
        report,
        "src/tool.rs no longer contains the author-owned implementation marker",
    );
    require_file_contains(
        &tool_rs_path,
        format!(
            "pub(crate) const TOOL_NAME: &str = \"{}\";",
            context.tool_id
        )
        .as_str(),
        report,
        "src/tool.rs no longer declares the expected TOOL_NAME constant for this tool",
    );
}

fn require_file_contains(path: &Path, needle: &str, report: &mut ToolLintReport, message: &str) {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) => {
            report.errors.push(format!(
                "Failed to read '{}' during tool lint: {}",
                path.display(),
                error
            ));
            return;
        }
    };
    if !contents.contains(needle) {
        report
            .errors
            .push(format!("{} ('{}').", message, path.display()));
    }
}

pub(crate) fn run(sub_m: &ArgMatches) -> bool {
    if let Some(build_m) = sub_m.subcommand_matches("build") {
        run_build(build_m)
    } else if let Some(describe_m) = sub_m.subcommand_matches("describe") {
        run_describe(describe_m)
    } else if let Some(lint_m) = sub_m.subcommand_matches("lint") {
        run_lint(lint_m)
    } else if let Some(check_m) = sub_m.subcommand_matches("check") {
        run_check(check_m)
    } else {
        eprintln!(
            "No tools subcommand found. Try `cargo ai tools build <name>`, `cargo ai tools describe <name>`, `cargo ai tools lint <name>`, or `cargo ai tools check <name>`."
        );
        false
    }
}

fn run_build(sub_m: &ArgMatches) -> bool {
    let Some(name) = sub_m.get_one::<String>("name").map(String::as_str) else {
        eprintln!("x Missing tool name. Use `cargo ai tools build <name>`.");
        return false;
    };
    let project_root = match current_project_root() {
        Some(root) => root,
        None => {
            eprintln!(
                "x No Cargo AI project metadata was found from the current directory upward."
            );
            return false;
        }
    };

    let build_target = match crate::agent_builder::build_target::BuildTarget::from_cli(
        sub_m.get_one::<String>("target").map(String::as_str),
    ) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("x {error}");
            return false;
        }
    };
    match build_source_tool(
        name,
        &build_target,
        ToolScope::Project,
        &project_root,
        CargoCompileProfile::Dev,
    ) {
        Ok(resolved) => {
            println!("✓ Tool built");
            println!("Cargo profile: {}", CargoCompileProfile::Dev.name());
            println!("Tool:   {}", resolved.tool_id);
            println!("Scope:  {}", display_scope(&resolved.scope));
            println!("Target: {}", resolved.target_triple);
            println!("Path:   {}", resolved.binary_path.display());
            true
        }
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

fn run_lint(sub_m: &ArgMatches) -> bool {
    let Some(name) = sub_m.get_one::<String>("name").map(String::as_str) else {
        eprintln!("x Missing tool name. Use `cargo ai tools lint <name>`.");
        return false;
    };
    let project_root = match current_project_root() {
        Some(root) => root,
        None => {
            eprintln!(
                "x No Cargo AI project metadata was found from the current directory upward."
            );
            return false;
        }
    };

    match lint_project_source_tool(&project_root, name) {
        Ok(report) if report.errors.is_empty() => {
            println!("✓ Tool lint passed");
            println!("Tool:   {}", report.tool_id);
            if !report.notes.is_empty() {
                println!("Notes:");
                for note in report.notes {
                    println!("- {note}");
                }
            }
            true
        }
        Ok(report) => {
            eprintln!("x Tool lint failed");
            eprintln!("Tool:   {}", report.tool_id);
            eprintln!("Problems:");
            for error in report.errors {
                eprintln!("- {error}");
            }
            if !report.notes.is_empty() {
                eprintln!("Notes:");
                for note in report.notes {
                    eprintln!("- {note}");
                }
            }
            false
        }
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

fn run_describe(sub_m: &ArgMatches) -> bool {
    let Some(name) = sub_m.get_one::<String>("name").map(String::as_str) else {
        eprintln!("x Missing tool name. Use `cargo ai tools describe <name>`.");
        return false;
    };

    match resolver_from_current_dir(sub_m).and_then(|resolver| resolver.resolve_contract(name)) {
        Ok(contract) => {
            match serde_json::to_string_pretty(&contract.describe) {
                Ok(rendered) => println!("{rendered}"),
                Err(error) => {
                    eprintln!("x Failed to render tool describe output: {error}");
                    return false;
                }
            }
            true
        }
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

fn run_check(sub_m: &ArgMatches) -> bool {
    if let Some(config) = sub_m.get_one::<String>("config").map(String::as_str) {
        let definition = match crate::runtime_definition::RuntimeAgentDefinition::load_from_path(
            Path::new(config),
        ) {
            Ok(definition) => definition,
            Err(error) => {
                eprintln!("x {error}");
                return false;
            }
        };
        let target_triple = sub_m
            .get_one::<String>("target")
            .cloned()
            .unwrap_or_else(crate::cargo_ai_metadata::current_build_target);
        let project_root = std::env::current_dir()
            .ok()
            .and_then(|_| maybe_find_project_root(Path::new(config)));
        let resolver = ToolResolver::new(project_root, target_triple);
        match audit_actions_for_tools(&definition.actions(), &resolver, current_platform_label()) {
            Ok(resolved) => {
                println!("✓ Tool contract checks passed");
                println!("Tools: {}", resolved.len());
                for tool in resolved {
                    println!(
                        "- {} ({}, {})",
                        tool.tool_id,
                        display_scope(&tool.scope),
                        tool.target_triple
                    );
                }
                true
            }
            Err(error) => {
                eprintln!("x {error}");
                false
            }
        }
    } else if let Some(name) = sub_m.get_one::<String>("name").map(String::as_str) {
        match resolver_from_current_dir(sub_m).and_then(|resolver| resolver.resolve_contract(name))
        {
            Ok(contract) => {
                println!("✓ Tool contract checks passed");
                println!("Tool:   {}", contract.resolved.tool_id);
                println!("Scope:  {}", display_scope(&contract.resolved.scope));
                println!("Target: {}", contract.resolved.target_triple);
                true
            }
            Err(error) => {
                eprintln!("x {error}");
                false
            }
        }
    } else {
        eprintln!("x Missing tool name or --config for `cargo ai tools check`.");
        false
    }
}

fn current_project_root() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .and_then(|dir| maybe_find_project_root(dir.as_path()))
}

fn resolver_from_current_dir(sub_m: &ArgMatches) -> Result<ToolResolver, String> {
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("Failed to read current directory: {error}"))?;
    let project_root = maybe_find_project_root(&current_dir);
    let target_triple = sub_m
        .get_one::<String>("target")
        .cloned()
        .unwrap_or_else(crate::cargo_ai_metadata::current_build_target);
    Ok(ToolResolver::new(project_root, target_triple))
}

fn resolve_tool_from_scope_root(
    scope_root: &Path,
    scope: ToolScope,
    tool_id: &str,
    target_triple: &str,
) -> Result<Option<ResolvedTool>, String> {
    validate_tool_identifier(tool_id)?;
    let candidate_tool_dir = scope_root.join(tool_id);
    match fs::symlink_metadata(&candidate_tool_dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to inspect {} tool '{}' at '{}': {}",
                display_scope(&scope),
                tool_id,
                candidate_tool_dir.display(),
                error
            ));
        }
    }
    let tool_dir = crate::commands::local_packages::resolve_existing_path_under_root(
        scope_root,
        Path::new(tool_id),
        format!("{} tool '{}' root", display_scope(&scope), tool_id).as_str(),
    )?;
    if !tool_dir.is_dir() {
        return Err(format!(
            "{} tool '{}' root '{}' must be a real directory.",
            display_scope(&scope),
            tool_id,
            tool_dir.display()
        ));
    }

    let manifest_path = crate::commands::local_packages::resolve_existing_path_under_root(
        tool_dir.as_path(),
        Path::new(TOOL_MANIFEST_FILE_NAME),
        format!("{} tool '{}' manifest", display_scope(&scope), tool_id).as_str(),
    )?;
    if !manifest_path.is_file() {
        return Err(format!(
            "{} tool '{}' is missing '{}'.",
            display_scope(&scope),
            tool_id,
            manifest_path.display()
        ));
    }

    let manifest = load_tool_manifest(&manifest_path, tool_id)?;
    let artifact = manifest.artifacts.get(target_triple).ok_or_else(|| {
        let remediation = if scope == ToolScope::Project && manifest.source.is_some() {
            format!(
                " Materialize it with `cargo ai tools build {tool_id} --target {target_triple}` or assemble the full project with `cargo ai build --target {target_triple}`."
            )
        } else {
            String::new()
        };
        format!(
            "Tool '{}' does not have a materialized artifact for target '{}'.{}",
            tool_id, target_triple, remediation
        )
    })?;
    let binary_name = default_binary_name_for_manifest(&manifest);
    let binary_path = if scope == ToolScope::Bundled {
        let artifact_relative = crate::commands::local_packages::normalize_portable_relative_path(
            artifact.path.as_str(),
            format!("Tool '{}' artifact path", tool_id).as_str(),
        )?;
        crate::commands::local_packages::resolve_existing_path_under_root(
            tool_dir.as_path(),
            artifact_relative.as_path(),
            format!("Tool '{}' artifact", tool_id).as_str(),
        )?
    } else {
        tool_dir.join(&artifact.path)
    };
    if !binary_path.is_file() {
        return Err(format!(
            "Tool '{}' artifact '{}' must be a regular file.",
            tool_id,
            binary_path.display()
        ));
    }

    Ok(Some(ResolvedTool {
        tool_id: tool_id.to_string(),
        scope,
        manifest_path,
        binary_name,
        target_triple: target_triple.to_string(),
        binary_path,
    }))
}

fn load_tool_manifest(path: &Path, expected_tool_id: &str) -> Result<ToolManifest, String> {
    let contents = fs::read_to_string(path).map_err(|error| {
        format!(
            "Failed to read tool manifest '{}': {}",
            path.display(),
            error
        )
    })?;
    let manifest: ToolManifest = serde_json::from_str(&contents).map_err(|error| {
        format!(
            "Failed to parse tool manifest '{}': {}",
            path.display(),
            error
        )
    })?;
    if manifest.schema_version != 1 {
        return Err(format!(
            "Tool manifest '{}' uses unsupported schema_version {}.",
            path.display(),
            manifest.schema_version
        ));
    }
    if manifest.tool_id != expected_tool_id {
        return Err(format!(
            "Tool manifest '{}' declares tool_id '{}', expected '{}'.",
            path.display(),
            manifest.tool_id,
            expected_tool_id
        ));
    }
    Ok(manifest)
}

fn load_tool_describe_document(resolved: &ResolvedTool) -> Result<ToolDescribeDocument, String> {
    let stdout = run_tool_command_capture_stdout(&resolved.binary_path, "describe", None)?;
    serde_json::from_slice::<ToolDescribeDocument>(&stdout).map_err(|error| {
        format!(
            "Tool '{}' returned invalid describe JSON: {}",
            resolved.tool_id, error
        )
    })
}

fn run_tool_command_capture_stdout(
    binary_path: &Path,
    command: &str,
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let mut child = Command::new(binary_path);
    child
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        child.stdin(Stdio::piped());
    }

    let mut child = child.spawn().map_err(|error| {
        format!(
            "Failed to start tool binary '{}': {}",
            binary_path.display(),
            error
        )
    })?;

    if let Some(stdin_bytes) = stdin {
        use std::io::Write;
        let mut writer = child.stdin.take().ok_or_else(|| {
            format!(
                "Failed to open stdin for tool binary '{}'.",
                binary_path.display()
            )
        })?;
        writer.write_all(stdin_bytes).map_err(|error| {
            format!(
                "Failed to write stdin for tool binary '{}': {}",
                binary_path.display(),
                error
            )
        })?;
    }

    let output = child.wait_with_output().map_err(|error| {
        format!(
            "Failed while waiting for tool binary '{}': {}",
            binary_path.display(),
            error
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "Tool binary '{}' exited with status {}.",
                binary_path.display(),
                output.status
            )
        } else {
            format!(
                "Tool binary '{}' exited with status {}: {}",
                binary_path.display(),
                output.status,
                stderr
            )
        });
    }

    Ok(output.stdout)
}

fn validate_describe_document(
    describe: &ToolDescribeDocument,
    resolved: &ResolvedTool,
) -> Result<(), String> {
    if describe.protocol_version != TOOL_PROTOCOL_VERSION {
        return Err(format!(
            "Tool '{}' reports unsupported protocol_version {} in describe.",
            resolved.tool_id, describe.protocol_version
        ));
    }
    if describe.name.trim().is_empty() {
        return Err(format!(
            "Tool '{}' describe output is missing a non-empty `name`.",
            resolved.tool_id
        ));
    }
    if describe.description.trim().is_empty() {
        return Err(format!(
            "Tool '{}' describe output is missing a non-empty `description`.",
            resolved.tool_id
        ));
    }
    if describe.result.kind != "string" || !describe.result.nullable {
        return Err(format!(
            "Tool '{}' describe result must be a nullable string.",
            resolved.tool_id
        ));
    }

    for (name, param) in &describe.params {
        if !matches!(
            param.kind.as_str(),
            "string" | "boolean" | "integer" | "number" | "array" | "object"
        ) {
            return Err(format!(
                "Tool '{}' describe param '{}' uses unsupported type '{}'.",
                resolved.tool_id, name, param.kind
            ));
        }
        if param.required && param.default.is_some() {
            return Err(format!(
                "Tool '{}' describe param '{}' cannot be both required and defaulted.",
                resolved.tool_id, name
            ));
        }
        if let Some(default) = param.default.as_ref() {
            if !json_value_matches_declared_type(default, param.kind.as_str()) {
                return Err(format!(
                    "Tool '{}' describe param '{}' default does not match declared type '{}'.",
                    resolved.tool_id, name, param.kind
                ));
            }
        }
    }

    for (label, value) in [
        ("network", describe.resource_profile.network.as_str()),
        (
            "filesystem_read",
            describe.resource_profile.filesystem_read.as_str(),
        ),
        (
            "filesystem_write",
            describe.resource_profile.filesystem_write.as_str(),
        ),
        ("subprocess", describe.resource_profile.subprocess.as_str()),
        ("env_read", describe.resource_profile.env_read.as_str()),
        (
            "credential_access",
            describe.resource_profile.credential_access.as_str(),
        ),
    ] {
        if !matches!(value, "none" | "optional" | "required") {
            return Err(format!(
                "Tool '{}' describe resource_profile.{} uses unsupported value '{}'.",
                resolved.tool_id, label, value
            ));
        }
    }

    for (label, example) in [
        ("minimal_invoke", &describe.examples.minimal_invoke),
        ("full_invoke", &describe.examples.full_invoke),
    ] {
        let Some(example_obj) = example.as_object() else {
            return Err(format!(
                "Tool '{}' describe examples.{} must be an object.",
                resolved.tool_id, label
            ));
        };
        let protocol_version = example_obj
            .get("protocol_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                format!(
                    "Tool '{}' describe examples.{} is missing integer protocol_version.",
                    resolved.tool_id, label
                )
            })?;
        if protocol_version != TOOL_PROTOCOL_VERSION as u64 {
            return Err(format!(
                "Tool '{}' describe examples.{} must use protocol_version {}.",
                resolved.tool_id, label, TOOL_PROTOCOL_VERSION
            ));
        }
        if !example_obj.get("params").is_some_and(Value::is_object) {
            return Err(format!(
                "Tool '{}' describe examples.{} must include an object `params` field.",
                resolved.tool_id, label
            ));
        }
    }

    Ok(())
}

pub(crate) fn validate_tool_invoke_response(
    resolved: &ResolvedTool,
    stdout: &[u8],
) -> Result<Option<String>, String> {
    let response: ToolInvokeResponse = serde_json::from_slice(stdout).map_err(|error| {
        format!(
            "Tool '{}' returned invalid invoke JSON: {}",
            resolved.tool_id, error
        )
    })?;
    if response.protocol_version != TOOL_PROTOCOL_VERSION {
        return Err(format!(
            "Tool '{}' returned unsupported invoke protocol_version {}.",
            resolved.tool_id, response.protocol_version
        ));
    }
    Ok(response.result)
}

fn json_value_matches_declared_type(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn display_type_name(kind: &str) -> &str {
    match kind {
        "string" => "a string",
        "boolean" => "a boolean",
        "integer" => "an integer",
        "number" => "a number",
        "array" => "an array",
        "object" => "an object",
        _ => "a supported value",
    }
}

fn describe_runtime_value(value: &Value) -> &'static str {
    if value.is_string() {
        "a string"
    } else if value.is_boolean() {
        "a boolean"
    } else if value.as_i64().is_some() || value.as_u64().is_some() {
        "an integer"
    } else if value.is_number() {
        "a number"
    } else if value.is_array() {
        "an array"
    } else if value.is_object() {
        "an object"
    } else {
        "null"
    }
}

pub(crate) fn validate_tool_identifier(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("Tool names cannot be empty.".to_string());
    }
    if name != name.trim() {
        return Err("Tool names cannot start or end with whitespace.".to_string());
    }
    if name.chars().any(char::is_whitespace) {
        return Err("Tool names cannot contain whitespace.".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err(
            "Tool names must be a single portable path component and cannot contain '/' or '\\'."
                .to_string(),
        );
    }
    let normalized =
        crate::commands::local_packages::normalize_portable_relative_path(name, "Tool name")?;
    if normalized.components().count() != 1 {
        return Err(format!(
            "Tool name '{}' must be a single portable path component.",
            name
        ));
    }
    Ok(())
}

fn validate_local_tool_name(name: &str) -> Result<(), String> {
    validate_tool_identifier(name)?;
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(
            "Local tool scaffold names may use only ASCII letters, numbers, '_' or '-'."
                .to_string(),
        );
    }
    Ok(())
}

fn write_utf8_file(path: &Path, contents: String) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("Failed to write '{}': {}", path.display(), error))
}

fn render_source_tool_manifest_json(
    tool_id: &str,
    manifest_path: &str,
    binary_name: &str,
) -> String {
    serde_json::json!({
        "schema_version": 1,
        "tool_id": tool_id,
        "source": {
            "manifest_path": manifest_path
        },
        "binary": {
            "default_name": binary_name
        },
        "artifacts": {}
    })
    .to_string()
}

fn render_project_built_tool_manifest_json(
    tool_id: &str,
    manifest_path: &str,
    binary_name: &str,
    target_triple: &str,
    artifact_path: &str,
) -> String {
    serde_json::json!({
        "schema_version": 1,
        "tool_id": tool_id,
        "source": {
            "manifest_path": manifest_path
        },
        "binary": {
            "default_name": binary_name
        },
        "artifacts": {
            target_triple: {
                "path": artifact_path
            }
        }
    })
    .to_string()
}

fn render_binary_tool_manifest_json(
    tool_id: &str,
    binary_name: &str,
    target_triple: &str,
    artifact_path: &str,
) -> String {
    serde_json::json!({
        "schema_version": 1,
        "tool_id": tool_id,
        "binary": {
            "default_name": binary_name
        },
        "artifacts": {
            target_triple: {
                "path": artifact_path
            }
        }
    })
    .to_string()
}

fn default_binary_name_for_manifest(manifest: &ToolManifest) -> String {
    manifest
        .binary
        .default_name
        .clone()
        .unwrap_or_else(|| manifest.tool_id.clone())
}

fn display_scope(scope: &ToolScope) -> &'static str {
    match scope {
        ToolScope::Bundled => "bundled",
        ToolScope::Project => "project",
        ToolScope::Machine => "machine",
    }
}

fn lookup_action_variable<'a>(data: &'a Value, variable: &str) -> Option<&'a Value> {
    if let Some(runtime_name) = variable.strip_prefix("runtime.") {
        return data
            .get("runtime")
            .and_then(Value::as_object)
            .and_then(|runtime| runtime.get(runtime_name));
    }

    if variable.contains('.') {
        return None;
    }

    data.get(variable)
}

fn tool_step_context(action_name: Option<&str>, step_index: Option<usize>) -> String {
    match (action_name, step_index) {
        (Some(action_name), Some(step_index)) => {
            format!("Action '{}' tool step {}", action_name, step_index + 1)
        }
        (Some(action_name), None) => format!("Action '{}' tool step", action_name),
        _ => "Tool step".to_string(),
    }
}

fn current_platform_label() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("macos")
    } else if cfg!(target_os = "linux") {
        Some("linux")
    } else if cfg!(target_os = "windows") {
        Some("windows")
    } else {
        None
    }
}

fn step_matches_platform(platforms: Option<&[String]>, current_platform: Option<&str>) -> bool {
    match platforms {
        None => true,
        Some(platforms) => current_platform
            .is_some_and(|platform| platforms.iter().any(|candidate| candidate == platform)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        lint_project_source_tool, materialize_source_tool_for_package_runtime,
        maybe_find_project_root, render_binary_tool_manifest_json,
        render_source_tool_manifest_json, resolve_tool_from_scope_root, scaffold_local_tool,
        validate_describe_document, validate_local_tool_name, validate_package_runtime_tools,
        validate_tool_identifier, ResolvedTool, ToolDescribeDocument, ToolDescribeExamples,
        ToolDescribeParam, ToolDescribeResourceProfile, ToolDescribeResult, ToolDescribeSelfTest,
        ToolResolver, ToolScope,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(stem: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cargo-ai-tools-test-{stem}-{nanos}"))
    }

    fn cargo_command() -> Command {
        match std::env::var_os("CARGO") {
            Some(path) => Command::new(path),
            None => Command::new("cargo"),
        }
    }

    #[cfg(unix)]
    fn make_executable_script(path: &PathBuf, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, body).expect("script should be written");
        let mut permissions = fs::metadata(path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("script should be executable");
    }

    #[cfg(unix)]
    fn write_machine_tool_fixture(root: &PathBuf, tool_id: &str, target: &str) -> PathBuf {
        let tool_dir = root.join("tools").join(tool_id);
        fs::create_dir_all(&tool_dir).expect("machine tool dir should be created");

        let script_path = tool_dir.join(tool_id);
        make_executable_script(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"describe\" ]; then\nprintf '{\"protocol_version\":1,\"name\":\"machine_only\",\"description\":\"machine fixture\",\"params\":{},\"result\":{\"type\":\"string\",\"nullable\":true},\"resource_profile\":{\"network\":\"none\",\"filesystem_read\":\"none\",\"filesystem_write\":\"none\",\"subprocess\":\"none\",\"env_read\":\"none\",\"credential_access\":\"none\"},\"self_test\":{\"supported\":false,\"safe\":false},\"examples\":{\"minimal_invoke\":{\"protocol_version\":1,\"params\":{}},\"full_invoke\":{\"protocol_version\":1,\"params\":{}}}}\\n'\nelse\nprintf '{\"protocol_version\":1,\"result\":\"ok\"}\\n'\nfi\n",
        );

        let manifest = render_binary_tool_manifest_json(
            tool_id,
            tool_id,
            target,
            script_path
                .strip_prefix(&tool_dir)
                .expect("artifact should be relative to tool dir")
                .to_string_lossy()
                .as_ref(),
        );
        fs::write(tool_dir.join("tool.json"), manifest).expect("tool manifest should be written");

        script_path
    }

    fn write_source_tool_fixture(
        project_root: &PathBuf,
        tool_id: &str,
        manifest_relative_path: &str,
    ) -> PathBuf {
        let source_manifest_path = project_root.join(manifest_relative_path);
        fs::create_dir_all(
            source_manifest_path
                .parent()
                .expect("manifest parent should exist"),
        )
        .expect("source manifest parent should be created");
        fs::write(
            &source_manifest_path,
            format!(
                "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                tool_id
            ),
        )
        .expect("source manifest should be written");

        let tool_dir = project_root.join(".cargo-ai/tools").join(tool_id);
        fs::create_dir_all(&tool_dir).expect("tool metadata dir should be created");
        fs::write(
            tool_dir.join("tool.json"),
            render_source_tool_manifest_json(tool_id, manifest_relative_path, tool_id),
        )
        .expect("tool manifest should be written");

        source_manifest_path
    }

    #[test]
    fn project_source_tool_missing_artifact_suggests_materialization_commands() {
        let project_root = temp_dir("missing-artifact-remediation");
        let tool_dir = project_root.join(".cargo-ai/tools").join("hello_tool");
        fs::create_dir_all(&tool_dir).expect("tool metadata dir should be created");
        fs::write(
            tool_dir.join("tool.json"),
            render_source_tool_manifest_json(
                "hello_tool",
                "tools/hello_tool/Cargo.toml",
                "hello_tool",
            ),
        )
        .expect("tool manifest should be written");

        let error = resolve_tool_from_scope_root(
            &project_root.join(".cargo-ai/tools"),
            ToolScope::Project,
            "hello_tool",
            "aarch64-apple-darwin",
        )
        .expect_err("missing artifact should fail");

        assert!(error.contains("cargo ai tools build hello_tool --target aarch64-apple-darwin"));
        assert!(error.contains("cargo ai build --target aarch64-apple-darwin"));

        let _ = fs::remove_dir_all(project_root);
    }

    #[test]
    fn tool_artifact_path_rejects_parent_traversal() {
        let root = temp_dir("artifact-parent-traversal");
        let tool_dir = root.join("hello_tool");
        fs::create_dir_all(&tool_dir).expect("tool directory should exist");
        fs::write(root.join("outside-tool"), "outside").expect("outside file should exist");
        fs::write(
            tool_dir.join("tool.json"),
            render_binary_tool_manifest_json(
                "hello_tool",
                "hello_tool",
                "test-target",
                "../outside-tool",
            ),
        )
        .expect("tool manifest should be written");

        let error = resolve_tool_from_scope_root(
            root.as_path(),
            ToolScope::Bundled,
            "hello_tool",
            "test-target",
        )
        .expect_err("artifact traversal should fail");

        assert!(error.contains("parent traversal") || error.contains("stay beneath"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn tool_artifact_path_rejects_symbolic_link() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("artifact-symlink");
        let tool_dir = root.join("hello_tool");
        let artifact_dir = tool_dir.join("bin/test-target");
        fs::create_dir_all(&artifact_dir).expect("artifact directory should exist");
        let outside = root.join("outside-tool");
        fs::write(&outside, "outside").expect("outside file should exist");
        symlink(&outside, artifact_dir.join("hello_tool"))
            .expect("artifact symlink should be created");
        fs::write(
            tool_dir.join("tool.json"),
            render_binary_tool_manifest_json(
                "hello_tool",
                "hello_tool",
                "test-target",
                "bin/test-target/hello_tool",
            ),
        )
        .expect("tool manifest should be written");

        let error = resolve_tool_from_scope_root(
            root.as_path(),
            ToolScope::Bundled,
            "hello_tool",
            "test-target",
        )
        .expect_err("artifact symlink should fail");

        assert!(error.contains("symbolic link") || error.contains("reparse point"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn finds_project_root_by_managed_metadata() {
        let root = temp_dir("project-root");
        let nested = root.join("examples").join("agents");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be written");
        fs::create_dir_all(&nested).expect("nested dir should be created");

        let found = maybe_find_project_root(&nested).expect("project root should be found");
        assert_eq!(found, root);

        let _ = fs::remove_dir_all(found);
    }

    #[cfg(unix)]
    #[test]
    fn project_tool_resolution_defaults_to_project_only_when_policy_missing() {
        let _guard = crate::commands::runtime_actions::TEST_ENV_LOCK
            .lock()
            .expect("environment lock should not be poisoned");
        let original_cargo_ai_home = std::env::var_os("CARGO_AI_HOME");
        let root = temp_dir("resolver-project-only");
        let machine_home = temp_dir("resolver-machine-home");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be written");
        std::env::set_var("CARGO_AI_HOME", &machine_home);
        write_machine_tool_fixture(&machine_home, "machine_only", "test-target");

        let resolver = ToolResolver::new(Some(root.clone()), "test-target");
        let err = resolver
            .resolve_tool("machine_only")
            .expect_err("missing policy should block machine fallback");
        assert!(err.contains("disallows Cargo AI Home fallback"));

        match original_cargo_ai_home {
            Some(value) => std::env::set_var("CARGO_AI_HOME", value),
            None => std::env::remove_var("CARGO_AI_HOME"),
        }
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(machine_home);
    }

    #[cfg(unix)]
    #[test]
    fn project_tool_resolution_can_fallback_to_machine_when_policy_enabled() {
        let _guard = crate::commands::runtime_actions::TEST_ENV_LOCK
            .lock()
            .expect("environment lock should not be poisoned");
        let original_cargo_ai_home = std::env::var_os("CARGO_AI_HOME");
        let root = temp_dir("resolver-machine-fallback");
        let machine_home = temp_dir("resolver-machine-home-allowed");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(
            root.join(".cargo-ai/project.toml"),
            "format_version = 1\n\n[tools]\nallow_global_fallback = true\n",
        )
        .expect("project metadata should be written");
        std::env::set_var("CARGO_AI_HOME", &machine_home);
        let script_path = write_machine_tool_fixture(&machine_home, "machine_only", "test-target");

        let resolver = ToolResolver::new(Some(root.clone()), "test-target");
        let resolved = resolver
            .resolve_tool("machine_only")
            .expect("explicit policy should allow machine fallback");
        assert_eq!(resolved.scope, ToolScope::Machine);
        assert_eq!(resolved.binary_path, script_path);

        match original_cargo_ai_home {
            Some(value) => std::env::set_var("CARGO_AI_HOME", value),
            None => std::env::remove_var("CARGO_AI_HOME"),
        }
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(machine_home);
    }

    #[test]
    fn local_tool_name_validation_rejects_whitespace() {
        let err = validate_local_tool_name("bad name").expect_err("whitespace should fail");
        assert!(err.contains("whitespace"));
    }

    #[test]
    fn tool_identifier_rejects_nonportable_path_shapes_on_every_platform() {
        for invalid in [
            "foo/bar",
            r"foo\bar",
            ".",
            "..",
            "../foo",
            "C:foo",
            r"C:\foo",
            "/absolute",
            r"\\server\share",
        ] {
            assert!(
                validate_tool_identifier(invalid).is_err(),
                "non-portable tool identifier should fail: {invalid:?}"
            );
        }
    }

    #[test]
    fn tool_identifier_allows_portable_hyphen_underscore_dot_and_unicode_names() {
        for valid in ["render-cover_2", "résumé.tool-v2"] {
            validate_tool_identifier(valid)
                .unwrap_or_else(|error| panic!("portable tool identifier {valid:?}: {error}"));
        }
    }

    #[test]
    fn package_materialization_and_completeness_reject_nested_tool_ids_consistently() {
        let root = temp_dir("package-tool-id-validation");
        let package_root = root.join("package");
        let runtime_tools_root = root.join("runtime/tools");
        let scratch_root = root.join("scratch");
        let build_target =
            crate::agent_builder::build_target::BuildTarget::from_cli(Some("aarch64-apple-darwin"))
                .expect("test target should resolve");

        let materialize_error = materialize_source_tool_for_package_runtime(
            "foo/bar",
            &build_target,
            &package_root,
            &runtime_tools_root,
            &scratch_root,
        )
        .expect_err("nested forward-slash tool id must fail before materialization");
        assert!(materialize_error.contains("single portable path component"));
        assert!(!scratch_root.exists());
        assert!(!runtime_tools_root.exists());

        fs::create_dir_all(&runtime_tools_root).expect("runtime tools root should exist");
        let completeness_error = validate_package_runtime_tools(
            &runtime_tools_root,
            &[r"foo\bar".to_string()],
            "aarch64-apple-darwin",
        )
        .expect_err("nested backslash tool id must fail completeness validation");
        assert!(completeness_error.contains("single portable path component"));
        assert!(!runtime_tools_root.join("foo").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scaffold_local_tool_splits_protocol_adapter_from_author_code() {
        let root = temp_dir("tool-scaffold");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be written");

        scaffold_local_tool(&root, "hello_tool").expect("tool scaffold should succeed");

        let source_dir = root.join("tools/hello_tool/src");
        assert!(source_dir.join("main.rs").exists());
        assert!(source_dir.join("lib.rs").exists());
        assert!(source_dir.join("agent_bridge.rs").exists());
        assert!(source_dir.join("tool.rs").exists());
        assert!(root.join(".cargo-ai/tools/hello_tool/tool.json").exists());

        let lib_rs =
            fs::read_to_string(source_dir.join("lib.rs")).expect("lib.rs should be readable");
        assert!(lib_rs.contains("mod tool;"));
        assert!(lib_rs.contains("Cargo AI protocol adapter"));

        let agent_bridge_rs = fs::read_to_string(source_dir.join("agent_bridge.rs"))
            .expect("agent_bridge.rs should be readable");
        assert!(agent_bridge_rs.contains("Cargo AI-owned helper layer"));
        assert!(agent_bridge_rs.contains("ChildAgentRequest"));

        let tool_rs =
            fs::read_to_string(source_dir.join("tool.rs")).expect("tool.rs should be readable");
        assert!(tool_rs.contains("Author-owned implementation area"));
        assert!(tool_rs.contains("pub(crate) const TOOL_NAME: &str = \"hello_tool\";"));
        assert!(tool_rs.contains("context.invoke_agent(request)?;"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lint_project_source_tool_passes_for_scaffolded_tool() {
        let root = temp_dir("tool-lint-scaffold");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(
            root.join(".cargo-ai/project.toml"),
            "format_version = 1\n\n[tools]\nallow_global_fallback = true\n",
        )
        .expect("project metadata should be written");

        scaffold_local_tool(&root, "hello_tool").expect("tool scaffold should succeed");
        let report = lint_project_source_tool(&root, "hello_tool")
            .expect("lint should succeed structurally");

        assert!(
            report.errors.is_empty(),
            "report should be clean: {report:?}"
        );
        assert!(
            report.notes.is_empty(),
            "scaffolded tool should not skip checks"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lint_project_source_tool_skips_scaffold_checks_for_non_scaffolded_source_tool() {
        let root = temp_dir("tool-lint-non-scaffold");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be written");
        write_source_tool_fixture(&root, "custom_tool", "custom_tools/custom_tool/Cargo.toml");

        let report =
            lint_project_source_tool(&root, "custom_tool").expect("lint should still succeed");

        assert!(
            report.errors.is_empty(),
            "report should be clean: {report:?}"
        );
        assert_eq!(report.notes.len(), 1);
        assert!(report.notes[0].contains("Skipped scaffold-specific checks"));

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn lint_project_source_tool_rejects_machine_only_tools() {
        let _guard = crate::commands::runtime_actions::TEST_ENV_LOCK
            .lock()
            .expect("environment lock should not be poisoned");
        let original_cargo_ai_home = std::env::var_os("CARGO_AI_HOME");
        let root = temp_dir("tool-lint-project-root");
        let machine_home = temp_dir("tool-lint-machine-only");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be written");
        std::env::set_var("CARGO_AI_HOME", &machine_home);
        write_machine_tool_fixture(&machine_home, "machine_only", "test-target");

        let error = lint_project_source_tool(&root, "machine_only")
            .expect_err("machine-only tool should be out of scope");
        assert!(error.contains("project-local source-backed tools only"));

        match original_cargo_ai_home {
            Some(value) => std::env::set_var("CARGO_AI_HOME", value),
            None => std::env::remove_var("CARGO_AI_HOME"),
        }
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(machine_home);
    }

    #[test]
    fn lint_project_source_tool_reports_missing_scaffold_files() {
        let root = temp_dir("tool-lint-missing-file");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be written");

        scaffold_local_tool(&root, "hello_tool").expect("tool scaffold should succeed");
        fs::remove_file(root.join("tools/hello_tool/src/agent_bridge.rs"))
            .expect("agent bridge file should be removed");

        let report =
            lint_project_source_tool(&root, "hello_tool").expect("lint should produce a report");
        let expected_relative_path = PathBuf::from("src").join("agent_bridge.rs");

        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains(expected_relative_path.to_string_lossy().as_ref())),
            "expected missing scaffold file error, got {report:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scaffolded_tool_compiles_with_agent_bridge_support() {
        let _guard = crate::commands::runtime_actions::TEST_ENV_LOCK
            .lock()
            .expect("environment lock should not be poisoned");
        let root = temp_dir("tool-compile");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be written");

        scaffold_local_tool(&root, "hello_tool").expect("tool scaffold should succeed");

        let output = cargo_command()
            .arg("check")
            .arg("--manifest-path")
            .arg(root.join("tools/hello_tool/Cargo.toml"))
            .arg("--target")
            .arg(crate::cargo_ai_metadata::current_build_target())
            .output()
            .expect("cargo check should start");

        if !output.status.success() {
            panic!(
                "scaffolded tool should compile\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn scaffolded_tool_agent_bridge_invokes_child_agent() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;

        let _guard = crate::commands::runtime_actions::TEST_ENV_LOCK
            .lock()
            .expect("environment lock should not be poisoned");
        let root = temp_dir("tool-child-agent");
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should be created");
        fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be written");

        scaffold_local_tool(&root, "hello_tool").expect("tool scaffold should succeed");

        let tool_source = root.join("tools/hello_tool/src/tool.rs");
        fs::write(
            &tool_source,
            r#"//! Author-owned implementation area for this Cargo AI tool.

use crate::{
    AccessLevel, AgentInputMode, ChildAgentRequest, InvocationContext, ParamSpec, ResourceProfile,
    ResultSpec, SelfTestSpec, ToolError,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) const TOOL_NAME: &str = "hello_tool";

pub(crate) fn description() -> &'static str {
    "Bridge test tool."
}

pub(crate) fn params() -> BTreeMap<String, ParamSpec> {
    BTreeMap::new()
}

pub(crate) fn result() -> ResultSpec {
    ResultSpec {
        kind: "string".to_string(),
        nullable: true,
        description: "Bridge result.".to_string(),
    }
}

pub(crate) fn resource_profile() -> ResourceProfile {
    ResourceProfile {
        network: AccessLevel::None,
        filesystem_read: AccessLevel::None,
        filesystem_write: AccessLevel::Optional,
        subprocess: AccessLevel::Required,
        env_read: AccessLevel::Optional,
        credential_access: AccessLevel::None,
    }
}

pub(crate) fn self_test() -> SelfTestSpec {
    SelfTestSpec {
        supported: false,
        safe: false,
        description: "Not implemented.".to_string(),
    }
}

pub(crate) fn minimal_example_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

pub(crate) fn full_example_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

pub(crate) fn invoke(
    _params: BTreeMap<String, Value>,
    context: InvocationContext,
) -> Result<Option<String>, ToolError> {
    let request = ChildAgentRequest::new("./child_agent")
        .with_input_mode(AgentInputMode::Append)
        .add_run_var("ticker", "MSFT")
        .add_input_override("company", "Microsoft")
        .add_text_input("from tool");
    context.invoke_agent(request)?;
    Ok(Some("child invoked".to_string()))
}
"#,
        )
        .expect("tool.rs should be overwritten");

        let child_capture_path = root.join("child-agent-capture.txt");
        let child_script_path = root.join("child_agent");
        let child_script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$CARGO_AI_AGENT_ACTION_DEPTH\" > \"{}\"\nprintf '%s\\n' \"$@\" >> \"{}\"\n",
            child_capture_path.display(),
            child_capture_path.display()
        );
        fs::write(&child_script_path, child_script).expect("child script should be written");
        let mut child_permissions = fs::metadata(&child_script_path)
            .expect("child script metadata should load")
            .permissions();
        child_permissions.set_mode(0o755);
        fs::set_permissions(&child_script_path, child_permissions)
            .expect("child script should be executable");

        let build_output = cargo_command()
            .arg("build")
            .arg("--manifest-path")
            .arg(root.join("tools/hello_tool/Cargo.toml"))
            .arg("--target")
            .arg(crate::cargo_ai_metadata::current_build_target())
            .output()
            .expect("cargo build should start");
        if !build_output.status.success() {
            panic!(
                "scaffolded tool should build\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&build_output.stdout),
                String::from_utf8_lossy(&build_output.stderr)
            );
        }

        let binary_name = if cfg!(windows) {
            "hello_tool.exe"
        } else {
            "hello_tool"
        };
        let binary_path = root
            .join("tools/hello_tool/target")
            .join(crate::cargo_ai_metadata::current_build_target())
            .join("debug")
            .join(binary_name);

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be valid")
            .as_millis() as u64;
        let request = serde_json::json!({
            "protocol_version": 1,
            "params": {},
            "runtime_context": {
                "agent_bridge": {
                    "current_depth": 1,
                    "max_depth": 3,
                    "runtime_budget": {
                        "max_runtime_secs": 600,
                        "started_at_ms": now_ms,
                        "deadline_ms": now_ms + 600_000
                    },
                    "action_execution": "sequential"
                }
            }
        });

        let mut child = Command::new(&binary_path)
            .arg("invoke")
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("tool invoke should start");
        child
            .stdin
            .as_mut()
            .expect("stdin should be available")
            .write_all(
                serde_json::to_vec(&request)
                    .expect("request should serialize")
                    .as_slice(),
            )
            .expect("request should be written");
        let output = child.wait_with_output().expect("tool invoke should finish");
        assert!(
            output.status.success(),
            "tool invoke should succeed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("response should be valid json");
        assert_eq!(response.get("result"), Some(&json!("child invoked")));

        let capture =
            fs::read_to_string(&child_capture_path).expect("child invocation should be captured");
        assert_eq!(
            capture.lines().collect::<Vec<_>>(),
            vec![
                "2",
                "--action-execution",
                "sequential",
                "--run-var",
                "ticker=MSFT",
                "--input-override",
                "company=Microsoft",
                "--input-mode",
                "append",
                "--input-text",
                "from tool",
            ]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn source_manifest_renders_expected_shape() {
        let rendered = render_source_tool_manifest_json(
            "render_cover_image",
            "tools/render_cover_image/Cargo.toml",
            "render_cover_image",
        );
        assert!(rendered.contains("\"tool_id\":\"render_cover_image\""));
        assert!(rendered.contains("\"manifest_path\":\"tools/render_cover_image/Cargo.toml\""));
    }

    #[test]
    fn binary_manifest_renders_target_artifact() {
        let rendered = render_binary_tool_manifest_json(
            "render_cover_image",
            "render_cover_image",
            "aarch64-apple-darwin",
            "bin/aarch64-apple-darwin/render_cover_image",
        );
        assert!(rendered.contains("\"aarch64-apple-darwin\""));
        assert!(rendered.contains("\"default_name\":\"render_cover_image\""));
    }

    #[test]
    fn validate_describe_document_rejects_non_nullable_string_result() {
        let root = temp_dir("describe-result");
        let tool_dir = root.join(".cargo-ai/tools/hello_tool");
        let binary_path = tool_dir.join("bin/aarch64-apple-darwin/hello_tool");
        let resolved = ResolvedTool {
            tool_id: "hello_tool".to_string(),
            scope: ToolScope::Project,
            manifest_path: tool_dir.join("tool.json"),
            binary_name: "hello_tool".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            binary_path,
        };
        let describe = ToolDescribeDocument {
            protocol_version: 1,
            name: "hello_tool".to_string(),
            description: "Example tool.".to_string(),
            params: BTreeMap::new(),
            result: ToolDescribeResult {
                kind: "string".to_string(),
                nullable: false,
                description: None,
            },
            resource_profile: ToolDescribeResourceProfile {
                network: "none".to_string(),
                filesystem_read: "none".to_string(),
                filesystem_write: "none".to_string(),
                subprocess: "none".to_string(),
                env_read: "none".to_string(),
                credential_access: "none".to_string(),
            },
            self_test: ToolDescribeSelfTest {
                supported: false,
                safe: false,
                description: None,
            },
            examples: ToolDescribeExamples {
                minimal_invoke: json!({ "protocol_version": 1, "params": {} }),
                full_invoke: json!({ "protocol_version": 1, "params": {} }),
            },
        };

        let error = validate_describe_document(&describe, &resolved)
            .expect_err("non-nullable result should fail");

        assert!(error.contains("nullable string"));
    }

    #[test]
    fn validate_describe_document_accepts_array_and_object_params() {
        let root = temp_dir("describe-structured-params");
        let tool_dir = root.join(".cargo-ai/tools/hello_tool");
        let binary_path = tool_dir.join("bin/aarch64-apple-darwin/hello_tool");
        let resolved = ResolvedTool {
            tool_id: "hello_tool".to_string(),
            scope: ToolScope::Project,
            manifest_path: tool_dir.join("tool.json"),
            binary_name: "hello_tool".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            binary_path,
        };
        let describe = ToolDescribeDocument {
            protocol_version: 1,
            name: "hello_tool".to_string(),
            description: "Example tool.".to_string(),
            params: BTreeMap::from([
                (
                    "rows".to_string(),
                    ToolDescribeParam {
                        kind: "array".to_string(),
                        required: true,
                        description: None,
                        default: None,
                    },
                ),
                (
                    "options".to_string(),
                    ToolDescribeParam {
                        kind: "object".to_string(),
                        required: false,
                        description: None,
                        default: Some(json!({ "delimiter": "," })),
                    },
                ),
            ]),
            result: ToolDescribeResult {
                kind: "string".to_string(),
                nullable: true,
                description: None,
            },
            resource_profile: ToolDescribeResourceProfile {
                network: "none".to_string(),
                filesystem_read: "none".to_string(),
                filesystem_write: "none".to_string(),
                subprocess: "none".to_string(),
                env_read: "none".to_string(),
                credential_access: "none".to_string(),
            },
            self_test: ToolDescribeSelfTest {
                supported: false,
                safe: false,
                description: None,
            },
            examples: ToolDescribeExamples {
                minimal_invoke: json!({ "protocol_version": 1, "params": { "rows": [] } }),
                full_invoke: json!({
                    "protocol_version": 1,
                    "params": {
                        "rows": [{ "customer": "Acme" }],
                        "options": { "delimiter": "," }
                    }
                }),
            },
        };

        validate_describe_document(&describe, &resolved)
            .expect("array/object params should be accepted");
    }
}
