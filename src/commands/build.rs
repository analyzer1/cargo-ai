//! Runtime behavior for `cargo ai build`.
use crate::agent_builder::build_target::CargoCompileProfile;
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

const PROJECT_METADATA_RELATIVE_PATH: &str = ".cargo-ai/project.toml";
const PROJECT_TOOLS_RELATIVE_PATH: &str = ".cargo-ai/tools";
const BUILD_MANIFEST_FILE_NAME: &str = "cargo-ai-build.toml";
#[derive(Clone, Debug, Default, Deserialize)]
struct ProjectMetadataDocument {
    #[serde(default)]
    runtime: Option<ProjectRuntimeDocument>,
    #[serde(default)]
    build: BTreeMap<String, BuildProfileDocument>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
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
struct ProjectRuntimeDefaultsDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inference_timeout_in_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_runtime_in_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_agent_depth: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
struct GeneratedProjectMetadataDocument {
    format_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<ProjectRuntimeDocument>,
    tools: GeneratedProjectToolsPolicyDocument,
}

#[derive(Clone, Debug, Serialize)]
struct GeneratedProjectToolsPolicyDocument {
    allow_global_fallback: bool,
}

#[derive(Clone, Debug)]
struct BuildOutputRoot {
    path: PathBuf,
    explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HatchedAgentEntry {
    relative_path: String,
    output_name: String,
}

#[derive(Clone, Debug, Default)]
struct LoadedProjectMetadata {
    project_runtime: Option<ProjectRuntimeDocument>,
    build_profile: BuildProfileDocument,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct BuildManifestDocument {
    format_version: u32,
    profile: String,
    cargo_compile_profile: String,
    target: String,
    agent_definitions: Vec<String>,
    hatched_agents: Vec<BuildManifestHatchedAgent>,
    tools: Vec<String>,
    assets: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct BuildManifestHatchedAgent {
    source: String,
    binary: String,
}

pub fn run(sub_m: &ArgMatches) -> bool {
    let project_root = match current_project_root() {
        Some(root) => root,
        None => {
            eprintln!(
                "x No Cargo AI project metadata was found from the current directory upward."
            );
            return false;
        }
    };
    let profile_name = sub_m
        .get_one::<String>("profile")
        .map(String::as_str)
        .unwrap_or("default");
    let build_target = match crate::agent_builder::build_target::BuildTarget::from_cli(
        sub_m.get_one::<String>("target").map(String::as_str),
    ) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("x {error}");
            return false;
        }
    };
    let loaded_metadata = match load_project_metadata(&project_root, profile_name) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("x {error}");
            return false;
        }
    };
    let output_root = match resolve_build_output_root(
        &project_root,
        profile_name,
        &build_target,
        sub_m.get_one::<String>("output_dir").map(String::as_str),
    ) {
        Ok(output_root) => output_root,
        Err(error) => {
            eprintln!("x {error}");
            return false;
        }
    };

    println!("Building profile `{profile_name}`...");
    println!("Cargo profile: {}", CargoCompileProfile::Release.name());
    println!("Project: {}", project_root.display());
    println!("Target:  {}", build_target.cache_key_target());
    println!("Output:  {}", output_root.path.display());
    println!();

    match assemble_build_root(
        &project_root,
        profile_name,
        &loaded_metadata,
        &build_target,
        &output_root,
        sub_m.get_flag("force"),
    ) {
        Ok(manifest) => {
            println!("✓ Build assembled");
            println!("Build profile: {}", manifest.profile);
            println!("Cargo profile: {}", manifest.cargo_compile_profile);
            println!("Target:  {}", manifest.target);
            println!("Output:  {}", output_root.path.display());
            if !manifest.hatched_agents.is_empty() {
                println!("Binaries:");
                for agent in manifest.hatched_agents {
                    println!("- {} -> {}", agent.source, agent.binary);
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

fn current_project_root() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .and_then(|dir| crate::commands::tools::maybe_find_project_root(dir.as_path()))
}

fn load_project_metadata(
    project_root: &Path,
    profile_name: &str,
) -> Result<LoadedProjectMetadata, String> {
    let metadata_path = super::runtime_data::confined_path(
        project_root,
        Path::new(PROJECT_METADATA_RELATIVE_PATH),
        "Project metadata",
    )?;
    let contents = fs::read_to_string(&metadata_path).map_err(|error| {
        format!(
            "Failed to read project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    super::runtime_data::uses_project_data(&contents)?;
    let metadata: ProjectMetadataDocument = toml::from_str(&contents).map_err(|error| {
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

    Ok(LoadedProjectMetadata {
        project_runtime: metadata.runtime,
        build_profile: profile,
    })
}

fn resolve_build_output_root(
    project_root: &Path,
    profile_name: &str,
    build_target: &crate::agent_builder::build_target::BuildTarget,
    raw_output_dir: Option<&str>,
) -> Result<BuildOutputRoot, String> {
    let Some(raw_output_dir) = raw_output_dir else {
        return Ok(BuildOutputRoot {
            path: project_root
                .join("target")
                .join("cargo-ai")
                .join("build")
                .join(profile_name)
                .join(build_target.cache_key_target())
                .join(CargoCompileProfile::Release.name()),
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
            "Output directory '{}' resolves to the current Cargo AI project root. Choose a nested build folder or omit --output-dir to use the default target path.",
            output_path.display()
        ));
    }

    Ok(BuildOutputRoot {
        path: output_path,
        explicit: true,
    })
}

fn assemble_build_root(
    project_root: &Path,
    profile_name: &str,
    loaded_metadata: &LoadedProjectMetadata,
    build_target: &crate::agent_builder::build_target::BuildTarget,
    output_root: &BuildOutputRoot,
    force: bool,
) -> Result<BuildManifestDocument, String> {
    let build_profile = &loaded_metadata.build_profile;
    let agent_definitions = dedupe_preserve_order(&build_profile.agent_definitions);
    let hatched_agents = resolve_hatched_agents(project_root, &build_profile.hatched_agents)?;
    let tools = dedupe_preserve_order(&build_profile.tools);
    let assets = dedupe_preserve_order(&build_profile.assets);

    validate_build_input_boundaries(project_root, build_profile, output_root)?;
    prepare_output_root(output_root, force)?;
    write_generated_project_metadata(
        output_root.path.as_path(),
        loaded_metadata.project_runtime.as_ref(),
    )?;

    for tool_name in &tools {
        validate_tool_attached_to_project(project_root, tool_name)?;
        materialize_build_tool(
            project_root,
            tool_name,
            build_target,
            output_root.path.as_path(),
        )?;
    }

    let tool_resolver = crate::commands::tools::ToolResolver::new(
        Some(output_root.path.clone()),
        build_target.cache_key_target(),
    );
    let target_platform = platform_label_for_target(build_target.cache_key_target());

    for agent_path in dedupe_preserve_order(
        &agent_definitions
            .iter()
            .cloned()
            .chain(
                hatched_agents
                    .iter()
                    .map(|entry| entry.relative_path.clone()),
            )
            .collect::<Vec<_>>(),
    ) {
        audit_agent_definition(
            project_root,
            agent_path.as_str(),
            &tool_resolver,
            target_platform,
        )?;
    }

    for relative_path in &agent_definitions {
        copy_declared_path(
            project_root,
            relative_path,
            output_root.path.as_path(),
            true,
        )?;
    }
    for relative_path in &assets {
        copy_declared_path(
            project_root,
            relative_path,
            output_root.path.as_path(),
            false,
        )?;
    }
    for agent in &hatched_agents {
        hatch_agent_into_build_root(
            project_root,
            agent,
            build_target,
            output_root.path.as_path(),
        )?;
    }

    let manifest = BuildManifestDocument {
        format_version: 1,
        profile: profile_name.to_string(),
        cargo_compile_profile: CargoCompileProfile::Release.name().to_string(),
        target: build_target.cache_key_target().to_string(),
        agent_definitions,
        hatched_agents: hatched_agents
            .iter()
            .map(|agent| BuildManifestHatchedAgent {
                source: agent.relative_path.clone(),
                binary: build_target
                    .exported_binary_path(output_root.path.as_path(), agent.output_name.as_str())
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(agent.output_name.as_str())
                    .to_string(),
            })
            .collect(),
        tools,
        assets,
    };
    write_build_manifest(output_root.path.as_path(), &manifest)?;

    Ok(manifest)
}

fn validate_build_input_boundaries(
    project_root: &Path,
    profile: &BuildProfileDocument,
    output: &BuildOutputRoot,
) -> Result<(), String> {
    let resolved_output = super::runtime_data::validate_output_root(project_root, &output.path)?;
    let mut sources = vec![PathBuf::from(PROJECT_METADATA_RELATIVE_PATH)];
    for raw in profile
        .agent_definitions
        .iter()
        .chain(&profile.hatched_agents)
        .chain(&profile.assets)
    {
        let path = super::runtime_data::validate_declared_input(raw)?;
        super::add::guidance::packaging::validate_declared(&path)?;
        super::runtime_data::validate_source_tree(project_root, &path, false)?;
        sources.push(path);
    }
    for tool in &profile.tools {
        validate_tool_attached_to_project(project_root, tool)?;
        let metadata = PathBuf::from(PROJECT_TOOLS_RELATIVE_PATH)
            .join(tool)
            .join("tool.json");
        let checked = super::runtime_data::confined_path(project_root, &metadata, "Tool metadata")?;
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(checked).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if let Some(manifest) = value
            .pointer("/source/manifest_path")
            .and_then(serde_json::Value::as_str)
        {
            let manifest = super::runtime_data::validate_declared_input(manifest)?;
            let source = manifest
                .parent()
                .ok_or("Tool manifest must have a source directory")?;
            super::runtime_data::validate_declared_input(
                source.to_str().ok_or("Tool source path must be Unicode")?,
            )?;
            super::runtime_data::validate_source_tree(project_root, source, true)?;
            sources.push(source.to_path_buf());
        }
        sources.push(metadata);
    }
    let output = resolved_output;
    for relative in sources {
        let source = fs::canonicalize(project_root.join(relative))
            .map_err(|error| format!("Failed to resolve build source: {error}"))?;
        if output.starts_with(&source) || source.starts_with(&output) {
            return Err(format!(
                "Build output '{}' overlaps source '{}'",
                output.display(),
                source.display()
            ));
        }
    }
    Ok(())
}

fn prepare_output_root(output_root: &BuildOutputRoot, force: bool) -> Result<(), String> {
    if output_root.path.exists() {
        if output_root.explicit && !force {
            return Err(format!(
                "Output directory '{}' already exists. Re-run with --force to replace it, or omit --output-dir to use the default target build path.",
                output_root.path.display()
            ));
        }

        remove_existing_output_root(output_root.path.as_path())?;
    }

    fs::create_dir_all(&output_root.path).map_err(|error| {
        format!(
            "Failed to create build output directory '{}': {}",
            output_root.path.display(),
            error
        )
    })?;
    Ok(())
}

fn remove_existing_output_root(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|error| {
            format!(
                "Failed to replace existing build output directory '{}': {}",
                path.display(),
                error
            )
        })?;
    } else {
        fs::remove_file(path).map_err(|error| {
            format!(
                "Failed to replace existing build output file '{}': {}",
                path.display(),
                error
            )
        })?;
    }

    Ok(())
}

fn write_generated_project_metadata(
    build_root: &Path,
    project_runtime: Option<&ProjectRuntimeDocument>,
) -> Result<(), String> {
    let metadata_path = build_root.join(PROJECT_METADATA_RELATIVE_PATH);
    if let Some(parent) = metadata_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create generated metadata directory '{}': {}",
                parent.display(),
                error
            )
        })?;
    }
    let document = GeneratedProjectMetadataDocument {
        format_version: 1,
        runtime: project_runtime.cloned(),
        tools: GeneratedProjectToolsPolicyDocument {
            allow_global_fallback: false,
        },
    };
    let rendered = toml::to_string_pretty(&document)
        .map_err(|error| format!("Failed to render generated project metadata TOML: {error}"))?;
    fs::write(&metadata_path, rendered).map_err(|error| {
        format!(
            "Failed to write generated project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })
}

fn validate_tool_attached_to_project(project_root: &Path, tool_name: &str) -> Result<(), String> {
    let project_manifest_path = crate::commands::tools::project_tools_root(project_root)
        .join(tool_name)
        .join("tool.json");
    if project_manifest_path.exists() {
        return Ok(());
    }

    let machine_manifest_path = crate::commands::tools::machine_tools_root()
        .join(tool_name)
        .join("tool.json");
    if machine_manifest_path.exists() {
        return Err(format!(
            "Tool '{}' is only installed in Cargo AI Home. `cargo ai build` requires project-attached tools. Rebuild or attach/install this tool into the current project first.",
            tool_name
        ));
    }

    Err(format!(
        "Tool '{}' was not found in the current project's managed tool metadata. Add or attach/install it into the project before running `cargo ai build`.",
        tool_name
    ))
}

fn materialize_build_tool(
    project_root: &Path,
    tool_name: &str,
    build_target: &crate::agent_builder::build_target::BuildTarget,
    build_root: &Path,
) -> Result<(), String> {
    let resolved = crate::commands::tools::build_source_tool(
        tool_name,
        build_target,
        crate::commands::tools::ToolScope::Project,
        project_root,
        CargoCompileProfile::Release,
    )?;
    let artifact_relative_path = PathBuf::from("bin")
        .join(build_target.cache_key_target())
        .join(CargoCompileProfile::Release.name())
        .join(resolved.binary_name.as_str());
    let output_tool_dir = build_root.join(PROJECT_TOOLS_RELATIVE_PATH).join(tool_name);
    let artifact_path = output_tool_dir.join(&artifact_relative_path);
    if let Some(parent) = artifact_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create build tool artifact directory '{}': {}",
                parent.display(),
                error
            )
        })?;
    }
    fs::copy(&resolved.binary_path, &artifact_path).map_err(|error| {
        format!(
            "Failed to copy built tool binary into '{}': {}",
            artifact_path.display(),
            error
        )
    })?;

    let manifest_path = output_tool_dir.join("tool.json");
    let manifest = serde_json::json!({
        "schema_version": 1,
        "tool_id": tool_name,
        "binary": {
            "default_name": resolved.binary_name
        },
        "artifacts": {
            build_target.cache_key_target(): {
                "path": artifact_relative_path.to_string_lossy()
            }
        }
    })
    .to_string();
    fs::write(&manifest_path, manifest).map_err(|error| {
        format!(
            "Failed to write build tool manifest '{}': {}",
            manifest_path.display(),
            error
        )
    })
}

fn audit_agent_definition(
    project_root: &Path,
    relative_path: &str,
    resolver: &crate::commands::tools::ToolResolver,
    target_platform: Option<&str>,
) -> Result<(), String> {
    let source_path = resolve_agent_json_path(project_root, relative_path, "Agent")?;
    let definition = crate::runtime_definition::RuntimeAgentDefinition::load_from_path(
        &source_path,
    )
    .map_err(|error| {
        format!(
            "Failed to load agent definition '{}': {}",
            source_path.display(),
            error
        )
    })?;

    crate::commands::tools::audit_actions_for_tools(
        &definition.actions(),
        resolver,
        target_platform,
    )
    .map(|_| ())
    .map_err(|error| {
        format!(
            "Agent definition '{}' failed build-time tool audit: {}",
            relative_path, error
        )
    })
}

fn resolve_hatched_agents(
    project_root: &Path,
    raw_paths: &[String],
) -> Result<Vec<HatchedAgentEntry>, String> {
    let mut entries = Vec::new();
    let mut seen_paths = BTreeSet::new();
    let mut output_names = BTreeMap::<String, String>::new();

    for raw_path in raw_paths {
        if !seen_paths.insert(raw_path.clone()) {
            continue;
        }

        let source_path = resolve_agent_json_path(project_root, raw_path, "Hatched agent")?;
        let output_name = derive_agent_output_name(raw_path)?;
        if let Some(existing) = output_names.get(output_name.as_str()) {
            if existing != raw_path {
                return Err(format!(
                    "Hatched agent paths '{}' and '{}' both resolve to output binary '{}'. Rename one source file or keep only one entry in `hatched_agents`.",
                    existing, raw_path, output_name
                ));
            }
        } else {
            output_names.insert(output_name.clone(), raw_path.clone());
        }

        if !source_path.exists() {
            return Err(format!(
                "Hatched agent definition '{}' was not found.",
                source_path.display()
            ));
        }

        entries.push(HatchedAgentEntry {
            relative_path: raw_path.clone(),
            output_name,
        });
    }

    Ok(entries)
}

fn hatch_agent_into_build_root(
    project_root: &Path,
    agent: &HatchedAgentEntry,
    build_target: &crate::agent_builder::build_target::BuildTarget,
    build_root: &Path,
) -> Result<(), String> {
    let source_path =
        resolve_agent_json_path(project_root, agent.relative_path.as_str(), "Hatched agent")?;
    let file_contents = fs::read_to_string(&source_path).map_err(|error| {
        format!(
            "Failed to read hatched agent definition '{}': {}",
            source_path.display(),
            error
        )
    })?;

    let _agent_lock = crate::agent_builder::lock::try_acquire_agent_lock(
        agent.output_name.as_str(),
    )
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            format!(
                "Agent '{}' is already running a hatch/build operation in another process.",
                agent.output_name
            )
        } else {
            format!(
                "Failed to acquire lock for agent '{}': {}",
                agent.output_name, error
            )
        }
    })?;

    if crate::agent_builder::agent_workspace_path(agent.output_name.as_str()).exists() {
        crate::agent_builder::cleanup::delete_agent_workspace(agent.output_name.as_str()).map_err(
            |error| {
                format!(
                    "Failed to reset existing internal workspace for agent '{}': {}",
                    agent.output_name, error
                )
            },
        )?;
    }

    let warmed_template =
        crate::agent_builder::template_cache::ensure_warmed_template_with_prepare_hook(
            build_target,
            CargoCompileProfile::Release,
            || {},
        )
        .map_err(|error| format!("Failed to prepare warmed template: {error}"))?;

    crate::agent_builder::project::create_new_agent_project(
        &warmed_template.path,
        agent.output_name.as_str(),
        Ok(file_contents),
    )
    .map_err(|error| {
        format!(
            "Failed to create build workspace for agent '{}': {}",
            agent.output_name, error
        )
    })?;

    let shared_target_dir = warmed_template.path.join("target");
    let build_result = crate::agent_builder::build::build_agent_project(
        agent.output_name.as_str(),
        build_target,
        Some(shared_target_dir.as_path()),
    )
    .map_err(|error| {
        format!(
            "Failed to build agent '{}' for target '{}': {}",
            agent.output_name,
            build_target.cache_key_target(),
            error
        )
    });

    if build_result.is_err() {
        let _ = crate::agent_builder::cleanup::delete_agent_workspace(agent.output_name.as_str());
        return build_result.map(|_| ());
    }

    let export_result = crate::agent_builder::export::export_binary(
        agent.output_name.as_str(),
        true,
        build_target,
        Some(build_root),
        Some(warmed_template.path.as_path()),
    )
    .map_err(|error| {
        format!(
            "Failed to export built agent '{}' into '{}': {}",
            agent.output_name,
            build_root.display(),
            error
        )
    });
    let _ = crate::agent_builder::cleanup::delete_agent_workspace(agent.output_name.as_str());
    export_result.map(|_| ())
}

fn copy_declared_path(
    project_root: &Path,
    relative_path: &str,
    build_root: &Path,
    require_json_file: bool,
) -> Result<(), String> {
    super::runtime_data::validate_declared_input(relative_path)?;
    super::add::guidance::packaging::validate_declared(Path::new(relative_path))?;
    validate_project_relative_path(
        relative_path,
        if require_json_file { "Agent" } else { "Asset" },
    )?;
    let source_path =
        super::runtime_data::confined_path(project_root, Path::new(relative_path), "Build input")?;
    if !source_path.exists() {
        return Err(format!(
            "{} path '{}' was not found in the current project.",
            if require_json_file { "Agent" } else { "Asset" },
            source_path.display()
        ));
    }
    if require_json_file {
        if !source_path.is_file() {
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

    let dest_path = build_root.join(relative_path);
    if source_path.is_dir() {
        copy_directory_recursive(
            project_root,
            source_path.as_path(),
            dest_path.as_path(),
            super::add::guidance::packaging::is_bundle_path(Path::new(relative_path)),
        )
    } else {
        copy_file(project_root, source_path.as_path(), dest_path.as_path())
    }
}

fn copy_file(project_root: &Path, source: &Path, dest: &Path) -> Result<(), String> {
    super::runtime_data::confined_path(
        project_root,
        source
            .strip_prefix(project_root)
            .map_err(|error| error.to_string())?,
        "Built file",
    )?;
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

fn copy_directory_recursive(
    project_root: &Path,
    source: &Path,
    dest: &Path,
    declared_bundle: bool,
) -> Result<(), String> {
    super::runtime_data::confined_path(
        project_root,
        source
            .strip_prefix(project_root)
            .map_err(|error| error.to_string())?,
        "Build directory",
    )?;
    fs::create_dir_all(dest).map_err(|error| {
        format!(
            "Failed to create destination directory '{}': {}",
            dest.display(),
            error
        )
    })?;

    for entry in fs::read_dir(source).map_err(|error| {
        format!(
            "Failed to read directory '{}' while assembling build output: {}",
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
        if super::add::guidance::packaging::skip_entry(project_root, &source_path, declared_bundle)?
        {
            continue;
        }
        super::runtime_data::confined_path(
            project_root,
            source_path
                .strip_prefix(project_root)
                .map_err(|error| error.to_string())?,
            "Build entry",
        )?;
        if source_path.is_dir() {
            copy_directory_recursive(
                project_root,
                source_path.as_path(),
                dest_path.as_path(),
                declared_bundle,
            )?;
        } else {
            copy_file(project_root, source_path.as_path(), dest_path.as_path())?;
        }
    }

    Ok(())
}

fn write_build_manifest(build_root: &Path, manifest: &BuildManifestDocument) -> Result<(), String> {
    let manifest_path = build_root.join(BUILD_MANIFEST_FILE_NAME);
    let rendered = toml::to_string_pretty(manifest)
        .map_err(|error| format!("Failed to render build manifest TOML: {error}"))?;
    fs::write(&manifest_path, rendered).map_err(|error| {
        format!(
            "Failed to write build manifest '{}': {}",
            manifest_path.display(),
            error
        )
    })
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

fn resolve_agent_json_path(
    project_root: &Path,
    relative_path: &str,
    label: &str,
) -> Result<PathBuf, String> {
    validate_project_relative_path(relative_path, label)?;
    let path = project_root.join(relative_path);
    if !path.exists() {
        return Err(format!("{label} path '{}' was not found.", path.display()));
    }
    if !path.is_file() {
        return Err(format!(
            "{label} path '{}' must point to a JSON file.",
            path.display()
        ));
    }
    let is_json = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if !is_json {
        return Err(format!(
            "{label} path '{}' must point to a JSON file.",
            path.display()
        ));
    }
    Ok(path)
}

fn derive_agent_output_name(relative_path: &str) -> Result<String, String> {
    let stem = Path::new(relative_path)
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            format!(
                "Unable to derive an agent output name from '{}'. Use a JSON filename with a valid stem.",
                relative_path
            )
        })?;
    if stem.is_empty()
        || !stem
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(format!(
            "Derived agent output name '{}' from '{}' is invalid. Use only letters, numbers, '-' or '_' in the JSON filename stem.",
            stem, relative_path
        ));
    }

    Ok(stem.to_string())
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
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "{label} path must stay at the current level or below; parent traversal (`..`) is not allowed."
        ));
    }
    Ok(())
}

fn platform_label_for_target(target: &str) -> Option<&'static str> {
    if target.contains("apple-darwin") {
        Some("macos")
    } else if target.contains("windows") {
        Some("windows")
    } else if target.contains("linux") {
        Some("linux")
    } else {
        None
    }
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
        dedupe_preserve_order, derive_agent_output_name, normalize_path, platform_label_for_target,
        BuildProfileDocument, ProjectMetadataDocument,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn parses_build_profile_shape_from_project_metadata() {
        let parsed: ProjectMetadataDocument = toml::from_str(
            r#"
format_version = 1

[build.default]
agent_definitions = ["agents/demo.json"]
hatched_agents = ["agents/cli.json"]
tools = ["hello_tool"]
assets = ["assets/prompts/"]
"#,
        )
        .expect("metadata should parse");

        let profile = parsed
            .build
            .get("default")
            .expect("default profile should exist");
        assert_eq!(
            profile,
            &BuildProfileDocument {
                agent_definitions: vec!["agents/demo.json".to_string()],
                hatched_agents: vec!["agents/cli.json".to_string()],
                tools: vec!["hello_tool".to_string()],
                assets: vec!["assets/prompts/".to_string()],
            }
        );
    }

    #[test]
    fn default_output_separates_assembly_and_cargo_profiles() {
        let target =
            crate::agent_builder::build_target::BuildTarget::from_cli(Some("aarch64-apple-darwin"))
                .unwrap();
        let project = std::env::current_dir().unwrap().join("fixture-project");
        for assembly in ["default", "release"] {
            let output =
                super::resolve_build_output_root(&project, assembly, &target, None).unwrap();
            assert_eq!(
                output.path,
                project
                    .join("target/cargo-ai/build")
                    .join(assembly)
                    .join("aarch64-apple-darwin/release")
            );
            assert!(!output.explicit);
        }
        let explicit =
            super::resolve_build_output_root(&project, "release", &target, Some("chosen-output"))
                .unwrap();
        assert_eq!(explicit.path, PathBuf::from("chosen-output"));
        assert!(explicit.explicit);
    }

    #[test]
    fn derives_agent_output_name_from_json_stem() {
        assert_eq!(
            derive_agent_output_name("agents/report_runner.json").expect("name should derive"),
            "report_runner".to_string()
        );
    }

    #[test]
    fn normalizes_relative_segments() {
        assert_eq!(
            normalize_path(Path::new("/tmp/demo/./build/../dist")),
            PathBuf::from("/tmp/demo/dist")
        );
    }

    #[test]
    fn dedupe_preserve_order_keeps_first_occurrence() {
        assert_eq!(
            dedupe_preserve_order(&[
                "a".to_string(),
                "b".to_string(),
                "a".to_string(),
                "c".to_string()
            ]),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn maps_target_triples_to_platform_labels() {
        assert_eq!(
            platform_label_for_target("aarch64-apple-darwin"),
            Some("macos")
        );
        assert_eq!(
            platform_label_for_target("x86_64-pc-windows-msvc"),
            Some("windows")
        );
        assert_eq!(
            platform_label_for_target("x86_64-unknown-linux-gnu"),
            Some("linux")
        );
    }
}
