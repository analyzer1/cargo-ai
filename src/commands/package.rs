//! Runtime behavior for `cargo ai package`.
use base64::Engine as _;
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use super::package_dependencies::{validate_dependency_declarations, PackageDependencies};

const PROJECT_METADATA_RELATIVE_PATH: &str = ".cargo-ai/project.toml";
const PROJECT_TOOLS_RELATIVE_PATH: &str = ".cargo-ai/tools";
const PACKAGE_MANIFEST_FILE_NAME: &str = "cargo-ai-package.toml";
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[derive(Clone, Debug, Default, Deserialize)]
struct ProjectMetadataDocument {
    #[serde(default)]
    project: Option<ProjectIdentityDocument>,
    #[serde(default)]
    runtime: Option<ProjectRuntimeDocument>,
    #[serde(default)]
    build: BTreeMap<String, BuildProfileDocument>,
    #[serde(default)]
    package: ProjectPackageDocument,
    #[serde(default)]
    package_dependencies: PackageDependencies,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProjectPackageDocument {
    #[serde(default)]
    permissions: ProjectPackagePermissionsDocument,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProjectPackagePermissionsDocument {
    #[serde(default = "default_project_subprocess_permission")]
    subprocess: String,
}

impl Default for ProjectPackagePermissionsDocument {
    fn default() -> Self {
        Self {
            subprocess: default_project_subprocess_permission(),
        }
    }
}

fn default_project_subprocess_permission() -> String {
    "blocked_without_explicit_grant".to_string()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ProjectIdentityDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(flatten)]
    extra: toml::Table,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct BuildProfileDocument {
    #[serde(default)]
    agent_definitions: Vec<String>,
    #[serde(default)]
    hatched_agents: Vec<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    assets: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct ProjectRuntimeDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data_root: Option<String>,
    #[serde(default)]
    defaults: Option<ProjectRuntimeDefaultsDocument>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ProjectRuntimeDefaultsDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inference_timeout_in_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_runtime_in_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_agent_depth: Option<u32>,
}

#[derive(Clone, Debug)]
struct PackageOutputRoot {
    path: PathBuf,
    explicit: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct AssembledPackage {
    pub root_path: PathBuf,
    pub manifest_project_name: Option<String>,
    pub manifest_project_version: Option<String>,
    pub manifest_value: serde_json::Value,
    pub archive_bytes: Vec<u8>,
    pub assembled_size_bytes: u64,
    pub archive_size_bytes: u64,
    pub estimated_publish_request_size_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PackageManifestDocument {
    format_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_version: Option<String>,
    profile: String,
    agent_definitions: Vec<String>,
    hatched_agents: Vec<String>,
    tools: Vec<String>,
    assets: Vec<String>,
    #[serde(default)]
    permissions: PackagePermissionProfileDocument,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PackagePermissionProfileDocument {
    package_payload: String,
    package_data: String,
    project_workspace: String,
    subprocess: String,
}

impl Default for PackagePermissionProfileDocument {
    fn default() -> Self {
        Self {
            package_payload: "read".to_string(),
            package_data: "read_write".to_string(),
            project_workspace: "explicit_grant_required".to_string(),
            subprocess: "blocked_without_explicit_grant".to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct GeneratedProjectMetadataDocument {
    format_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<ProjectIdentityDocument>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<ProjectRuntimeDocument>,
    tools: GeneratedProjectToolsPolicyDocument,
    package: ProjectPackageDocument,
    build: BTreeMap<String, BuildProfileDocument>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    package_dependencies: PackageDependencies,
}

#[derive(Clone, Debug, Serialize)]
struct GeneratedProjectToolsPolicyDocument {
    allow_global_fallback: bool,
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
struct ToolManifestDocument {
    tool_id: String,
    #[serde(default)]
    source: Option<ToolManifestSource>,
    #[serde(default)]
    binary: ToolManifestBinary,
}

#[derive(Clone, Debug)]
struct ProjectSourceToolContext {
    source_manifest_relative_path: String,
    source_root_relative_path: String,
    binary_name: String,
}

#[derive(Clone, Debug)]
struct LoadedProjectMetadata {
    project_identity: Option<ProjectIdentityDocument>,
    project_runtime: Option<ProjectRuntimeDocument>,
    build_profile: BuildProfileDocument,
    package_permissions: PackagePermissionProfileDocument,
    package_dependencies: PackageDependencies,
}

pub fn run(sub_m: &ArgMatches) -> bool {
    let profile_name = sub_m
        .get_one::<String>("profile")
        .map(String::as_str)
        .unwrap_or("default");
    match assemble_current_project_package(
        profile_name,
        sub_m.get_one::<String>("output_dir").map(String::as_str),
        sub_m.get_flag("force"),
        true,
    ) {
        Ok(assembled_package) => {
            println!("✓ Package assembled");
            if let Some(project_name) = assembled_package.manifest_project_name.as_deref() {
                println!("Project: {}", project_name);
            }
            if let Some(project_version) = assembled_package.manifest_project_version.as_deref() {
                println!("Version: {}", project_version);
            }
            println!("Profile: {}", profile_name);
            println!("Output:  {}", assembled_package.root_path.display());
            println!(
                "Package size on disk: {}",
                crate::commands::account::format_bytes(assembled_package.assembled_size_bytes)
            );
            println!(
                "Archive size:         {}",
                crate::commands::account::format_bytes(assembled_package.archive_size_bytes)
            );
            println!(
                "Estimated request:    {}",
                crate::commands::account::format_bytes(
                    assembled_package.estimated_publish_request_size_bytes
                )
            );
            true
        }
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

fn current_project_root() -> Result<Option<PathBuf>, String> {
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("Failed to inspect the current project directory: {error}"))?;
    crate::commands::package_dependencies::find_project_root(current_dir.as_path())
}

pub(crate) fn assemble_current_project_package(
    profile_name: &str,
    raw_output_dir: Option<&str>,
    force: bool,
    print_banner: bool,
) -> Result<AssembledPackage, String> {
    let project_root = current_project_root()?.ok_or_else(|| {
        "No Cargo AI project metadata was found from the current directory upward.".to_string()
    })?;
    let loaded_metadata = load_project_metadata(&project_root, profile_name)?;
    let output_root = resolve_package_output_root(&project_root, profile_name, raw_output_dir)?;

    if print_banner {
        println!("Packaging profile `{profile_name}`...");
        println!("Project: {}", project_root.display());
        println!("Output:  {}", output_root.path.display());
        println!();
    }

    let manifest = assemble_package_root(
        &project_root,
        profile_name,
        loaded_metadata.project_identity.as_ref(),
        loaded_metadata.project_runtime.as_ref(),
        &loaded_metadata.build_profile,
        &loaded_metadata.package_permissions,
        &loaded_metadata.package_dependencies,
        &output_root,
        force,
    )?;
    let manifest_value = serde_json::to_value(&manifest)
        .map_err(|error| format!("Failed to serialize package manifest JSON: {error}"))?;
    let assembled_size_bytes =
        crate::commands::account::directory_size_bytes(output_root.path.as_path())?;
    let archive_bytes =
        crate::commands::account::create_package_archive_bytes(output_root.path.as_path())?;
    let archive_size_bytes = u64::try_from(archive_bytes.len())
        .map_err(|_| "Package archive size exceeded supported limits.".to_string())?;
    let package_sha256 = crate::commands::account::sha256_hex(archive_bytes.as_slice());
    let package_size_bytes = i64::try_from(archive_bytes.len())
        .map_err(|_| "Package archive size exceeded supported limits.".to_string())?;
    let package_archive_base64 =
        base64::engine::general_purpose::STANDARD.encode(archive_bytes.as_slice());
    let estimated_publish_request_size_bytes =
        crate::infra_api::account::projects::estimate_publish_project_request_size(
            "__publish-size-estimate__",
            manifest
                .project_name
                .as_deref()
                .unwrap_or("unknown_project"),
            manifest.project_version.as_deref().unwrap_or("0.0.0"),
            manifest_value.clone(),
            package_sha256.as_str(),
            package_size_bytes,
            package_archive_base64.as_str(),
        )?;

    Ok(AssembledPackage {
        root_path: output_root.path.clone(),
        manifest_project_name: manifest.project_name.clone(),
        manifest_project_version: manifest.project_version.clone(),
        manifest_value,
        archive_bytes,
        assembled_size_bytes,
        archive_size_bytes,
        estimated_publish_request_size_bytes,
    })
}

fn load_project_metadata(
    project_root: &Path,
    profile_name: &str,
) -> Result<LoadedProjectMetadata, String> {
    let metadata_path = project_root.join(PROJECT_METADATA_RELATIVE_PATH);
    validate_project_source_path(project_root, &metadata_path, "Project metadata")?;
    let contents = fs::read_to_string(&metadata_path).map_err(|error| {
        format!(
            "Failed to read project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    super::runtime_data::uses_project_data(&contents)?;
    let mut metadata: ProjectMetadataDocument = toml::from_str(&contents).map_err(|error| {
        format!(
            "Failed to parse project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    let Some(profile) = metadata.build.get(profile_name).cloned() else {
        let mut available = metadata.build.keys().cloned().collect::<Vec<_>>();
        available.sort();
        let available_suffix = if available.is_empty() {
            " No `[build.<profile>]` sections were found.".to_string()
        } else {
            format!(" Available profiles: {}.", available.join(", "))
        };
        return Err(format!(
            "Build profile '{}' was not found in '{}'.{}",
            profile_name,
            metadata_path.display(),
            available_suffix
        ));
    };

    if profile.agent_definitions.is_empty()
        && profile.hatched_agents.is_empty()
        && profile.tools.is_empty()
        && profile.assets.is_empty()
    {
        return Err(format!(
            "Build profile '{}' in '{}' does not declare any `agent_definitions`, `hatched_agents`, `tools`, or `assets`.",
            profile_name,
            metadata_path.display()
        ));
    }
    validate_dependency_declarations(&metadata.package_dependencies)?;
    let package_permissions = package_permission_profile(&metadata.package)?;

    Ok(LoadedProjectMetadata {
        project_identity: normalize_project_identity(metadata.project.take()),
        project_runtime: metadata.runtime,
        build_profile: profile,
        package_permissions,
        package_dependencies: metadata.package_dependencies,
    })
}

fn package_permission_profile(
    package: &ProjectPackageDocument,
) -> Result<PackagePermissionProfileDocument, String> {
    let subprocess = package.permissions.subprocess.as_str();
    if !matches!(subprocess, "blocked_without_explicit_grant" | "allowed") {
        return Err(format!(
            "Unsupported package permission `package.permissions.subprocess = \"{subprocess}\"`. Expected `blocked_without_explicit_grant` or `allowed`."
        ));
    }

    Ok(PackagePermissionProfileDocument {
        subprocess: subprocess.to_string(),
        ..PackagePermissionProfileDocument::default()
    })
}

fn resolve_package_output_root(
    project_root: &Path,
    profile_name: &str,
    raw_output_dir: Option<&str>,
) -> Result<PackageOutputRoot, String> {
    let Some(raw_output_dir) = raw_output_dir else {
        return Ok(PackageOutputRoot {
            path: project_root
                .join("target")
                .join("cargo-ai")
                .join("package")
                .join(profile_name),
            explicit: false,
        });
    };

    let trimmed = raw_output_dir.trim();
    if trimmed.is_empty() {
        return Err("Output directory cannot be empty. Provide --output-dir <DIR>.".to_string());
    }

    let output_path = PathBuf::from(trimmed);
    let normalized_project_root = normalize_path(project_root);
    let normalized_output_root = normalize_against_current_dir(&output_path)?;
    if normalized_output_root == normalized_project_root {
        return Err(format!(
            "Output directory '{}' resolves to the current Cargo AI project root. Choose a nested package folder or omit --output-dir to use the default target path.",
            output_path.display()
        ));
    }
    if normalized_project_root.starts_with(&normalized_output_root) {
        return Err(format!(
            "Output directory '{}' resolves to an ancestor of the current Cargo AI project. Choose a project-contained package folder or omit --output-dir to use the default target path.",
            output_path.display()
        ));
    }

    Ok(PackageOutputRoot {
        path: output_path,
        explicit: true,
    })
}

fn assemble_package_root(
    project_root: &Path,
    profile_name: &str,
    project_identity: Option<&ProjectIdentityDocument>,
    project_runtime: Option<&ProjectRuntimeDocument>,
    build_profile: &BuildProfileDocument,
    package_permissions: &PackagePermissionProfileDocument,
    package_dependencies: &PackageDependencies,
    output_root: &PackageOutputRoot,
    force: bool,
) -> Result<PackageManifestDocument, String> {
    let build_profile = BuildProfileDocument {
        agent_definitions: dedupe_preserve_order(&build_profile.agent_definitions),
        hatched_agents: dedupe_preserve_order(&build_profile.hatched_agents),
        tools: dedupe_preserve_order(&build_profile.tools),
        assets: dedupe_preserve_order(&build_profile.assets),
    };

    validate_output_source_boundaries(project_root, &build_profile, output_root)?;
    prepare_output_root(project_root, output_root, force)?;

    for relative_path in dedupe_preserve_order(
        &build_profile
            .agent_definitions
            .iter()
            .cloned()
            .chain(build_profile.hatched_agents.iter().cloned())
            .collect::<Vec<_>>(),
    ) {
        copy_declared_path(
            project_root,
            relative_path.as_str(),
            output_root.path.as_path(),
            true,
        )?;
    }
    for relative_path in &build_profile.assets {
        copy_declared_path(
            project_root,
            relative_path,
            output_root.path.as_path(),
            false,
        )?;
    }

    let mut copied_source_roots = BTreeSet::new();
    for tool_name in &build_profile.tools {
        let context = load_project_source_tool_context(project_root, tool_name)?;
        if copied_source_roots.insert(context.source_root_relative_path.clone()) {
            copy_tool_source_root(
                project_root,
                context.source_root_relative_path.as_str(),
                output_root.path.as_path(),
            )?;
        }
        write_package_tool_manifest(
            output_root.path.as_path(),
            tool_name,
            context.source_manifest_relative_path.as_str(),
            context.binary_name.as_str(),
        )?;
    }

    write_generated_project_metadata(
        output_root.path.as_path(),
        project_identity,
        project_runtime,
        profile_name,
        &build_profile,
        package_permissions,
        package_dependencies,
    )?;

    let manifest = PackageManifestDocument {
        format_version: 1,
        project_name: project_identity.and_then(|project| project.name.clone()),
        project_version: project_identity.and_then(|project| project.version.clone()),
        profile: profile_name.to_string(),
        agent_definitions: build_profile.agent_definitions.clone(),
        hatched_agents: build_profile.hatched_agents.clone(),
        tools: build_profile.tools.clone(),
        assets: build_profile.assets.clone(),
        permissions: package_permissions.clone(),
    };
    write_package_manifest(output_root.path.as_path(), &manifest)?;

    Ok(manifest)
}

fn validate_output_source_boundaries(
    project_root: &Path,
    build_profile: &BuildProfileDocument,
    output_root: &PackageOutputRoot,
) -> Result<(), String> {
    super::runtime_data::validate_output_root(project_root, &output_root.path)?;
    let mut sources = vec![(
        "Project metadata".to_string(),
        project_root.join(PROJECT_METADATA_RELATIVE_PATH),
    )];

    for relative_path in dedupe_preserve_order(
        &build_profile
            .agent_definitions
            .iter()
            .cloned()
            .chain(build_profile.hatched_agents.iter().cloned())
            .collect::<Vec<_>>(),
    ) {
        let relative_path = super::runtime_data::validate_declared_input(relative_path.as_str())?;
        super::runtime_data::validate_source_tree(project_root, &relative_path, false)?;
        let source_path = project_root.join(relative_path);
        validate_project_source_path(project_root, &source_path, "Agent")?;
        sources.push(("Agent".to_string(), source_path));
    }

    for relative_path in &build_profile.assets {
        let relative_path = super::runtime_data::validate_declared_input(relative_path)?;
        super::runtime_data::validate_source_tree(project_root, &relative_path, false)?;
        let source_path = project_root.join(relative_path);
        validate_project_source_path(project_root, &source_path, "Asset")?;
        sources.push(("Asset".to_string(), source_path));
    }

    for tool_name in &build_profile.tools {
        let context = load_project_source_tool_context(project_root, tool_name)?;
        let source =
            super::runtime_data::validate_declared_input(&context.source_root_relative_path)?;
        super::runtime_data::validate_source_tree(project_root, &source, true)?;
        let tool_manifest_path = crate::commands::tools::project_tools_root(project_root)
            .join(tool_name)
            .join("tool.json");
        sources.push((format!("Tool '{tool_name}' metadata"), tool_manifest_path));
        sources.push((
            format!("Tool '{tool_name}' source"),
            project_root.join(context.source_root_relative_path),
        ));
    }

    let comparable_output = comparable_filesystem_path(output_root.path.as_path())?;
    for (label, source_path) in sources {
        let comparable_source = comparable_filesystem_path(source_path.as_path())?;
        if paths_overlap(&comparable_output, &comparable_source) {
            return Err(format!(
                "Package output path '{}' overlaps {label} source '{}'. Choose an output directory that is separate from every packaged source path.",
                output_root.path.display(),
                source_path.display()
            ));
        }
    }

    Ok(())
}

fn comparable_filesystem_path(path: &Path) -> Result<PathBuf, String> {
    let absolute_path = normalize_against_current_dir(path)?;
    let mut existing_ancestor = absolute_path.clone();
    let mut missing_components = Vec::new();

    loop {
        match fs::symlink_metadata(&existing_ancestor) {
            Ok(_) => {
                let mut comparable = fs::canonicalize(&existing_ancestor).map_err(|error| {
                    format!(
                        "Failed to resolve package path '{}': {}",
                        existing_ancestor.display(),
                        error
                    )
                })?;
                for component in missing_components.iter().rev() {
                    comparable.push(component);
                }
                return Ok(normalize_path(comparable.as_path()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = existing_ancestor.file_name().ok_or_else(|| {
                    format!(
                        "Package path '{}' has no existing filesystem ancestor.",
                        path.display()
                    )
                })?;
                missing_components.push(component.to_os_string());
                if !existing_ancestor.pop() {
                    return Err(format!(
                        "Package path '{}' has no existing filesystem ancestor.",
                        path.display()
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "Failed to inspect package path '{}': {}",
                    existing_ancestor.display(),
                    error
                ));
            }
        }
    }
}

fn paths_overlap(first: &Path, second: &Path) -> bool {
    first == second || first.starts_with(second) || second.starts_with(first)
}

fn prepare_output_root(
    project_root: &Path,
    output_root: &PackageOutputRoot,
    force: bool,
) -> Result<(), String> {
    ensure_output_path_ancestors_are_safe(&output_root.path, project_root)?;
    match fs::symlink_metadata(&output_root.path) {
        Ok(metadata) => {
            if metadata_is_link_like(&metadata) {
                return Err(format!(
                    "Package output path '{}' must not be a symbolic link or reparse point.",
                    output_root.path.display()
                ));
            }
            if output_root.explicit && !force {
                return Err(format!(
                    "Output directory '{}' already exists. Re-run with --force to replace it, or omit --output-dir to use the default target package path.",
                    output_root.path.display()
                ));
            }
            remove_existing_output_root(output_root.path.as_path(), &metadata)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Failed to inspect package output path '{}': {}",
                output_root.path.display(),
                error
            ));
        }
    }

    fs::create_dir_all(&output_root.path).map_err(|error| {
        format!(
            "Failed to create package output directory '{}': {}",
            output_root.path.display(),
            error
        )
    })?;
    ensure_output_path_ancestors_are_safe(&output_root.path, project_root)?;
    let metadata = fs::symlink_metadata(&output_root.path).map_err(|error| {
        format!(
            "Failed to inspect created package output directory '{}': {}",
            output_root.path.display(),
            error
        )
    })?;
    if metadata_is_link_like(&metadata) || !metadata.is_dir() {
        return Err(format!(
            "Package output path '{}' must be a real directory and not a symbolic link or reparse point.",
            output_root.path.display()
        ));
    }
    Ok(())
}

fn remove_existing_output_root(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(|error| {
            format!(
                "Failed to replace existing package output directory '{}': {}",
                path.display(),
                error
            )
        })?;
    } else if metadata.is_file() {
        fs::remove_file(path).map_err(|error| {
            format!(
                "Failed to replace existing package output file '{}': {}",
                path.display(),
                error
            )
        })?;
    } else {
        return Err(format!(
            "Package output path '{}' must be a regular file or directory before replacement.",
            path.display()
        ));
    }

    Ok(())
}

fn load_project_source_tool_context(
    project_root: &Path,
    tool_name: &str,
) -> Result<ProjectSourceToolContext, String> {
    crate::commands::tools::validate_tool_identifier(tool_name)?;
    let tool_manifest_path = crate::commands::tools::project_tools_root(project_root)
        .join(tool_name)
        .join("tool.json");
    let tool_manifest_metadata = match fs::symlink_metadata(&tool_manifest_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "Failed to inspect tool manifest '{}': {}",
                tool_manifest_path.display(),
                error
            ));
        }
    };
    if tool_manifest_metadata.is_none() {
        let machine_manifest_path = crate::commands::tools::machine_tools_root()
            .join(tool_name)
            .join("tool.json");
        if machine_manifest_path.exists() {
            return Err(format!(
                "Tool '{}' is only installed in Cargo AI Home. `cargo ai package` requires project-attached source-backed tools. Attach/install this tool into the current project first.",
                tool_name
            ));
        }
        return Err(format!(
            "Tool '{}' was not found in the current project's managed tool metadata. Add or attach/install it into the project before running `cargo ai package`.",
            tool_name
        ));
    }
    validate_project_source_path(project_root, &tool_manifest_path, "Tool manifest")?;

    let manifest_contents = fs::read_to_string(&tool_manifest_path).map_err(|error| {
        format!(
            "Failed to read tool manifest '{}': {}",
            tool_manifest_path.display(),
            error
        )
    })?;
    let manifest: ToolManifestDocument =
        serde_json::from_str(&manifest_contents).map_err(|error| {
            format!(
                "Failed to parse tool manifest '{}': {}",
                tool_manifest_path.display(),
                error
            )
        })?;
    if manifest.tool_id != tool_name {
        return Err(format!(
            "Tool manifest '{}' declares tool_id '{}', but the project references '{}'.",
            tool_manifest_path.display(),
            manifest.tool_id,
            tool_name
        ));
    }

    let source = manifest.source.ok_or_else(|| {
        format!(
            "Tool '{}' is not source-backed. `cargo ai package` requires project-attached source-backed tools only.",
            tool_name
        )
    })?;
    let source_manifest_relative_path =
        validate_project_relative_path(source.manifest_path.as_str(), "Tool source manifest")?;
    if source_manifest_relative_path
        .file_name()
        .and_then(|value| value.to_str())
        != Some("Cargo.toml")
    {
        return Err(format!(
            "Tool '{}' source manifest '{}' must point to a Cargo.toml file.",
            tool_name, source.manifest_path
        ));
    }

    let source_manifest_path = project_root.join(&source_manifest_relative_path);
    validate_project_source_path(project_root, &source_manifest_path, "Tool source manifest")?;

    let source_root = source_manifest_path.parent().ok_or_else(|| {
        format!(
            "Tool '{}' source manifest '{}' has no parent directory.",
            tool_name,
            source_manifest_path.display()
        )
    })?;
    if source_root == project_root {
        return Err(format!(
            "Tool '{}' source manifest '{}' resolves to the current project root. `cargo ai package` currently requires tool source to live in its own project-relative directory.",
            tool_name,
            source.manifest_path
        ));
    }

    let source_root_relative_path = source_root
        .strip_prefix(project_root)
        .map_err(|_| {
            format!(
                "Tool '{}' source directory '{}' is not project-relative.",
                tool_name,
                source_root.display()
            )
        })?
        .to_string_lossy()
        .to_string();

    Ok(ProjectSourceToolContext {
        source_manifest_relative_path: source_manifest_relative_path
            .to_string_lossy()
            .replace('\\', "/"),
        source_root_relative_path,
        binary_name: manifest
            .binary
            .default_name
            .unwrap_or_else(|| tool_name.to_string()),
    })
}

fn write_package_tool_manifest(
    package_root: &Path,
    tool_name: &str,
    source_manifest_relative_path: &str,
    binary_name: &str,
) -> Result<(), String> {
    let tool_dir = package_root
        .join(PROJECT_TOOLS_RELATIVE_PATH)
        .join(tool_name);
    fs::create_dir_all(&tool_dir).map_err(|error| {
        format!(
            "Failed to create packaged tool metadata directory '{}': {}",
            tool_dir.display(),
            error
        )
    })?;

    let manifest_path = tool_dir.join("tool.json");
    let manifest = serde_json::json!({
        "schema_version": 1,
        "tool_id": tool_name,
        "source": {
            "manifest_path": source_manifest_relative_path
        },
        "binary": {
            "default_name": binary_name
        },
        "artifacts": {}
    })
    .to_string();
    fs::write(&manifest_path, manifest).map_err(|error| {
        format!(
            "Failed to write packaged tool manifest '{}': {}",
            manifest_path.display(),
            error
        )
    })
}

fn write_generated_project_metadata(
    package_root: &Path,
    project_identity: Option<&ProjectIdentityDocument>,
    project_runtime: Option<&ProjectRuntimeDocument>,
    profile_name: &str,
    build_profile: &BuildProfileDocument,
    package_permissions: &PackagePermissionProfileDocument,
    package_dependencies: &PackageDependencies,
) -> Result<(), String> {
    let metadata_path = package_root.join(PROJECT_METADATA_RELATIVE_PATH);
    if let Some(parent) = metadata_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create generated metadata directory '{}': {}",
                parent.display(),
                error
            )
        })?;
    }

    let mut build = BTreeMap::new();
    build.insert(profile_name.to_string(), build_profile.clone());
    let document = GeneratedProjectMetadataDocument {
        format_version: 1,
        project: project_identity.cloned(),
        runtime: project_runtime.cloned(),
        tools: GeneratedProjectToolsPolicyDocument {
            allow_global_fallback: false,
        },
        package: ProjectPackageDocument {
            permissions: ProjectPackagePermissionsDocument {
                subprocess: package_permissions.subprocess.clone(),
            },
        },
        build,
        package_dependencies: package_dependencies.clone(),
    };
    let rendered = toml::to_string_pretty(&document)
        .map_err(|error| format!("Failed to render package project metadata TOML: {error}"))?;
    fs::write(&metadata_path, rendered).map_err(|error| {
        format!(
            "Failed to write generated project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })
}

fn normalize_project_identity(
    project_identity: Option<ProjectIdentityDocument>,
) -> Option<ProjectIdentityDocument> {
    let mut project_identity = project_identity?;
    project_identity.name = normalize_optional_metadata_text(project_identity.name.take());
    project_identity.version = normalize_optional_metadata_text(project_identity.version.take());

    if project_identity.name.is_none()
        && project_identity.version.is_none()
        && project_identity.extra.is_empty()
    {
        None
    } else {
        Some(project_identity)
    }
}

fn normalize_optional_metadata_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn write_package_manifest(
    package_root: &Path,
    manifest: &PackageManifestDocument,
) -> Result<(), String> {
    let manifest_path = package_root.join(PACKAGE_MANIFEST_FILE_NAME);
    let rendered = toml::to_string_pretty(manifest)
        .map_err(|error| format!("Failed to render package manifest TOML: {error}"))?;
    fs::write(&manifest_path, rendered).map_err(|error| {
        format!(
            "Failed to write package manifest '{}': {}",
            manifest_path.display(),
            error
        )
    })
}

fn copy_declared_path(
    project_root: &Path,
    relative_path: &str,
    package_root: &Path,
    require_json_file: bool,
) -> Result<(), String> {
    super::runtime_data::validate_declared_input(relative_path)?;
    let relative_path = validate_project_relative_path(
        relative_path,
        if require_json_file { "Agent" } else { "Asset" },
    )?;
    let source_path = project_root.join(&relative_path);
    let source_metadata = validate_project_source_path(
        project_root,
        &source_path,
        if require_json_file { "Agent" } else { "Asset" },
    )?;
    if require_json_file {
        if !source_metadata.is_file() {
            return Err(format!(
                "Agent path '{}' must point to a JSON file.",
                source_path.display()
            ));
        }
        let is_json = source_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        if !is_json {
            return Err(format!(
                "Agent path '{}' must point to a JSON file.",
                source_path.display()
            ));
        }
    }

    let dest_path = package_root.join(&relative_path);
    if source_metadata.is_dir() {
        copy_directory_recursive(project_root, source_path.as_path(), dest_path.as_path())
    } else {
        copy_file(project_root, source_path.as_path(), dest_path.as_path())
    }
}

fn copy_file(project_root: &Path, source: &Path, dest: &Path) -> Result<(), String> {
    let metadata = validate_project_source_path(project_root, source, "Packaged file")?;
    if !metadata.is_file() {
        return Err(format!(
            "Packaged path '{}' must be a regular file.",
            source.display()
        ));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create destination directory '{}': {}",
                parent.display(),
                error
            )
        })?;
    }
    fs::copy(source, dest).map_err(|error| {
        format!(
            "Failed to copy '{}' into '{}': {}",
            source.display(),
            dest.display(),
            error
        )
    })?;
    Ok(())
}

fn copy_tool_source_root(
    project_root: &Path,
    relative_path: &str,
    package_root: &Path,
) -> Result<(), String> {
    let relative_path = validate_project_relative_path(relative_path, "Tool source root")?;
    let source_path = project_root.join(&relative_path);
    let source_metadata =
        validate_project_source_path(project_root, &source_path, "Tool source root")?;
    if !source_metadata.is_dir() {
        return Err(format!(
            "Tool source root '{}' must point to a directory.",
            source_path.display()
        ));
    }

    let dest_path = package_root.join(relative_path);
    copy_directory_recursive_skipping_target(
        project_root,
        source_path.as_path(),
        dest_path.as_path(),
    )
}

fn copy_directory_recursive(project_root: &Path, source: &Path, dest: &Path) -> Result<(), String> {
    let metadata = validate_project_source_path(project_root, source, "Packaged directory")?;
    if !metadata.is_dir() {
        return Err(format!(
            "Packaged path '{}' must be a real directory.",
            source.display()
        ));
    }
    fs::create_dir_all(dest).map_err(|error| {
        format!(
            "Failed to create destination directory '{}': {}",
            dest.display(),
            error
        )
    })?;

    for entry in fs::read_dir(source).map_err(|error| {
        format!(
            "Failed to read directory '{}' while assembling package output: {}",
            source.display(),
            error
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to read directory entry under '{}': {}",
                source.display(),
                error
            )
        })?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if super::runtime_data::is_runtime_data(
            source_path
                .strip_prefix(project_root)
                .map_err(|error| error.to_string())?,
        ) {
            continue;
        }
        let metadata = validate_project_source_path(project_root, &source_path, "Packaged entry")?;
        if metadata.is_dir() {
            copy_directory_recursive(project_root, source_path.as_path(), dest_path.as_path())?;
        } else if metadata.is_file() {
            copy_file(project_root, source_path.as_path(), dest_path.as_path())?;
        } else {
            return Err(format!(
                "Packaged path '{}' must be a regular file or directory.",
                source_path.display()
            ));
        }
    }

    Ok(())
}

fn copy_directory_recursive_skipping_target(
    project_root: &Path,
    source: &Path,
    dest: &Path,
) -> Result<(), String> {
    let metadata = validate_project_source_path(project_root, source, "Tool source directory")?;
    if !metadata.is_dir() {
        return Err(format!(
            "Tool source path '{}' must be a real directory.",
            source.display()
        ));
    }
    fs::create_dir_all(dest).map_err(|error| {
        format!(
            "Failed to create destination directory '{}': {}",
            dest.display(),
            error
        )
    })?;

    for entry in fs::read_dir(source).map_err(|error| {
        format!(
            "Failed to read directory '{}' while assembling package output: {}",
            source.display(),
            error
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to read directory entry under '{}': {}",
                source.display(),
                error
            )
        })?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if super::runtime_data::is_runtime_data(
            source_path
                .strip_prefix(project_root)
                .map_err(|error| error.to_string())?,
        ) {
            continue;
        }
        let metadata =
            validate_project_source_path(project_root, &source_path, "Tool source entry")?;
        if metadata.is_dir()
            && entry
                .file_name()
                .to_str()
                .map(|name| name == "target")
                .unwrap_or(false)
        {
            continue;
        }
        if metadata.is_dir() {
            copy_directory_recursive_skipping_target(
                project_root,
                source_path.as_path(),
                dest_path.as_path(),
            )?;
        } else if metadata.is_file() {
            copy_file(project_root, source_path.as_path(), dest_path.as_path())?;
        } else {
            return Err(format!(
                "Tool source path '{}' must be a regular file or directory.",
                source_path.display()
            ));
        }
    }

    Ok(())
}

fn dedupe_preserve_order(values: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();

    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value.clone());
        }
    }

    deduped
}

fn validate_project_relative_path(raw_path: &str, label: &str) -> Result<PathBuf, String> {
    crate::commands::local_packages::normalize_portable_relative_path(
        raw_path,
        format!("{label} path").as_str(),
    )
}

fn validate_project_source_path(
    project_root: &Path,
    source_path: &Path,
    label: &str,
) -> Result<fs::Metadata, String> {
    let project_metadata = fs::symlink_metadata(project_root).map_err(|error| {
        format!(
            "Failed to inspect project root '{}' while validating {label}: {}",
            project_root.display(),
            error
        )
    })?;
    if metadata_is_link_like(&project_metadata) || !project_metadata.is_dir() {
        return Err(format!(
            "Project root '{}' must be a real directory and not a symbolic link or reparse point.",
            project_root.display()
        ));
    }
    let relative_path = source_path.strip_prefix(project_root).map_err(|_| {
        format!(
            "{label} '{}' is not inside project root '{}'.",
            source_path.display(),
            project_root.display()
        )
    })?;

    let mut current_path = project_root.to_path_buf();
    let mut source_metadata = project_metadata;
    for component in relative_path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(segment) => current_path.push(segment),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "{label} '{}' must stay inside project root '{}'.",
                    source_path.display(),
                    project_root.display()
                ));
            }
        }
        source_metadata = fs::symlink_metadata(&current_path).map_err(|error| {
            format!(
                "Failed to inspect {label} component '{}': {}",
                current_path.display(),
                error
            )
        })?;
        if metadata_is_link_like(&source_metadata) {
            return Err(format!(
                "{label} must not traverse symbolic link or reparse point '{}'.",
                current_path.display()
            ));
        }
    }

    let canonical_project_root = fs::canonicalize(project_root).map_err(|error| {
        format!(
            "Failed to resolve project root '{}': {}",
            project_root.display(),
            error
        )
    })?;
    let canonical_source_path = fs::canonicalize(source_path).map_err(|error| {
        format!(
            "Failed to resolve {label} '{}': {}",
            source_path.display(),
            error
        )
    })?;
    if !canonical_source_path.starts_with(&canonical_project_root) {
        return Err(format!(
            "{label} '{}' resolves outside project root '{}'.",
            source_path.display(),
            project_root.display()
        ));
    }
    Ok(source_metadata)
}

fn ensure_output_path_ancestors_are_safe(path: &Path, project_root: &Path) -> Result<(), String> {
    let absolute_path = normalize_against_current_dir(path)?;
    let parent = absolute_path.parent().ok_or_else(|| {
        format!(
            "Package output path '{}' must have a writable parent directory.",
            path.display()
        )
    })?;
    let trusted_boundaries = trusted_output_boundaries(project_root);
    inspect_existing_output_components(parent, &trusted_boundaries)?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Failed to create package output parent directory '{}': {}",
            parent.display(),
            error
        )
    })?;
    inspect_existing_output_components(parent, &trusted_boundaries)
}

fn inspect_existing_output_components(
    path: &Path,
    trusted_boundaries: &[PathBuf],
) -> Result<(), String> {
    let trusted_boundary = trusted_boundaries
        .iter()
        .filter(|boundary| path.starts_with(boundary))
        .max_by_key(|boundary| boundary.components().count());
    let (mut current_path, remaining_path) = match trusted_boundary {
        Some(boundary) => {
            let canonical_boundary = fs::canonicalize(boundary).map_err(|error| {
                format!(
                    "Failed to resolve trusted package output boundary '{}': {}",
                    boundary.display(),
                    error
                )
            })?;
            let remaining = path
                .strip_prefix(boundary)
                .map_err(|_| "Package output path escaped its trusted boundary.".to_string())?;
            (canonical_boundary, remaining)
        }
        None => (PathBuf::new(), path),
    };
    for component in remaining_path.components() {
        current_path.push(component.as_os_str());
        match fs::symlink_metadata(&current_path) {
            Ok(metadata) if metadata_is_link_like(&metadata) => {
                return Err(format!(
                    "Package output path must not traverse symbolic link or reparse point '{}'.",
                    current_path.display()
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!(
                    "Package output ancestor '{}' must be a real directory.",
                    current_path.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "Failed to inspect package output ancestor '{}': {}",
                    current_path.display(),
                    error
                ));
            }
        }
    }
    Ok(())
}

fn trusted_output_boundaries(project_root: &Path) -> Vec<PathBuf> {
    let mut boundaries = vec![
        normalize_path(project_root),
        normalize_path(std::env::temp_dir()),
    ];
    if let Ok(current_dir) = std::env::current_dir() {
        boundaries.push(normalize_path(current_dir));
    }
    boundaries
        .into_iter()
        .filter(|boundary| boundary.is_absolute() && boundary.exists())
        .collect()
}

#[cfg(windows)]
fn metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    windows_attributes_are_link_like(metadata.file_attributes())
}

#[cfg(windows)]
fn windows_attributes_are_link_like(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn normalize_against_current_dir(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(normalize_path(path));
    }

    let current_dir = std::env::current_dir().map_err(|error| {
        format!("Failed to resolve the current directory while validating --output-dir: {error}")
    })?;
    Ok(normalize_path(current_dir.join(path)))
}

fn normalize_path(path: impl AsRef<Path>) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.as_ref().components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::{
        assemble_package_root, load_project_metadata, load_project_source_tool_context,
        resolve_package_output_root, PackageManifestDocument, PackagePermissionProfileDocument,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(stem: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cargo-ai-package-test-{stem}-{nanos}"))
    }

    fn write_project_metadata(project_root: &PathBuf, body: &str) {
        let metadata_path = project_root.join(".cargo-ai/project.toml");
        fs::create_dir_all(
            metadata_path
                .parent()
                .expect("project metadata parent should exist"),
        )
        .expect("project metadata parent should be created");
        fs::write(metadata_path, body).expect("project metadata should be written");
    }

    fn write_source_tool_fixture(
        project_root: &PathBuf,
        tool_id: &str,
        manifest_relative_path: &str,
    ) {
        let source_manifest_path = project_root.join(manifest_relative_path);
        let source_root = source_manifest_path
            .parent()
            .expect("manifest parent should exist");
        fs::create_dir_all(source_root.join("src")).expect("tool source tree should be created");
        fs::write(
            &source_manifest_path,
            format!(
                "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                tool_id
            ),
        )
        .expect("source manifest should be written");
        fs::write(
            source_root.join("src/main.rs"),
            "fn main() { println!(\"hello\"); }\n",
        )
        .expect("tool source file should be written");

        let tool_dir = project_root.join(".cargo-ai/tools").join(tool_id);
        fs::create_dir_all(&tool_dir).expect("tool metadata dir should be created");
        fs::write(
            tool_dir.join("tool.json"),
            serde_json::json!({
                "schema_version": 1,
                "tool_id": tool_id,
                "source": {
                    "manifest_path": manifest_relative_path
                },
                "binary": {
                    "default_name": tool_id
                },
                "artifacts": {
                    "aarch64-apple-darwin": {
                        "path": format!("bin/aarch64-apple-darwin/{tool_id}")
                    }
                }
            })
            .to_string(),
        )
        .expect("tool manifest should be written");
    }

    #[test]
    fn package_tool_lookup_rejects_nonportable_ids_before_path_resolution() {
        let project_root = temp_dir("nonportable-tool-id");
        fs::create_dir_all(&project_root).expect("project root should exist");

        for invalid in ["foo/bar", r"foo\bar", "..", "C:foo"] {
            let error = load_project_source_tool_context(&project_root, invalid)
                .expect_err("non-portable package tool id should fail");
            assert!(
                error.contains("portable")
                    || error.contains("traversal")
                    || error.contains("drive-relative"),
                "unexpected error for {invalid:?}: {error}"
            );
        }

        let _ = fs::remove_dir_all(project_root);
    }

    fn write_machine_tool_fixture(root: &PathBuf, tool_id: &str) {
        let tool_dir = root.join("tools").join(tool_id);
        fs::create_dir_all(&tool_dir).expect("machine tool dir should be created");
        fs::write(
            tool_dir.join("tool.json"),
            serde_json::json!({
                "schema_version": 1,
                "tool_id": tool_id,
                "binary": {
                    "default_name": tool_id
                },
                "artifacts": {
                    "aarch64-apple-darwin": {
                        "path": tool_id
                    }
                }
            })
            .to_string(),
        )
        .expect("machine tool manifest should be written");
    }

    #[test]
    fn load_project_metadata_allows_missing_project_identity() {
        let project_root = temp_dir("no-project-identity");
        write_project_metadata(
            &project_root,
            r#"
format_version = 1

[build.default]
agent_definitions = ["agents/demo.json"]
"#,
        );

        let loaded_metadata =
            load_project_metadata(&project_root, "default").expect("profile should load");
        assert!(loaded_metadata.project_identity.is_none());
        assert_eq!(
            loaded_metadata.build_profile.agent_definitions,
            vec!["agents/demo.json".to_string()]
        );
        assert_eq!(
            loaded_metadata.package_permissions,
            PackagePermissionProfileDocument::default()
        );

        let _ = fs::remove_dir_all(&project_root);
    }

    #[test]
    fn load_project_metadata_rejects_invalid_package_permission_authoring() {
        for (stem, package_metadata, expected) in [
            (
                "unknown-package-field",
                "[package]\nunexpected = true\n",
                "unknown field `unexpected`",
            ),
            (
                "unknown-permission-field",
                "[package.permissions]\npackage_data = \"read_write\"\n",
                "unknown field `package_data`",
            ),
            (
                "unsupported-subprocess",
                "[package.permissions]\nsubprocess = \"unrestricted\"\n",
                "Unsupported package permission `package.permissions.subprocess = \"unrestricted\"`",
            ),
        ] {
            let project_root = temp_dir(stem);
            write_project_metadata(
                &project_root,
                format!(
                    "format_version = 1\n\n{package_metadata}\n[build.default]\nagent_definitions = [\"agents/demo.json\"]\n"
                )
                .as_str(),
            );

            let error = load_project_metadata(&project_root, "default")
                .expect_err("invalid package permission metadata should fail");
            assert!(
                error.contains(expected),
                "expected error containing {expected:?}, got {error:?}"
            );
            let _ = fs::remove_dir_all(project_root);
        }
    }

    #[test]
    fn assembles_source_package_from_selected_build_profile() {
        let project_root = temp_dir("assemble");
        fs::create_dir_all(project_root.join("agents")).expect("agents dir should be created");
        fs::create_dir_all(project_root.join("assets/prompts"))
            .expect("asset dir should be created");
        write_project_metadata(
            &project_root,
            r#"
format_version = 1

[project]
name = "hello_package"
version = "0.1.0"

[tools]
allow_global_fallback = true

[package.permissions]
subprocess = "allowed"

[package_dependencies.reports]
hosted_source_id = "source-reports"
version = ">=1.2, <2.0"

[build.default]
agent_definitions = ["agents/definition_only.json"]
hatched_agents = ["agents/hello_runner.json"]
tools = ["hello_tool"]
assets = ["assets/prompts/"]
"#,
        );
        fs::write(
            project_root.join("agents/definition_only.json"),
            "{\"name\":\"definition_only\"}\n",
        )
        .expect("agent definition should be written");
        fs::write(
            project_root.join("agents/hello_runner.json"),
            "{\"name\":\"hello_runner\"}\n",
        )
        .expect("hatched agent definition should be written");
        fs::write(
            project_root.join("assets/prompts/example.txt"),
            "example prompt\n",
        )
        .expect("asset should be written");
        write_source_tool_fixture(&project_root, "hello_tool", "tools/hello_tool/Cargo.toml");

        let loaded_metadata =
            load_project_metadata(&project_root, "default").expect("profile should load");
        let output_root = resolve_package_output_root(&project_root, "default", None)
            .expect("output root should resolve");
        let manifest = assemble_package_root(
            &project_root,
            "default",
            loaded_metadata.project_identity.as_ref(),
            loaded_metadata.project_runtime.as_ref(),
            &loaded_metadata.build_profile,
            &loaded_metadata.package_permissions,
            &loaded_metadata.package_dependencies,
            &output_root,
            false,
        )
        .expect("package should assemble");
        let allowed_permissions = PackagePermissionProfileDocument {
            subprocess: "allowed".to_string(),
            ..PackagePermissionProfileDocument::default()
        };

        assert_eq!(
            manifest,
            PackageManifestDocument {
                format_version: 1,
                project_name: Some("hello_package".to_string()),
                project_version: Some("0.1.0".to_string()),
                profile: "default".to_string(),
                agent_definitions: vec!["agents/definition_only.json".to_string()],
                hatched_agents: vec!["agents/hello_runner.json".to_string()],
                tools: vec!["hello_tool".to_string()],
                assets: vec!["assets/prompts/".to_string()],
                permissions: allowed_permissions.clone(),
            }
        );

        assert!(output_root
            .path
            .join("agents/definition_only.json")
            .exists());
        assert!(output_root.path.join("agents/hello_runner.json").exists());
        assert!(output_root.path.join("assets/prompts/example.txt").exists());
        assert!(output_root
            .path
            .join("tools/hello_tool/Cargo.toml")
            .exists());
        assert!(output_root
            .path
            .join("tools/hello_tool/src/main.rs")
            .exists());
        assert!(!output_root.path.join("tools/hello_tool/target").exists());
        assert!(output_root
            .path
            .join(".cargo-ai/tools/hello_tool/tool.json")
            .exists());
        assert!(!output_root
            .path
            .join(".cargo-ai/tools/hello_tool/bin")
            .exists());

        let generated_project = fs::read_to_string(output_root.path.join(".cargo-ai/project.toml"))
            .expect("generated project metadata should exist");
        assert!(generated_project.contains("[project]"));
        assert!(generated_project.contains("name = \"hello_package\""));
        assert!(generated_project.contains("version = \"0.1.0\""));
        assert!(generated_project.contains("allow_global_fallback = false"));
        assert!(generated_project.contains("[package.permissions]"));
        assert!(generated_project.contains("subprocess = \"allowed\""));
        assert!(generated_project.contains("[build.default]"));
        assert!(generated_project.contains("hatched_agents = [\"agents/hello_runner.json\"]"));
        assert!(generated_project.contains("[package_dependencies.reports]"));
        assert!(generated_project.contains("hosted_source_id = \"source-reports\""));
        assert!(generated_project.contains("version = \">=1.2, <2.0\""));

        let package_manifest: PackageManifestDocument = toml::from_str(
            &fs::read_to_string(output_root.path.join("cargo-ai-package.toml"))
                .expect("package manifest should exist"),
        )
        .expect("package manifest should parse");
        assert_eq!(package_manifest, manifest);
        assert_eq!(package_manifest.permissions, allowed_permissions);

        let generated_metadata = load_project_metadata(&output_root.path, "default")
            .expect("generated package project metadata should reload");
        assert_eq!(
            generated_metadata.package_permissions,
            package_manifest.permissions
        );

        let packaged_tool_manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(
                output_root
                    .path
                    .join(".cargo-ai/tools/hello_tool/tool.json"),
            )
            .expect("packaged tool manifest should exist"),
        )
        .expect("tool manifest should parse");
        assert_eq!(
            packaged_tool_manifest
                .get("source")
                .and_then(serde_json::Value::as_object)
                .and_then(|source| source.get("manifest_path"))
                .and_then(serde_json::Value::as_str),
            Some("tools/hello_tool/Cargo.toml")
        );
        assert_eq!(
            packaged_tool_manifest
                .get("artifacts")
                .and_then(serde_json::Value::as_object)
                .map(|artifacts| artifacts.len()),
            Some(0)
        );

        let _ = fs::remove_dir_all(&project_root);
    }

    #[test]
    fn package_fails_when_tool_exists_only_in_cargo_ai_home() {
        let _guard = crate::commands::runtime_actions::TEST_ENV_LOCK
            .lock()
            .expect("environment lock should not be poisoned");
        let original_cargo_ai_home = std::env::var_os("CARGO_AI_HOME");
        let cargo_ai_home = temp_dir("machine-home");
        std::env::set_var("CARGO_AI_HOME", &cargo_ai_home);

        let project_root = temp_dir("machine-only");
        write_project_metadata(
            &project_root,
            r#"
format_version = 1

[build.default]
tools = ["machine_only"]
"#,
        );
        write_machine_tool_fixture(&cargo_ai_home, "machine_only");

        let loaded_metadata =
            load_project_metadata(&project_root, "default").expect("profile should load");
        let output_root = resolve_package_output_root(&project_root, "default", None)
            .expect("output root should resolve");
        let error = assemble_package_root(
            &project_root,
            "default",
            loaded_metadata.project_identity.as_ref(),
            loaded_metadata.project_runtime.as_ref(),
            &loaded_metadata.build_profile,
            &loaded_metadata.package_permissions,
            &loaded_metadata.package_dependencies,
            &output_root,
            false,
        )
        .expect_err("machine-only tool should fail package assembly");
        assert!(error.contains("only installed in Cargo AI Home"));
        assert!(error.contains("requires project-attached source-backed tools"));

        match original_cargo_ai_home {
            Some(value) => std::env::set_var("CARGO_AI_HOME", value),
            None => std::env::remove_var("CARGO_AI_HOME"),
        }
        let _ = fs::remove_dir_all(&project_root);
        let _ = fs::remove_dir_all(&cargo_ai_home);
    }

    #[test]
    fn package_rejects_default_output_nested_in_declared_source() {
        let project_root = temp_dir("default-output-source-overlap");
        fs::create_dir_all(project_root.join("target"))
            .expect("declared target asset should exist");
        fs::write(project_root.join("target/sentinel.txt"), "source")
            .expect("source sentinel should be writable");
        write_project_metadata(
            &project_root,
            r#"
format_version = 1

[build.default]
assets = ["target"]
"#,
        );

        let loaded_metadata =
            load_project_metadata(&project_root, "default").expect("profile should load");
        let output_root = resolve_package_output_root(&project_root, "default", None)
            .expect("default output should resolve");
        let error = assemble_package_root(
            &project_root,
            "default",
            loaded_metadata.project_identity.as_ref(),
            loaded_metadata.project_runtime.as_ref(),
            &loaded_metadata.build_profile,
            &loaded_metadata.package_permissions,
            &loaded_metadata.package_dependencies,
            &output_root,
            false,
        )
        .expect_err("output nested in a copied directory must fail before assembly");

        assert!(error.contains("overlaps Asset source"));
        assert_eq!(
            fs::read_to_string(project_root.join("target/sentinel.txt"))
                .expect("declared source must remain intact"),
            "source"
        );
        assert!(!output_root.path.exists());
        let _ = fs::remove_dir_all(project_root);
    }

    #[test]
    fn package_rejects_explicit_output_that_would_delete_declared_source() {
        let project_root = temp_dir("explicit-output-source-overlap");
        fs::create_dir_all(project_root.join("assets"))
            .expect("declared asset directory should exist");
        fs::write(project_root.join("assets/sentinel.txt"), "source")
            .expect("source sentinel should be writable");
        write_project_metadata(
            &project_root,
            r#"
format_version = 1

[build.default]
assets = ["assets"]
"#,
        );

        let loaded_metadata =
            load_project_metadata(&project_root, "default").expect("profile should load");
        let explicit_output = project_root.join("assets");
        let output_root = resolve_package_output_root(
            &project_root,
            "default",
            Some(explicit_output.to_string_lossy().as_ref()),
        )
        .expect("explicit output should resolve before source preflight");
        let error = assemble_package_root(
            &project_root,
            "default",
            loaded_metadata.project_identity.as_ref(),
            loaded_metadata.project_runtime.as_ref(),
            &loaded_metadata.build_profile,
            &loaded_metadata.package_permissions,
            &loaded_metadata.package_dependencies,
            &output_root,
            true,
        )
        .expect_err("output equal to a copied directory must not delete the source");

        assert!(error.contains("overlaps Asset source"));
        assert_eq!(
            fs::read_to_string(project_root.join("assets/sentinel.txt"))
                .expect("declared source must remain intact"),
            "source"
        );
        let _ = fs::remove_dir_all(project_root);
    }

    #[cfg(unix)]
    #[test]
    fn package_rejects_linked_sources_and_output_ancestors() {
        use std::os::unix::fs::symlink;

        let project_root = temp_dir("linked-boundaries");
        let external_root = temp_dir("linked-boundaries-external");
        fs::create_dir_all(project_root.join("assets")).expect("assets root should exist");
        fs::create_dir_all(&external_root).expect("external root should exist");
        fs::write(external_root.join("secret.txt"), "outside")
            .expect("external fixture should be writable");
        write_project_metadata(
            &project_root,
            r#"
format_version = 1

[build.default]
assets = ["assets/linked.txt"]
"#,
        );
        symlink(
            external_root.join("secret.txt"),
            project_root.join("assets/linked.txt"),
        )
        .expect("linked asset should be created");
        let loaded_metadata =
            load_project_metadata(&project_root, "default").expect("profile should load");
        let output_root = resolve_package_output_root(&project_root, "default", None)
            .expect("default output should resolve");
        let source_error = assemble_package_root(
            &project_root,
            "default",
            loaded_metadata.project_identity.as_ref(),
            loaded_metadata.project_runtime.as_ref(),
            &loaded_metadata.build_profile,
            &loaded_metadata.package_permissions,
            &loaded_metadata.package_dependencies,
            &output_root,
            false,
        )
        .expect_err("linked project source should be rejected");
        assert!(source_error.contains("symbolic link"));
        assert_eq!(
            fs::read_to_string(external_root.join("secret.txt"))
                .expect("external source should remain readable"),
            "outside"
        );

        fs::remove_file(project_root.join("assets/linked.txt"))
            .expect("linked asset should be removable");
        fs::write(project_root.join("assets/linked.txt"), "inside")
            .expect("real asset should be writable");
        symlink(&external_root, project_root.join("linked-output"))
            .expect("linked output ancestor should be created");
        let explicit_output = project_root.join("linked-output/package");
        let output_root = resolve_package_output_root(
            &project_root,
            "default",
            Some(explicit_output.to_string_lossy().as_ref()),
        )
        .expect("explicit output should resolve lexically");
        let output_error = assemble_package_root(
            &project_root,
            "default",
            loaded_metadata.project_identity.as_ref(),
            loaded_metadata.project_runtime.as_ref(),
            &loaded_metadata.build_profile,
            &loaded_metadata.package_permissions,
            &loaded_metadata.package_dependencies,
            &output_root,
            true,
        )
        .expect_err("linked output ancestor should be rejected");
        assert!(output_error.contains("symbolic link"));
        assert!(!external_root.join("package").exists());

        let _ = fs::remove_file(project_root.join("linked-output"));
        let _ = fs::remove_dir_all(project_root);
        let _ = fs::remove_dir_all(external_root);
    }

    #[cfg(windows)]
    #[test]
    fn package_rejects_windows_reparse_attributes() {
        assert!(super::windows_attributes_are_link_like(
            super::FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert!(!super::windows_attributes_are_link_like(0));
    }
}
