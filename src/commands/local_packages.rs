//! Local machine package install and lookup support.
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use clap::ArgMatches;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

const PACKAGE_MANIFEST_FILE_NAME: &str = "cargo-ai-package.toml";
const INSTALL_MANIFEST_FILE_NAME: &str = "install.toml";
const INSTALLED_PACKAGE_DIR_NAME: &str = "package";
const INSTALLED_PACKAGE_DATA_DIR_NAME: &str = "data";
const INSTALLED_PACKAGE_RUNTIME_DIR_NAME: &str = "runtime";
const INSTALLED_PACKAGE_RUNTIME_TOOLS_DIR_NAME: &str = "tools";
const MAX_PORTABLE_RELATIVE_PATH_BYTES: usize = 1_024;
const MAX_PACKAGE_ARCHIVE_COMPRESSED_BYTES: usize = 10 * 1024 * 1024;
const MAX_PACKAGE_ARCHIVE_BASE64_BYTES: usize =
    MAX_PACKAGE_ARCHIVE_COMPRESSED_BYTES.div_ceil(3) * 4;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StagedInstallFailurePoint {
    BackupInspection,
    AfterBackup,
    AfterDataTransfer,
    FinalAliasReplacement,
    RestoreData,
    RestoreAlias,
}

#[cfg(test)]
thread_local! {
    static TEST_PACKAGES_ROOT: std::cell::RefCell<Option<PathBuf>> = std::cell::RefCell::new(None);
    static TEST_STAGED_INSTALL_FAILURES: std::cell::RefCell<Vec<StagedInstallFailurePoint>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug, Deserialize)]
struct PackageManifestDocument {
    format_version: u32,
    #[serde(default)]
    project_name: Option<String>,
    #[serde(default)]
    project_version: Option<String>,
    profile: String,
    #[serde(default)]
    agent_definitions: Vec<String>,
    #[serde(default)]
    hatched_agents: Vec<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    assets: Vec<String>,
    #[serde(default)]
    permissions: PackagePermissionProfileDocument,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct InstalledPackageDocument {
    format_version: u32,
    alias: String,
    package_name: String,
    package_version: String,
    profile: String,
    content_sha256: String,
    source: InstalledPackageSourceDocument,
    installed_at: String,
    #[serde(default)]
    permissions: PackagePermissionProfileDocument,
    entrypoints: Vec<InstalledPackageEntrypointDocument>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstalledPackageSourceDocument {
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested_owner_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hosted_source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hosted_version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_handle: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct InstalledPackageEntrypointDocument {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) runnable: bool,
    pub(crate) hatchable: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PackagePermissionProfileDocument {
    pub(crate) package_payload: String,
    pub(crate) package_data: String,
    pub(crate) project_workspace: String,
    pub(crate) subprocess: String,
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

#[derive(Clone, Debug)]
pub(crate) struct InstalledPackageRuntimeContext {
    pub(crate) alias: String,
    pub(crate) source_kind: String,
    pub(crate) package_payload_root: PathBuf,
    pub(crate) package_data_root: PathBuf,
    pub(crate) current_entrypoint_path: Option<String>,
    pub(crate) entrypoints: Vec<InstalledPackageEntrypointDocument>,
    pub(crate) permissions: PackagePermissionProfileDocument,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstalledEntrypointCapability {
    Run,
    Hatch,
}

#[derive(Clone, Debug)]
pub(crate) struct CheckedInstalledPackageRuntime {
    pub(crate) context: InstalledPackageRuntimeContext,
    pub(crate) lease: Arc<crate::commands::package_lock::PackageAliasLockGuard>,
}

#[derive(Clone, Debug)]
struct PreparedPackage {
    package_root: PathBuf,
    manifest: PackageManifestDocument,
    content_sha256: String,
    source: InstalledPackageSourceDocument,
    temporary_root: Option<PathBuf>,
}

#[derive(Debug)]
struct TemporaryPackageRootGuard {
    path: Option<PathBuf>,
}

impl TemporaryPackageRootGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("temporary package root guard should remain armed")
    }

    fn release(mut self) {
        self.path = None;
    }

    fn cleanup(mut self, label: &str) -> Result<(), String> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        fs::remove_dir_all(path)
            .map_err(|error| format!("Failed to remove {label} '{}': {}", path.display(), error))?;
        self.path = None;
        Ok(())
    }
}

impl Drop for TemporaryPackageRootGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.as_deref() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[derive(Clone, Debug)]
struct InstallRequest {
    source: Option<String>,
    alias: Option<String>,
    profile: String,
    replace: bool,
    downgrade: bool,
    keep_data: bool,
    delete_data: bool,
}

#[derive(Clone, Debug)]
enum InstallAction {
    New,
    Noop,
    Upgrade,
    Replace,
    Downgrade,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedPackageEntrypoint {
    pub(crate) alias: String,
    pub(crate) entrypoint: String,
    pub(crate) definition_path: PathBuf,
    pub(crate) package_root: PathBuf,
    pub(crate) package_name: String,
    pub(crate) package_version: String,
    pub(crate) content_sha256: String,
    pub(crate) source_kind: String,
    pub(crate) package_data_root: PathBuf,
    pub(crate) permissions: PackagePermissionProfileDocument,
    pub(crate) lease: Arc<crate::commands::package_lock::PackageAliasLockGuard>,
}

pub async fn run(sub_m: &ArgMatches) -> bool {
    if let Some(list_m) = sub_m.subcommand_matches("list") {
        run_list(list_m)
    } else if let Some(install_m) = sub_m.subcommand_matches("install") {
        run_install(install_m)
    } else if let Some(inspect_m) = sub_m.subcommand_matches("inspect") {
        run_inspect(inspect_m)
    } else if let Some(uninstall_m) = sub_m.subcommand_matches("uninstall") {
        run_uninstall(uninstall_m)
    } else {
        eprintln!(
            "No local packages subcommand found. Try 'cargo ai packages list|install|inspect|uninstall'."
        );
        false
    }
}

pub(crate) fn account_handle_from_list_matches(list_m: &ArgMatches) -> Option<Option<String>> {
    list_m.get_one::<String>("account").map(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub(crate) fn account_handle_from_install_matches(
    install_m: &ArgMatches,
) -> Option<Option<String>> {
    install_m.get_one::<String>("account").map(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn run_list(list_m: &ArgMatches) -> bool {
    let display_limit = if list_m.get_flag("all") {
        None
    } else {
        Some(
            list_m
                .get_one::<u32>("limit")
                .copied()
                .unwrap_or(20)
                .try_into()
                .expect("u32 limit should fit in usize"),
        )
    };

    match list_installed_packages() {
        Ok(mut packages) => {
            packages.sort_by(|left, right| left.alias.cmp(&right.alias));
            if let Some(limit) = display_limit {
                packages.truncate(limit);
            }
            if packages.is_empty() {
                println!("No local packages are installed.");
                return true;
            }

            println!("Installed packages:");
            for package in packages {
                let provenance = match package.source.kind.as_str() {
                    "hosted" => package
                        .source
                        .owner_handle
                        .as_deref()
                        .map(|handle| format!("hosted {handle}/{}", package.package_name))
                        .unwrap_or_else(|| "hosted".to_string()),
                    _ => package.source.kind.clone(),
                };
                println!(
                    "- {}  {} {}  {}  {}",
                    package.alias,
                    package.package_name,
                    package.package_version,
                    entrypoint_summary(&package.entrypoints),
                    provenance
                );
            }
            true
        }
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

fn run_install(install_m: &ArgMatches) -> bool {
    let request = InstallRequest {
        source: install_m
            .get_one::<String>("source")
            .map(|value| value.to_string()),
        alias: install_m
            .get_one::<String>("alias")
            .map(|value| value.to_string()),
        profile: install_m
            .get_one::<String>("profile")
            .map(|value| value.to_string())
            .unwrap_or_else(|| "default".to_string()),
        replace: install_m.get_flag("replace"),
        downgrade: install_m.get_flag("downgrade"),
        keep_data: install_m.get_flag("keep_data"),
        delete_data: install_m.get_flag("delete_data"),
    };

    match install_local_package(&request) {
        Ok(InstallAction::Noop) => true,
        Ok(_) => true,
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

pub(crate) async fn run_hosted_install(install_m: &ArgMatches) -> bool {
    let source_id = install_m.get_one::<String>("source_id").map(String::as_str);
    let version_id = install_m
        .get_one::<String>("version_id")
        .map(String::as_str);
    let package_name = install_m
        .get_one::<String>("source")
        .map(String::as_str)
        .unwrap_or("");
    if package_name.is_empty() && source_id.is_none() {
        eprintln!("x Hosted install requires a package name or --source-id.");
        return false;
    }
    let Some(alias) = install_m.get_one::<String>("alias").map(String::as_str) else {
        eprintln!(
            "x Hosted install requires --as <alias> so the local runtime reference is explicit."
        );
        return false;
    };
    let alias = alias.trim();
    if let Err(error) = validate_package_alias(alias) {
        eprintln!("x {error}");
        return false;
    }

    let owner_handle = account_handle_from_install_matches(install_m).flatten();
    let version = install_m
        .get_one::<String>("version")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(version) = version.as_deref() {
        if let Err(error) = Version::parse(version) {
            eprintln!("x Hosted package version '{version}' is not valid semver: {error}");
            return false;
        }
    }

    match install_hosted_package(
        package_name,
        owner_handle.as_deref(),
        version.as_deref(),
        source_id,
        version_id,
        alias,
        install_m.get_flag("replace"),
        install_m.get_flag("downgrade"),
        install_m.get_flag("accept_permissions"),
        install_m.get_flag("keep_data"),
        install_m.get_flag("delete_data"),
    )
    .await
    {
        Ok(InstallAction::Noop) => true,
        Ok(_) => true,
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

pub(crate) async fn run_hosted_update(update_m: &ArgMatches) -> bool {
    let Some(alias) = update_m.get_one::<String>("alias").map(String::as_str) else {
        eprintln!("x Missing package alias.");
        return false;
    };

    match update_hosted_package(alias, update_m.get_flag("accept_permissions")).await {
        Ok(InstallAction::Noop) => true,
        Ok(_) => true,
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

pub(crate) async fn run_hosted_rollback(rollback_m: &ArgMatches) -> bool {
    let Some(alias) = rollback_m.get_one::<String>("alias").map(String::as_str) else {
        eprintln!("x Missing package alias.");
        return false;
    };
    let Some(version) = rollback_m.get_one::<String>("to").map(String::as_str) else {
        eprintln!("x Missing rollback target. Use --to <version>.");
        return false;
    };
    let version = version.trim();
    if let Err(error) = Version::parse(version) {
        eprintln!("x Hosted package rollback version '{version}' is not valid semver: {error}");
        return false;
    }

    match rollback_hosted_package(alias, version, rollback_m.get_flag("accept_permissions")).await {
        Ok(InstallAction::Noop) => true,
        Ok(_) => true,
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

fn run_inspect(inspect_m: &ArgMatches) -> bool {
    let Some(alias) = inspect_m.get_one::<String>("alias").map(String::as_str) else {
        eprintln!("x Missing package alias.");
        return false;
    };

    match load_installed_package(alias) {
        Ok(package) => {
            println!("Package:   {}", package.alias);
            println!("Identity:  {}", package.package_name);
            println!("Version:   {}", package.package_version);
            println!("Profile:   {}", package.profile);
            println!("Hash:      {}", package.content_sha256);
            println!("Installed: {}", package.installed_at);
            match package.source.path.as_deref() {
                Some(path) => println!("Source:    {} ({})", package.source.kind, path),
                None => println!("Source:    {}", package.source.kind),
            }
            if package.source.kind == "hosted" {
                println!("Hosted:");
                println!(
                    "  Source ID:  {}",
                    package
                        .source
                        .hosted_source_id
                        .as_deref()
                        .unwrap_or("unknown")
                );
                println!(
                    "  Version ID: {}",
                    package
                        .source
                        .hosted_version_id
                        .as_deref()
                        .unwrap_or("unknown")
                );
                if let Some(owner_handle) = package.source.owner_handle.as_deref() {
                    println!("  Owner:      {}", owner_handle);
                }
            }
            println!("Permissions:");
            for line in permission_profile_lines(&package.permissions) {
                println!("  {line}");
            }
            println!(
                "Data root: {}",
                installed_package_data_root(package.alias.as_str()).display()
            );
            println!("Entrypoints:");
            for entrypoint in package.entrypoints {
                let mut capabilities = Vec::new();
                if entrypoint.runnable {
                    capabilities.push("run");
                }
                if entrypoint.hatchable {
                    capabilities.push("hatch");
                }
                println!(
                    "- {}  {}  {}",
                    entrypoint.name,
                    capabilities.join(","),
                    entrypoint.path
                );
            }
            true
        }
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

fn run_uninstall(uninstall_m: &ArgMatches) -> bool {
    let Some(alias) = uninstall_m.get_one::<String>("alias").map(String::as_str) else {
        eprintln!("x Missing package alias.");
        return false;
    };

    match uninstall_package(alias, uninstall_m.get_flag("delete_data")) {
        Ok(()) => {
            println!("✓ Package `{alias}` uninstalled");
            true
        }
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

fn install_local_package(request: &InstallRequest) -> Result<InstallAction, String> {
    let prepared = match request.source.as_deref() {
        Some(source) => prepare_explicit_source(source)?,
        None => prepare_current_project(request.profile.as_str())?,
    };
    let materialized = match materialize_prepared_package(
        &prepared,
        request.alias.as_deref(),
        request.replace,
        request.downgrade,
        false,
        request.keep_data,
        request.delete_data,
    ) {
        Ok(materialized) => materialized,
        Err(error) => {
            cleanup_prepared_package(&prepared);
            return Err(error);
        }
    };
    cleanup_prepared_package(&prepared);

    if matches!(materialized.action, InstallAction::Noop) {
        println!(
            "✓ Package `{}` is already installed at version {}.",
            materialized.alias, materialized.package_version
        );
        return Ok(materialized.action);
    }

    match materialized.action {
        InstallAction::New => println!(
            "✓ Package `{}` installed as `{}` at version {}.",
            materialized.package_name, materialized.alias, materialized.package_version
        ),
        InstallAction::Upgrade => println!(
            "✓ Package `{}` upgraded as `{}` to version {}.",
            materialized.package_name, materialized.alias, materialized.package_version
        ),
        InstallAction::Replace => println!(
            "✓ Package alias `{}` replaced with `{}` version {}.",
            materialized.alias, materialized.package_name, materialized.package_version
        ),
        InstallAction::Downgrade => println!(
            "✓ Package `{}` downgraded as `{}` to version {}.",
            materialized.package_name, materialized.alias, materialized.package_version
        ),
        InstallAction::Noop => {}
    }
    Ok(materialized.action)
}

#[derive(Clone, Debug)]
struct MaterializedPackageInstall {
    action: InstallAction,
    alias: String,
    package_name: String,
    package_version: String,
}

fn materialize_prepared_package(
    prepared: &PreparedPackage,
    alias_override: Option<&str>,
    replace: bool,
    downgrade: bool,
    accept_permissions: bool,
    keep_data: bool,
    delete_data: bool,
) -> Result<MaterializedPackageInstall, String> {
    let package_name = required_package_name(&prepared.manifest)?;
    let alias = alias_override
        .unwrap_or(package_name.as_str())
        .trim()
        .to_string();
    validate_package_alias(alias.as_str())?;
    let _lock = acquire_package_alias_lock(alias.as_str())?;
    materialize_prepared_package_under_lock(
        prepared,
        alias.as_str(),
        replace,
        downgrade,
        accept_permissions,
        keep_data,
        delete_data,
    )
}

fn materialize_prepared_package_under_lock(
    prepared: &PreparedPackage,
    alias: &str,
    replace: bool,
    downgrade: bool,
    accept_permissions: bool,
    keep_data: bool,
    delete_data: bool,
) -> Result<MaterializedPackageInstall, String> {
    let package_name = required_package_name(&prepared.manifest)?;
    let package_version = required_package_version(&prepared.manifest)?;
    validate_permission_profile(&prepared.manifest.permissions)?;
    let materialize_source_tools =
        prepared.source.kind != "hosted" || prepared.manifest.permissions.subprocess == "allowed";
    let entrypoints = build_entrypoints(&prepared.manifest, &prepared.package_root)?;
    let existing = load_installed_package_if_present(alias)?;
    ensure_source_identity_replacement_is_explicit(
        existing.as_ref(),
        &prepared.source,
        package_name.as_str(),
        replace,
    )?;
    if prepared.source.kind == "hosted" {
        ensure_hosted_permissions_are_accepted(
            existing.as_ref(),
            &prepared.source,
            &prepared.manifest.permissions,
            package_name.as_str(),
            package_version.as_str(),
            accept_permissions,
        )?;
    }
    let identity_changed = existing.as_ref().is_some_and(|installed| {
        installed_identity_changes(installed, &prepared.source, package_name.as_str())
    });
    let mut action = if identity_changed {
        InstallAction::Replace
    } else {
        determine_install_action(
            existing.as_ref(),
            package_name.as_str(),
            package_version.as_str(),
            prepared.content_sha256.as_str(),
            replace,
            downgrade,
        )?
    };
    if matches!(action, InstallAction::Noop)
        && !installed_package_runtime_is_complete(
            alias,
            prepared.manifest.tools.as_slice(),
            materialize_source_tools,
        )?
    {
        action = InstallAction::Replace;
    }
    if matches!(action, InstallAction::Noop) && existing.is_some() && delete_data {
        action = InstallAction::Replace;
    }
    let preserve_existing_data = preserve_existing_data_for_install(
        existing.as_ref(),
        &prepared.source,
        package_name.as_str(),
        keep_data,
        delete_data,
    )?;

    if !matches!(action, InstallAction::Noop) {
        let document = InstalledPackageDocument {
            format_version: 1,
            alias: alias.to_string(),
            package_name: package_name.clone(),
            package_version: package_version.clone(),
            profile: prepared.manifest.profile.clone(),
            content_sha256: prepared.content_sha256.clone(),
            source: prepared.source.clone(),
            installed_at: now_rfc3339()?,
            permissions: prepared.manifest.permissions.clone(),
            entrypoints,
        };

        write_staged_install(
            alias,
            &prepared.package_root,
            &document,
            preserve_existing_data,
            prepared.manifest.tools.as_slice(),
            materialize_source_tools,
        )?;
    }

    Ok(MaterializedPackageInstall {
        action,
        alias: alias.to_string(),
        package_name,
        package_version,
    })
}

async fn install_hosted_package(
    package_name: &str,
    owner_handle: Option<&str>,
    version: Option<&str>,
    source_id: Option<&str>,
    version_id: Option<&str>,
    alias: &str,
    replace: bool,
    downgrade: bool,
    accept_permissions: bool,
    keep_data: bool,
    delete_data: bool,
) -> Result<InstallAction, String> {
    let response =
        pull_inspected_hosted_package(package_name, owner_handle, source_id, version, version_id)
            .await?;
    let prepared = prepare_hosted_response(&response, owner_handle, source_id)?;
    print_permission_summary(&prepared.manifest.permissions);
    let materialized = match materialize_prepared_package(
        &prepared,
        Some(alias),
        replace,
        downgrade,
        accept_permissions,
        keep_data,
        delete_data,
    ) {
        Ok(materialized) => materialized,
        Err(error) => {
            cleanup_prepared_package(&prepared);
            return Err(error);
        }
    };
    cleanup_prepared_package(&prepared);

    if matches!(materialized.action, InstallAction::Noop) {
        println!(
            "✓ Hosted package `{}` is already installed as `{}` at version {}.",
            materialized.package_name, materialized.alias, materialized.package_version
        );
        return Ok(materialized.action);
    }

    println!(
        "✓ Hosted package `{}` installed as `{}` at version {}.",
        materialized.package_name, materialized.alias, materialized.package_version
    );
    Ok(materialized.action)
}

async fn update_hosted_package(
    alias: &str,
    accept_permissions: bool,
) -> Result<InstallAction, String> {
    validate_package_alias(alias)?;
    let _lock = acquire_package_alias_lock(alias)?;
    let existing = load_installed_package(alias)?;
    ensure_installed_source_is_hosted(&existing)?;
    let installed_version = Version::parse(existing.package_version.as_str()).map_err(|error| {
        format!(
            "Installed package alias `{}` has invalid version '{}': {}",
            existing.alias, existing.package_version, error
        )
    })?;

    let hosted_source_id = existing
        .source
        .hosted_source_id
        .as_deref()
        .expect("hosted source validation should require an id");
    let response = pull_hosted_package(
        existing.package_name.as_str(),
        None,
        Some(hosted_source_id),
        None,
    )
    .await?;
    let prepared = prepare_hosted_response(&response, None, Some(hosted_source_id))?;
    ensure_hosted_source_matches_existing(&existing, &prepared.source)?;
    let resolved_version = required_package_version(&prepared.manifest)?;
    let resolved = Version::parse(resolved_version.as_str()).map_err(|error| {
        format!(
            "Hosted package version '{}' is not valid semver: {}",
            resolved_version, error
        )
    })?;

    if resolved < installed_version {
        cleanup_prepared_package(&prepared);
        println!(
            "✓ Hosted package `{}` is already up to date at version {}.",
            existing.alias, existing.package_version
        );
        return Ok(InstallAction::Noop);
    }

    print_permission_summary(&prepared.manifest.permissions);
    let materialized = match materialize_prepared_package_under_lock(
        &prepared,
        alias,
        false,
        false,
        accept_permissions,
        false,
        false,
    ) {
        Ok(materialized) => materialized,
        Err(error) => {
            cleanup_prepared_package(&prepared);
            return Err(error);
        }
    };
    cleanup_prepared_package(&prepared);
    if matches!(materialized.action, InstallAction::Noop) {
        println!(
            "✓ Hosted package `{}` is already up to date at version {}.",
            materialized.alias, materialized.package_version
        );
    } else if resolved == installed_version {
        println!(
            "✓ Hosted package `{}` runtime repaired at version {}.",
            materialized.alias, materialized.package_version
        );
    } else {
        println!(
            "✓ Hosted package `{}` updated to version {}.",
            materialized.alias, materialized.package_version
        );
    }
    Ok(materialized.action)
}

async fn rollback_hosted_package(
    alias: &str,
    target_version: &str,
    accept_permissions: bool,
) -> Result<InstallAction, String> {
    validate_package_alias(alias)?;
    let _lock = acquire_package_alias_lock(alias)?;
    let existing = load_installed_package(alias)?;
    ensure_installed_source_is_hosted(&existing)?;
    let installed_version = Version::parse(existing.package_version.as_str()).map_err(|error| {
        format!(
            "Installed package alias `{}` has invalid version '{}': {}",
            existing.alias, existing.package_version, error
        )
    })?;
    let requested_version = Version::parse(target_version).map_err(|error| {
        format!("Hosted package rollback version '{target_version}' is not valid semver: {error}")
    })?;
    if requested_version > installed_version {
        return Err(format!(
            "Rollback target {} is newer than installed version {}. Use `cargo ai packages update {}` to move forward.",
            requested_version, installed_version, alias
        ));
    }

    let hosted_source_id = existing
        .source
        .hosted_source_id
        .as_deref()
        .expect("hosted source validation should require an id");
    let response = pull_hosted_package(
        existing.package_name.as_str(),
        None,
        Some(hosted_source_id),
        Some(target_version),
    )
    .await?;
    let prepared = prepare_hosted_response(&response, None, Some(hosted_source_id))?;
    ensure_hosted_source_matches_existing(&existing, &prepared.source)?;
    print_permission_summary(&prepared.manifest.permissions);
    let materialized = match materialize_prepared_package_under_lock(
        &prepared,
        alias,
        false,
        true,
        accept_permissions,
        false,
        false,
    ) {
        Ok(materialized) => materialized,
        Err(error) => {
            cleanup_prepared_package(&prepared);
            return Err(error);
        }
    };
    cleanup_prepared_package(&prepared);
    if matches!(materialized.action, InstallAction::Noop) {
        println!(
            "✓ Hosted package `{}` is already installed at version {}.",
            materialized.alias, materialized.package_version
        );
    } else if requested_version == installed_version {
        println!(
            "✓ Hosted package `{}` runtime repaired at version {}.",
            materialized.alias, materialized.package_version
        );
    } else {
        println!(
            "✓ Hosted package `{}` rolled back to version {}.",
            materialized.alias, materialized.package_version
        );
    }
    Ok(materialized.action)
}

async fn pull_hosted_package(
    package_name: &str,
    owner: Option<&str>,
    source: Option<&str>,
    version: Option<&str>,
) -> Result<Value, String> {
    pull_inspected_hosted_package(package_name, owner, source, version, None).await
}

pub(crate) async fn pull_inspected_hosted_package(
    package_name: &str,
    owner: Option<&str>,
    source: Option<&str>,
    version: Option<&str>,
    version_id: Option<&str>,
) -> Result<Value, String> {
    let inspected =
        read_hosted_package(package_name, owner, source, version, version_id, true).await?;
    crate::commands::package_inspection::render_snapshot(&inspected)?;
    let source = required_response_string(&inspected, "hosted_source_id")?;
    let exact = required_response_string(&inspected, "hosted_version_id")?;
    let downloaded =
        read_hosted_package("", None, Some(&source), None, Some(&exact), false).await?;
    crate::commands::package_inspection::validate_same_snapshot(&inspected, &downloaded)?;
    Ok(downloaded)
}

pub(crate) async fn read_hosted_package(
    package_name: &str,
    owner_handle: Option<&str>,
    hosted_source_id: Option<&str>,
    version: Option<&str>,
    version_id: Option<&str>,
    inspect: bool,
) -> Result<Value, String> {
    use crate::commands::account::helpers::{
        load_account_auth, persist_refreshed_access_token, refresh_access_token_for_retry,
        RefreshAccessError, INFRA_BASE_URL,
    };

    let auth = load_account_auth()
        .map_err(|message| crate::ui::account_status::normalize_leading_glyph(message.as_str()))?;
    let access_token_owned = auth.access_token;
    let refresh_token = auth.refresh_token;

    let mut response = crate::infra_api::account::projects::read_project(
        INFRA_BASE_URL,
        access_token_owned.as_str(),
        package_name,
        owner_handle,
        hosted_source_id,
        version,
        version_id,
        inspect,
    )
    .await
    .map_err(|error| format!("Request failed: {error}"))?;

    if response
        .get("type")
        .and_then(Value::as_str)
        .map(|kind| kind == "access_token_expired")
        .unwrap_or(false)
    {
        match refresh_access_token_for_retry(access_token_owned.as_str(), refresh_token.as_deref())
            .await
        {
            Err(RefreshAccessError::MissingRefreshToken) => {
                return Err(
                    "Access token expired, and no refresh token exists in credential store. Run `cargo ai account status` or re-confirm account."
                        .to_string(),
                );
            }
            Err(RefreshAccessError::RequestFailed(error)) => {
                return Err(format!("Request failed while refreshing session: {error}"));
            }
            Err(RefreshAccessError::MissingRefreshedToken(refresh_response)) => {
                return Err(backend_response_message(
                    &refresh_response,
                    "Session refresh did not return a new access token.",
                ));
            }
            Ok((retry_access_token, refreshed_expires_in)) => {
                if let Some(rt) = refresh_token.as_deref() {
                    persist_refreshed_access_token(
                        retry_access_token.as_str(),
                        rt,
                        refreshed_expires_in,
                    );
                }
                response = crate::infra_api::account::projects::read_project(
                    INFRA_BASE_URL,
                    retry_access_token.as_str(),
                    package_name,
                    owner_handle,
                    hosted_source_id,
                    version,
                    version_id,
                    inspect,
                )
                .await
                .map_err(|error| format!("Request failed after session refresh: {error}"))?;
            }
        }
    }

    let expected_type = if inspect {
        "account_projects_inspect_succeeded"
    } else {
        "account_projects_pull_succeeded"
    };
    if response["status"] != "success" || response["type"] != expected_type {
        return Err(backend_response_message(
            &response,
            "Hosted package pull did not succeed.",
        ));
    }
    let selected_name = if hosted_source_id.is_some() {
        ""
    } else {
        package_name
    };
    validate_hosted_response_matches_request(&response, selected_name, version)?;
    if version_id.is_some_and(|id| response["hosted_version_id"].as_str() != Some(id)) {
        return Err("Hosted response did not match the requested immutable version ID.".into());
    }
    validate_hosted_response_provenance(&response, owner_handle, hosted_source_id)?;

    Ok(response)
}

fn prepare_hosted_response(
    response: &Value,
    requested_owner_handle: Option<&str>,
    requested_hosted_source_id: Option<&str>,
) -> Result<PreparedPackage, String> {
    let archive_base64 = response
        .get("package_archive_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "Hosted pull response did not include `package_archive_base64`.".to_string()
        })?;
    let package_sha256 = response
        .get("package_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| "Hosted pull response did not include `package_sha256`.".to_string())?;
    let package_size_bytes = response
        .get("package_size_bytes")
        .and_then(Value::as_i64)
        .ok_or_else(|| "Hosted pull response did not include `package_size_bytes`.".to_string())?;
    let declared_size_bytes = usize::try_from(package_size_bytes).map_err(|_| {
        "Hosted pull response declared an invalid negative or unsupported package archive size."
            .to_string()
    })?;
    validate_package_archive_size(declared_size_bytes)?;
    let hosted_source_id = required_response_string(response, "hosted_source_id")?;
    let hosted_version_id = required_response_string(response, "hosted_version_id")?;

    let archive_bytes = decode_package_archive_base64(archive_base64)?;
    let decoded_size_bytes = i64::try_from(archive_bytes.len()).map_err(|_| {
        "Decoded hosted package archive exceeded supported size limits.".to_string()
    })?;
    if decoded_size_bytes != package_size_bytes {
        return Err(format!(
            "Hosted package archive size mismatch. Expected {} bytes, got {} bytes after decoding.",
            package_size_bytes, decoded_size_bytes
        ));
    }

    let decoded_sha256 = crate::commands::account::sha256_hex(archive_bytes.as_slice());
    if decoded_sha256 != package_sha256 {
        return Err(format!(
            "Hosted package archive checksum mismatch. Expected {}, got {}.",
            package_sha256, decoded_sha256
        ));
    }

    let staging_root = ensure_packages_staging_root()?.join(format!("hosted-{}", Uuid::new_v4()));
    fs::create_dir(&staging_root).map_err(|error| {
        format!(
            "Failed to create hosted package staging directory '{}': {}",
            staging_root.display(),
            error
        )
    })?;
    let staging_guard = TemporaryPackageRootGuard::new(staging_root);
    crate::commands::account::extract_package_archive_bytes(
        archive_bytes.as_slice(),
        staging_guard.path(),
    )?;

    let mut prepared = prepare_package_root(
        staging_guard.path().to_path_buf(),
        InstalledPackageSourceDocument {
            kind: "hosted".to_string(),
            path: None,
            account_selector: Some(
                if requested_hosted_source_id.is_some() {
                    "source_id"
                } else if requested_owner_handle.is_some() {
                    "handle"
                } else {
                    "self"
                }
                .to_string(),
            ),
            requested_owner_handle: requested_owner_handle.map(str::to_string),
            hosted_source_id: Some(hosted_source_id),
            hosted_version_id: Some(hosted_version_id),
            owner_handle: optional_response_string(response, "owner_handle"),
        },
        Some(staging_guard.path().to_path_buf()),
    )?;
    prepared.content_sha256 = decoded_sha256;
    validate_hosted_response_matches_manifest(response, &prepared.manifest)?;
    super::package_inspection::validate_archive_manifest(response, staging_guard.path())?;
    staging_guard.release();
    Ok(prepared)
}

pub(crate) fn resolve_entrypoint_reference(
    reference: &str,
    require_hatchable: bool,
) -> Result<Option<ResolvedPackageEntrypoint>, String> {
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("Failed to inspect the current project directory: {error}"))?;
    let project_root = crate::commands::package_dependencies::find_project_root(&current_dir)?;
    resolve_entrypoint_reference_for_project(reference, require_hatchable, project_root.as_deref())
}

pub(crate) fn resolve_entrypoint_reference_for_project(
    reference: &str,
    require_hatchable: bool,
    dependency_project_root: Option<&Path>,
) -> Result<Option<ResolvedPackageEntrypoint>, String> {
    let Some((alias, entrypoint)) = reference.split_once("::") else {
        return Ok(None);
    };
    validate_package_alias(alias)?;
    validate_entrypoint_name(entrypoint)?;

    let lease = Arc::new(acquire_package_alias_read_lock(alias)?);
    let package = load_installed_package(alias)?;
    if let Some(project_root) = dependency_project_root {
        crate::commands::package_dependencies::validate_installed_dependency(
            project_root,
            crate::commands::package_dependencies::InstalledPackageDependencyIdentity {
                alias,
                source_kind: package.source.kind.as_str(),
                hosted_source_id: package.source.hosted_source_id.as_deref(),
                package_version: package.package_version.as_str(),
            },
        )?;
    }
    let installed_root = installed_package_root(alias);
    let entry = package
        .entrypoints
        .iter()
        .find(|candidate| candidate.name == entrypoint)
        .ok_or_else(|| {
            format!(
                "Package alias `{}` does not export entrypoint `{}`.",
                alias, entrypoint
            )
        })?;
    if !entry.runnable {
        return Err(format!(
            "Package entrypoint `{}` is not runnable.",
            reference
        ));
    }
    if require_hatchable && !entry.hatchable {
        return Err(format!(
            "Package entrypoint `{}` is not hatchable.",
            reference
        ));
    }

    let package_payload_root = installed_root.join(INSTALLED_PACKAGE_DIR_NAME);
    let relative_definition_path =
        normalize_portable_relative_path(entry.path.as_str(), "Installed package entrypoint path")?;
    let definition_path = resolve_existing_path_under_root(
        package_payload_root.as_path(),
        relative_definition_path.as_path(),
        "Installed package entrypoint",
    )?;
    if !definition_path.is_file() {
        return Err(format!(
            "Installed package entrypoint `{}` points to missing definition '{}'.",
            reference,
            definition_path.display()
        ));
    }

    Ok(Some(ResolvedPackageEntrypoint {
        alias: alias.to_string(),
        entrypoint: entrypoint.to_string(),
        definition_path,
        package_root: package_payload_root,
        package_name: package.package_name,
        package_version: package.package_version,
        content_sha256: package.content_sha256,
        source_kind: package.source.kind,
        package_data_root: installed_root.join(INSTALLED_PACKAGE_DATA_DIR_NAME),
        permissions: package.permissions,
        lease,
    }))
}

pub(crate) fn validate_installed_alias_dependency_for_project(
    alias: &str,
    project_root: &Path,
) -> Result<(), String> {
    let _lease = acquire_package_alias_read_lock(alias)?;
    let package = load_installed_package(alias)?;
    crate::commands::package_dependencies::validate_installed_dependency(
        project_root,
        crate::commands::package_dependencies::InstalledPackageDependencyIdentity {
            alias,
            source_kind: package.source.kind.as_str(),
            hosted_source_id: package.source.hosted_source_id.as_deref(),
            package_version: package.package_version.as_str(),
        },
    )
}

#[cfg(feature = "developer-tools")]
fn prepare_current_project(profile: &str) -> Result<PreparedPackage, String> {
    let assembled =
        crate::commands::package::assemble_current_project_package(profile, None, true, false)?;
    let manifest_path = assembled.root_path.join(PACKAGE_MANIFEST_FILE_NAME);
    let manifest = load_package_manifest(manifest_path.as_path())?;
    Ok(PreparedPackage {
        package_root: assembled.root_path,
        manifest,
        content_sha256: crate::commands::account::sha256_hex(assembled.archive_bytes.as_slice()),
        source: InstalledPackageSourceDocument {
            kind: "current_project".to_string(),
            path: std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string()),
            account_selector: None,
            requested_owner_handle: None,
            hosted_source_id: None,
            hosted_version_id: None,
            owner_handle: None,
        },
        temporary_root: None,
    })
}

#[cfg(not(feature = "developer-tools"))]
fn prepare_current_project(_profile: &str) -> Result<PreparedPackage, String> {
    Err(
        "Installing the current project requires a Cargo AI build with developer-tools enabled. Provide an explicit local package source instead."
            .to_string(),
    )
}

fn prepare_explicit_source(source: &str) -> Result<PreparedPackage, String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err("Package source cannot be empty.".to_string());
    }
    let source_path = PathBuf::from(trimmed);
    if !source_path.exists() {
        return Err(format!(
            "Local package source '{}' was not found. Use a local package path, or add --account to install from cargo-ai.org in the hosted install lane.",
            source
        ));
    }

    if source_path.is_dir() {
        return prepare_package_root(
            source_path,
            InstalledPackageSourceDocument {
                kind: "local_root".to_string(),
                path: Some(trimmed.to_string()),
                account_selector: None,
                requested_owner_handle: None,
                hosted_source_id: None,
                hosted_version_id: None,
                owner_handle: None,
            },
            None,
        );
    }

    if source_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == PACKAGE_MANIFEST_FILE_NAME)
        .unwrap_or(false)
    {
        let package_root = source_path.parent().ok_or_else(|| {
            format!(
                "Package manifest '{}' has no parent package root.",
                source_path.display()
            )
        })?;
        return prepare_package_root(
            package_root.to_path_buf(),
            InstalledPackageSourceDocument {
                kind: "manifest_path".to_string(),
                path: Some(trimmed.to_string()),
                account_selector: None,
                requested_owner_handle: None,
                hosted_source_id: None,
                hosted_version_id: None,
                owner_handle: None,
            },
            None,
        );
    }

    prepare_archive_source(source_path.as_path(), trimmed)
}

fn prepare_archive_source(
    source_path: &Path,
    source_display: &str,
) -> Result<PreparedPackage, String> {
    let archive_metadata = fs::metadata(source_path).map_err(|error| {
        format!(
            "Failed to inspect package archive '{}': {}",
            source_path.display(),
            error
        )
    })?;
    let archive_size = usize::try_from(archive_metadata.len())
        .map_err(|_| "Package archive size exceeded supported limits.".to_string())?;
    validate_package_archive_size(archive_size)?;
    let staging_root = ensure_packages_staging_root()?.join(format!("archive-{}", Uuid::new_v4()));
    fs::create_dir(&staging_root).map_err(|error| {
        format!(
            "Failed to create package archive staging directory '{}': {}",
            staging_root.display(),
            error
        )
    })?;
    let staging_guard = TemporaryPackageRootGuard::new(staging_root);
    let bytes = fs::read(source_path).map_err(|error| {
        format!(
            "Failed to read package archive '{}': {}",
            source_path.display(),
            error
        )
    })?;
    crate::commands::account::extract_package_archive_bytes(
        bytes.as_slice(),
        staging_guard.path(),
    )?;

    let prepared = prepare_package_root(
        staging_guard.path().to_path_buf(),
        InstalledPackageSourceDocument {
            kind: "local_archive".to_string(),
            path: Some(source_display.to_string()),
            account_selector: None,
            requested_owner_handle: None,
            hosted_source_id: None,
            hosted_version_id: None,
            owner_handle: None,
        },
        Some(staging_guard.path().to_path_buf()),
    )?;
    staging_guard.release();
    Ok(prepared)
}

fn prepare_package_root(
    package_root: PathBuf,
    source: InstalledPackageSourceDocument,
    temporary_root: Option<PathBuf>,
) -> Result<PreparedPackage, String> {
    let manifest = load_package_manifest(package_root.join(PACKAGE_MANIFEST_FILE_NAME).as_path())?;
    let archive_bytes =
        crate::commands::account::create_package_archive_bytes(package_root.as_path())?;
    let content_sha256 = crate::commands::account::sha256_hex(archive_bytes.as_slice());
    Ok(PreparedPackage {
        package_root,
        manifest,
        content_sha256,
        source,
        temporary_root,
    })
}

fn load_package_manifest(path: &Path) -> Result<PackageManifestDocument, String> {
    let contents = fs::read_to_string(path).map_err(|error| {
        format!(
            "Failed to read package manifest '{}': {}",
            path.display(),
            error
        )
    })?;
    let raw: toml::Value =
        toml::from_str(&contents).map_err(|error| format!("Invalid package manifest: {error}"))?;
    super::package_metadata::verify_inventory(
        path.parent().ok_or("Package manifest has no parent")?,
        &serde_json::to_value(raw).map_err(|e| e.to_string())?,
    )?;
    let manifest: PackageManifestDocument = toml::from_str(contents.as_str()).map_err(|error| {
        format!(
            "Failed to parse package manifest '{}': {}",
            path.display(),
            error
        )
    })?;
    if manifest.format_version != 1 {
        return Err(format!(
            "Package manifest '{}' has unsupported format_version {}.",
            path.display(),
            manifest.format_version
        ));
    }
    Ok(manifest)
}

fn required_package_name(manifest: &PackageManifestDocument) -> Result<String, String> {
    manifest
        .project_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| "Package install requires `cargo-ai-package.toml` project_name.".to_string())
}

fn required_package_version(manifest: &PackageManifestDocument) -> Result<String, String> {
    let version = manifest
        .project_version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            "Package install requires `cargo-ai-package.toml` project_version.".to_string()
        })?;
    Version::parse(version.as_str()).map_err(|error| {
        format!(
            "Package version '{}' is not valid semver: {}",
            version, error
        )
    })?;
    Ok(version)
}

fn build_entrypoints(
    manifest: &PackageManifestDocument,
    package_root: &Path,
) -> Result<Vec<InstalledPackageEntrypointDocument>, String> {
    let _support_material_count = manifest.tools.len() + manifest.assets.len();
    let mut entrypoints = BTreeMap::new();
    for relative_path in &manifest.agent_definitions {
        upsert_entrypoint(&mut entrypoints, package_root, relative_path, true, false)?;
    }
    for relative_path in &manifest.hatched_agents {
        upsert_entrypoint(&mut entrypoints, package_root, relative_path, true, true)?;
    }
    Ok(entrypoints.into_values().collect())
}

fn upsert_entrypoint(
    entrypoints: &mut BTreeMap<String, InstalledPackageEntrypointDocument>,
    package_root: &Path,
    relative_path: &str,
    runnable: bool,
    hatchable: bool,
) -> Result<(), String> {
    let normalized_path = validate_package_relative_json_path(relative_path)?;
    let definition_path = resolve_existing_path_under_root(
        package_root,
        normalized_path.as_path(),
        "Package entrypoint",
    )?;
    if !definition_path.is_file() {
        return Err(format!(
            "Package entrypoint definition '{}' was not found.",
            definition_path.display()
        ));
    }
    let normalized_path = normalized_path.to_string_lossy().replace('\\', "/");
    let name = entrypoint_name_from_path(normalized_path.as_str())?;
    match entrypoints.entry(name.clone()) {
        Entry::Vacant(slot) => {
            slot.insert(InstalledPackageEntrypointDocument {
                name,
                path: normalized_path,
                runnable,
                hatchable,
            });
        }
        Entry::Occupied(mut slot) => {
            if slot.get().path != normalized_path {
                return Err(format!(
                    "Package entrypoint name '{}' is declared by multiple paths ('{}' and '{}').",
                    name,
                    slot.get().path,
                    relative_path
                ));
            }
            slot.get_mut().runnable |= runnable;
            slot.get_mut().hatchable |= hatchable;
        }
    }
    Ok(())
}

fn entrypoint_name_from_path(relative_path: &str) -> Result<String, String> {
    let name = Path::new(relative_path)
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "Unable to derive package entrypoint name from '{}'.",
                relative_path
            )
        })?
        .to_string();
    validate_entrypoint_name(name.as_str())?;
    Ok(name)
}

fn determine_install_action(
    existing: Option<&InstalledPackageDocument>,
    package_name: &str,
    package_version: &str,
    content_sha256: &str,
    replace: bool,
    downgrade: bool,
) -> Result<InstallAction, String> {
    let Some(existing) = existing else {
        return Ok(InstallAction::New);
    };
    if existing.package_name != package_name {
        if replace {
            return Ok(InstallAction::Replace);
        }
        return Err(format!(
            "Package alias `{}` is already installed for package `{}`. Re-run with --replace to replace it with `{}`.",
            existing.alias, existing.package_name, package_name
        ));
    }

    let existing_version = Version::parse(existing.package_version.as_str()).map_err(|error| {
        format!(
            "Installed package alias `{}` has invalid version '{}': {}",
            existing.alias, existing.package_version, error
        )
    })?;
    let new_version = Version::parse(package_version).map_err(|error| {
        format!(
            "Package version '{}' is not valid semver: {}",
            package_version, error
        )
    })?;

    if new_version == existing_version {
        if existing.content_sha256 == content_sha256 {
            return Ok(InstallAction::Noop);
        }
        if replace {
            return Ok(InstallAction::Replace);
        }
        return Err(format!(
            "Package alias `{}` already has `{}` version {} installed with different content. Re-run with --replace to replace same-version content.",
            existing.alias, package_name, package_version
        ));
    }
    if new_version > existing_version {
        return Ok(InstallAction::Upgrade);
    }
    if downgrade {
        return Ok(InstallAction::Downgrade);
    }
    Err(format!(
        "Package alias `{}` already has `{}` version {} installed. Installing older version {} requires --downgrade.",
        existing.alias, package_name, existing.package_version, package_version
    ))
}

fn ensure_source_identity_replacement_is_explicit(
    existing: Option<&InstalledPackageDocument>,
    new_source: &InstalledPackageSourceDocument,
    package_name: &str,
    replace: bool,
) -> Result<(), String> {
    let Some(existing) = existing else {
        return Ok(());
    };
    if existing.source.kind != "hosted" && new_source.kind != "hosted" {
        return Ok(());
    }
    let existing_identity = existing
        .source
        .hosted_source_id
        .as_deref()
        .or(Some(existing.source.kind.as_str()));
    let new_identity = new_source
        .hosted_source_id
        .as_deref()
        .or(Some(new_source.kind.as_str()));
    if existing_identity == new_identity {
        return Ok(());
    }
    if replace {
        return Ok(());
    }
    Err(format!(
        "Package alias `{}` is already installed for a different source identity. Re-run with --replace to replace it with `{}`.",
        existing.alias, package_name
    ))
}

fn ensure_hosted_source_matches_existing(
    existing: &InstalledPackageDocument,
    new_source: &InstalledPackageSourceDocument,
) -> Result<(), String> {
    let existing_source_id = existing.source.hosted_source_id.as_deref().ok_or_else(|| {
        format!(
            "Installed package alias `{}` is missing hosted_source_id metadata.",
            existing.alias
        )
    })?;
    let new_source_id = new_source.hosted_source_id.as_deref().ok_or_else(|| {
        "Hosted pull response did not include hosted_source_id metadata.".to_string()
    })?;
    if existing_source_id != new_source_id {
        return Err(format!(
            "Hosted package resolution returned source id `{}` but installed alias `{}` is pinned to `{}`.",
            new_source_id, existing.alias, existing_source_id
        ));
    }
    Ok(())
}

fn ensure_installed_source_is_hosted(package: &InstalledPackageDocument) -> Result<(), String> {
    if package.source.kind != "hosted" {
        return Err(format!(
            "Package alias `{}` was installed from `{}`. update/rollback only apply to hosted package aliases.",
            package.alias, package.source.kind
        ));
    }
    if package.source.hosted_source_id.is_none() {
        return Err(format!(
            "Package alias `{}` is hosted but missing hosted_source_id metadata.",
            package.alias
        ));
    }
    Ok(())
}

fn ensure_hosted_permissions_are_accepted(
    existing: Option<&InstalledPackageDocument>,
    new_source: &InstalledPackageSourceDocument,
    new_permissions: &PackagePermissionProfileDocument,
    package_name: &str,
    package_version: &str,
    accept_permissions: bool,
) -> Result<(), String> {
    if matches!(
        new_permissions.project_workspace.as_str(),
        "read" | "read_write"
    ) {
        return Err(format!(
            "Hosted package `{}` version {} requests unsupported project/workspace access (`{}`). Cargo AI cannot enforce that grant in this release, so installation is blocked even with `--accept-permissions`.",
            package_name, package_version, new_permissions.project_workspace
        ));
    }

    let baseline = PackagePermissionProfileDocument::default();
    let same_hosted_source = existing.filter(|package| {
        package.source.kind == "hosted"
            && package.source.hosted_source_id.is_some()
            && package.source.hosted_source_id == new_source.hosted_source_id
    });
    let current_permissions = same_hosted_source
        .map(|package| &package.permissions)
        .unwrap_or(&baseline);
    let requires_acceptance = new_permissions.subprocess == "allowed"
        && (same_hosted_source.is_none() || current_permissions.subprocess != "allowed")
        || permission_profile_expands(current_permissions, new_permissions);
    if !requires_acceptance || accept_permissions {
        return Ok(());
    }

    Err(format!(
        "Hosted package `{}` version {} requests permissions beyond the currently accepted profile. Review the permission summary and re-run with `--accept-permissions` to accept this exact transition.",
        package_name, package_version
    ))
}

fn preserve_existing_data_for_install(
    existing: Option<&InstalledPackageDocument>,
    new_source: &InstalledPackageSourceDocument,
    new_package_name: &str,
    keep_data: bool,
    delete_data: bool,
) -> Result<bool, String> {
    let Some(existing) = existing else {
        return Ok(false);
    };
    if keep_data && delete_data {
        return Err("Choose only one of --keep-data or --delete-data.".to_string());
    }

    let identity_changed = installed_identity_changes(existing, new_source, new_package_name);

    if identity_changed && !keep_data && !delete_data {
        return Err(format!(
            "Replacing package alias `{}` with a different source identity requires an explicit data disposition. Re-run with --keep-data to transfer the existing data after review, or --delete-data to start the new source with empty data.",
            existing.alias
        ));
    }
    if identity_changed {
        return Ok(keep_data);
    }
    if delete_data {
        return Ok(false);
    }
    Ok(true)
}

fn installed_identity_changes(
    existing: &InstalledPackageDocument,
    new_source: &InstalledPackageSourceDocument,
    new_package_name: &str,
) -> bool {
    existing.package_name != new_package_name
        || ((existing.source.kind == "hosted" || new_source.kind == "hosted")
            && (existing.source.kind != new_source.kind
                || existing.source.hosted_source_id != new_source.hosted_source_id))
}

fn permission_profile_expands(
    current: &PackagePermissionProfileDocument,
    candidate: &PackagePermissionProfileDocument,
) -> bool {
    permission_level(candidate.package_payload.as_str())
        > permission_level(current.package_payload.as_str())
        || permission_level(candidate.package_data.as_str())
            > permission_level(current.package_data.as_str())
        || permission_level(candidate.project_workspace.as_str())
            > permission_level(current.project_workspace.as_str())
        || permission_level(candidate.subprocess.as_str())
            > permission_level(current.subprocess.as_str())
}

fn permission_level(value: &str) -> u8 {
    match value {
        "none" => 0,
        "blocked" | "blocked_without_explicit_grant" => 1,
        "explicit_grant_required" => 2,
        "read" => 3,
        "read_write" => 4,
        "allowed" => 5,
        _ => 6,
    }
}

fn validate_permission_profile(
    permissions: &PackagePermissionProfileDocument,
) -> Result<(), String> {
    validate_permission_value(
        permissions.package_payload.as_str(),
        "permissions.package_payload",
        &["read"],
    )?;
    validate_permission_value(
        permissions.package_data.as_str(),
        "permissions.package_data",
        &["read_write"],
    )?;
    validate_permission_value(
        permissions.project_workspace.as_str(),
        "permissions.project_workspace",
        &["none", "explicit_grant_required", "read", "read_write"],
    )?;
    validate_permission_value(
        permissions.subprocess.as_str(),
        "permissions.subprocess",
        &["blocked_without_explicit_grant", "allowed"],
    )
}

fn validate_permission_value(value: &str, label: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(format!(
        "Unsupported package permission `{label} = {}`. Expected one of: {}.",
        value,
        allowed.join(", ")
    ))
}

pub(crate) fn hosted_package_allows_subprocess(context: &InstalledPackageRuntimeContext) -> bool {
    context.source_kind != "hosted" || context.permissions.subprocess == "allowed"
}

pub(crate) fn normalize_portable_relative_path(
    raw_path: &str,
    label: &str,
) -> Result<PathBuf, String> {
    if raw_path.trim().is_empty() {
        return Err(format!("{label} must be a non-empty relative path."));
    }
    if raw_path.contains('\0') {
        return Err(format!("{label} contains an unsupported null byte."));
    }

    let normalized = raw_path.replace('\\', "/");
    if normalized.len() > MAX_PORTABLE_RELATIVE_PATH_BYTES {
        return Err(format!(
            "{label} exceeds the portable {}-byte path limit.",
            MAX_PORTABLE_RELATIVE_PATH_BYTES
        ));
    }
    let bytes = normalized.as_bytes();
    let has_drive_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if normalized.starts_with('/') || has_drive_prefix {
        return Err(format!(
            "{label} '{}' must not use an absolute, drive-relative, UNC, or device-root path.",
            raw_path
        ));
    }

    let mut relative = PathBuf::new();
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(format!(
                "{label} '{}' must not use parent traversal (`..`).",
                raw_path
            ));
        }
        if segment.contains(':') {
            return Err(format!(
                "{label} '{}' contains a non-portable path prefix.",
                raw_path
            ));
        }
        relative.push(segment);
    }
    if relative.as_os_str().is_empty()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!("{label} must be a non-empty relative path."));
    }

    Ok(relative)
}

#[cfg(windows)]
fn metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    windows_file_attributes_are_link_like(metadata.file_attributes())
}

#[cfg(windows)]
fn windows_file_attributes_are_link_like(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

pub(crate) fn resolve_existing_path_under_root(
    root: &Path,
    relative_path: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    let root_metadata = fs::symlink_metadata(root).map_err(|error| {
        format!(
            "Failed to inspect {label} root '{}': {error}",
            root.display()
        )
    })?;
    if metadata_is_link_like(&root_metadata) || !root_metadata.is_dir() {
        return Err(format!(
            "{label} root '{}' must be a real directory and not a symbolic link or reparse point.",
            root.display()
        ));
    }

    let mut resolved = root.to_path_buf();
    for component in relative_path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(segment) => resolved.push(segment),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("{label} must stay beneath '{}'.", root.display()));
            }
        }
        let metadata = fs::symlink_metadata(&resolved).map_err(|error| {
            format!(
                "Failed to inspect {label} '{}': {error}",
                resolved.display()
            )
        })?;
        if metadata_is_link_like(&metadata) {
            return Err(format!(
                "{label} must not traverse symbolic link or reparse point '{}'.",
                resolved.display()
            ));
        }
    }
    Ok(resolved)
}

fn absolute_lexical_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("Failed to resolve current directory: {error}"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    Ok(normalized)
}

fn installed_package_alias_for_path(
    path: &Path,
    package_store_root: &Path,
) -> Result<Option<String>, String> {
    let Ok(relative_path) = path.strip_prefix(package_store_root) else {
        return Ok(None);
    };
    let mut components = relative_path.components();
    let Some(Component::Normal(alias)) = components.next() else {
        return Ok(None);
    };
    let Some(Component::Normal(payload_directory)) = components.next() else {
        return Ok(None);
    };
    if payload_directory != INSTALLED_PACKAGE_DIR_NAME {
        return Ok(None);
    }
    alias
        .to_str()
        .map(|alias| Some(alias.to_string()))
        .ok_or_else(|| {
            format!(
                "Installed package path '{}' contains a non-Unicode alias.",
                path.display()
            )
        })
}

fn validate_installed_package_project_metadata(
    package_root: &Path,
    alias: &str,
) -> Result<(), String> {
    let metadata_path = resolve_existing_path_under_root(
        package_root,
        Path::new(".cargo-ai/project.toml"),
        "Installed package project metadata",
    )?;
    if !metadata_path.is_file() {
        return Err(format!(
            "Installed package alias `{alias}` project metadata '{}' must be a regular file.",
            metadata_path.display()
        ));
    }
    let contents = fs::read_to_string(&metadata_path).map_err(|error| {
        format!(
            "Failed to read installed package alias `{alias}` project metadata '{}': {error}",
            metadata_path.display()
        )
    })?;
    toml::from_str::<toml::Value>(&contents).map_err(|error| {
        format!(
            "Failed to parse installed package alias `{alias}` project metadata '{}': {error}",
            metadata_path.display()
        )
    })?;
    Ok(())
}

pub(crate) fn checked_runtime_lease_for_path(
    candidate_path: &Path,
    required_capability: Option<InstalledEntrypointCapability>,
) -> Result<Option<CheckedInstalledPackageRuntime>, String> {
    let absolute_candidate_path = absolute_lexical_path(candidate_path)?;
    let absolute_packages_root = absolute_lexical_path(packages_root().as_path())?;
    let lexical_alias = installed_package_alias_for_path(
        absolute_candidate_path.as_path(),
        absolute_packages_root.as_path(),
    )?;
    let canonical_candidate_path = fs::canonicalize(candidate_path).map_err(|error| {
        format!(
            "Failed to resolve runtime definition context '{}': {error}",
            candidate_path.display()
        )
    })?;
    let canonical_packages_root = match fs::canonicalize(packages_root()) {
        Ok(root) => root,
        Err(error) if error.kind() == ErrorKind::NotFound && lexical_alias.is_none() => {
            return Ok(None);
        }
        Err(error) => {
            return Err(format!(
                "Failed to resolve package store '{}' while checking runtime definition context '{}': {error}",
                packages_root().display(),
                candidate_path.display()
            ));
        }
    };
    let canonical_alias = installed_package_alias_for_path(
        canonical_candidate_path.as_path(),
        canonical_packages_root.as_path(),
    )?;
    let alias = match (lexical_alias, canonical_alias) {
        (None, None) => return Ok(None),
        (Some(alias), None) | (None, Some(alias)) => alias,
        (Some(lexical), Some(canonical)) if lexical == canonical => lexical,
        (Some(lexical), Some(canonical)) => {
            return Err(format!(
                "Runtime definition context '{}' redirects between installed package aliases `{lexical}` and `{canonical}`.",
                candidate_path.display()
            ));
        }
    };
    let lease = Arc::new(acquire_package_alias_read_lock(alias.as_str())?);
    let package = load_installed_package(alias.as_str())?;
    if package.alias != alias {
        return Err(format!(
            "Installed package metadata for alias `{alias}` declares mismatched alias `{}`.",
            package.alias
        ));
    }
    let expected_install_root = installed_package_root(alias.as_str());
    let expected_package_root = expected_install_root.join(INSTALLED_PACKAGE_DIR_NAME);
    let expected_canonical = fs::canonicalize(&expected_package_root).map_err(|error| {
        format!(
            "Failed to resolve installed package payload root '{}': {error}",
            expected_package_root.display()
        )
    })?;
    if !canonical_candidate_path.starts_with(&expected_canonical) {
        return Err(format!(
            "Runtime definition context '{}' escaped expected installed package payload root '{}'.",
            candidate_path.display(),
            expected_package_root.display()
        ));
    }
    let install_metadata = fs::symlink_metadata(&expected_install_root).map_err(|error| {
        format!(
            "Failed to inspect installed package alias root '{}': {error}",
            expected_install_root.display()
        )
    })?;
    let package_metadata = fs::symlink_metadata(&expected_package_root).map_err(|error| {
        format!(
            "Failed to inspect installed package payload root '{}': {error}",
            expected_package_root.display()
        )
    })?;
    if metadata_is_link_like(&install_metadata)
        || !install_metadata.is_dir()
        || metadata_is_link_like(&package_metadata)
        || !package_metadata.is_dir()
    {
        return Err(format!(
            "Installed package paths for alias `{alias}` must be real directories and not symbolic links or reparse points."
        ));
    }
    validate_installed_package_project_metadata(expected_package_root.as_path(), alias.as_str())?;

    let mut context = InstalledPackageRuntimeContext {
        alias: package.alias,
        source_kind: package.source.kind,
        package_payload_root: expected_package_root,
        package_data_root: expected_install_root.join(INSTALLED_PACKAGE_DATA_DIR_NAME),
        current_entrypoint_path: None,
        entrypoints: package.entrypoints,
        permissions: package.permissions,
    };
    if let Some(required_capability) = required_capability {
        let mut matching_entrypoint = None;
        for entrypoint in &context.entrypoints {
            let relative_path = normalize_portable_relative_path(
                entrypoint.path.as_str(),
                "Installed package entrypoint path",
            )?;
            let resolved = resolve_existing_path_under_root(
                context.package_payload_root.as_path(),
                relative_path.as_path(),
                "Installed package entrypoint",
            )?;
            let canonical = fs::canonicalize(&resolved).map_err(|error| {
                format!(
                    "Failed to resolve installed package entrypoint '{}': {error}",
                    resolved.display()
                )
            })?;
            if canonical == canonical_candidate_path {
                matching_entrypoint = Some(entrypoint);
            }
        }
        let entrypoint = matching_entrypoint.ok_or_else(|| {
            format!(
                "Runtime definition '{}' is inside installed package alias `{alias}` but is not an exported package entrypoint.",
                candidate_path.display()
            )
        })?;
        if !entrypoint.runnable {
            return Err(format!(
                "Installed package entrypoint `{}::{}` is not runnable.",
                alias, entrypoint.name
            ));
        }
        if required_capability == InstalledEntrypointCapability::Hatch && !entrypoint.hatchable {
            return Err(format!(
                "Installed package entrypoint `{}::{}` is not hatchable.",
                alias, entrypoint.name
            ));
        }
        context.current_entrypoint_path = Some(entrypoint.path.clone());
    }

    Ok(Some(CheckedInstalledPackageRuntime { context, lease }))
}

pub(crate) fn checked_runtime_context_for_path(
    candidate_path: &Path,
) -> Result<Option<InstalledPackageRuntimeContext>, String> {
    checked_runtime_lease_for_path(candidate_path, None)
        .map(|checked| checked.map(|checked| checked.context))
}

pub(crate) fn checked_runtime_context_for_project_root(
    project_root: &Path,
) -> Result<Option<InstalledPackageRuntimeContext>, String> {
    checked_runtime_context_for_path(project_root)
}

pub(crate) fn runtime_context_for_package_root(
    package_root: &Path,
) -> Option<InstalledPackageRuntimeContext> {
    checked_runtime_context_for_project_root(package_root)
        .ok()
        .flatten()
}

pub(crate) fn runtime_context_for_resolved_entrypoint(
    resolved: &ResolvedPackageEntrypoint,
) -> Result<InstalledPackageRuntimeContext, String> {
    let mut context = checked_runtime_context_for_project_root(resolved.package_root.as_path())?
        .ok_or_else(|| {
            format!(
                "Installed package entrypoint `{}::{}` did not resolve to the verified package root for alias `{}`.",
                resolved.alias, resolved.entrypoint, resolved.alias
            )
        })?;
    let entrypoint_path = context
        .entrypoints
        .iter()
        .find(|entrypoint| entrypoint.name == resolved.entrypoint && entrypoint.runnable)
        .map(|entrypoint| entrypoint.path.clone())
        .ok_or_else(|| {
            format!(
                "Installed package alias `{}` no longer exports runnable entrypoint `{}`.",
                resolved.alias, resolved.entrypoint
            )
        })?;
    context.current_entrypoint_path = Some(entrypoint_path);
    Ok(context)
}

pub(crate) fn resolve_package_runtime_tools_root(
    context: &InstalledPackageRuntimeContext,
) -> Result<Option<PathBuf>, String> {
    let package_install_root = context.package_payload_root.parent().ok_or_else(|| {
        format!(
            "Package `{}` payload root '{}' has no installed alias parent.",
            context.alias,
            context.package_payload_root.display()
        )
    })?;
    let data_install_root = context.package_data_root.parent().ok_or_else(|| {
        format!(
            "Package `{}` data root '{}' has no installed alias parent.",
            context.alias,
            context.package_data_root.display()
        )
    })?;
    if package_install_root != data_install_root {
        return Err(format!(
            "Package `{}` payload and data roots do not share the same installed alias root.",
            context.alias
        ));
    }
    let runtime_root = package_install_root.join(INSTALLED_PACKAGE_RUNTIME_DIR_NAME);
    match fs::symlink_metadata(&runtime_root) {
        Ok(metadata) if metadata_is_link_like(&metadata) || !metadata.is_dir() => {
            return Err(format!(
                "Package `{}` runtime root '{}' must be a real directory and not a symbolic link or reparse point.",
                context.alias,
                runtime_root.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to inspect package `{}` runtime root '{}': {}",
                context.alias,
                runtime_root.display(),
                error
            ));
        }
    }
    let candidate_runtime_tools_root = runtime_root.join(INSTALLED_PACKAGE_RUNTIME_TOOLS_DIR_NAME);
    match fs::symlink_metadata(&candidate_runtime_tools_root) {
        Ok(metadata) if metadata_is_link_like(&metadata) || !metadata.is_dir() => {
            return Err(format!(
                "Package `{}` runtime tools root '{}' must be a real directory and not a symbolic link or reparse point.",
                context.alias,
                candidate_runtime_tools_root.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to inspect package `{}` runtime tools root '{}': {}",
                context.alias,
                candidate_runtime_tools_root.display(),
                error
            ));
        }
    }
    let runtime_tools_root = resolve_existing_path_under_root(
        package_install_root,
        Path::new(INSTALLED_PACKAGE_RUNTIME_DIR_NAME)
            .join(INSTALLED_PACKAGE_RUNTIME_TOOLS_DIR_NAME)
            .as_path(),
        format!("Package `{}` runtime tools", context.alias).as_str(),
    )?;
    if !runtime_tools_root.is_dir() {
        return Err(format!(
            "Package `{}` runtime tools root '{}' must be a real directory.",
            context.alias,
            runtime_tools_root.display()
        ));
    }
    Ok(Some(runtime_tools_root))
}

pub(crate) fn resolve_package_payload_path(
    context: &InstalledPackageRuntimeContext,
    relative_path: &Path,
) -> Result<PathBuf, String> {
    let normalized = normalize_portable_relative_path(
        relative_path.to_string_lossy().as_ref(),
        format!("Package `{}` payload path", context.alias).as_str(),
    )?;
    resolve_existing_path_under_root(
        context.package_payload_root.as_path(),
        normalized.as_path(),
        format!("Package `{}` payload path", context.alias).as_str(),
    )
}

pub(crate) fn resolve_package_payload_path_from_current_entrypoint(
    context: &InstalledPackageRuntimeContext,
    relative_path: &str,
) -> Result<(String, PathBuf), String> {
    let child_path = normalize_portable_relative_path(
        relative_path,
        format!("Package `{}` relative payload path", context.alias).as_str(),
    )?;
    let current_entrypoint = context.current_entrypoint_path.as_deref().ok_or_else(|| {
        format!(
            "Package `{}` runtime context is missing the current exported entrypoint.",
            context.alias
        )
    })?;
    let current_entrypoint = normalize_portable_relative_path(
        current_entrypoint,
        format!("Package `{}` current entrypoint path", context.alias).as_str(),
    )?;
    let package_relative = current_entrypoint
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(child_path);
    let package_relative_text = package_relative.to_string_lossy().replace('\\', "/");
    let package_relative = normalize_portable_relative_path(
        package_relative_text.as_str(),
        format!("Package `{}` resolved payload path", context.alias).as_str(),
    )?;
    let resolved = resolve_package_payload_path(context, package_relative.as_path())?;
    Ok((
        package_relative.to_string_lossy().replace('\\', "/"),
        resolved,
    ))
}

pub(crate) fn resolve_package_data_path(
    context: &InstalledPackageRuntimeContext,
    relative_path: &Path,
) -> Result<PathBuf, String> {
    let relative_path = normalize_portable_relative_path(
        relative_path.to_string_lossy().as_ref(),
        format!("Package `{}` data path", context.alias).as_str(),
    )?;

    super::runtime_data::confined_path(
        &context.package_data_root,
        &relative_path,
        &format!("Package `{}` data path", context.alias),
    )
}

fn permission_profile_lines(permissions: &PackagePermissionProfileDocument) -> Vec<String> {
    let mut lines = vec![
        format!("package payload: {}", permissions.package_payload),
        format!("package data:    {}", permissions.package_data),
        format!("project writes:  {}", permissions.project_workspace),
        format!("subprocess:      {}", permissions.subprocess),
    ];
    if permissions.subprocess == "allowed" {
        lines.push(
            "warning: hosted `cargo build` may execute publisher build scripts and proc macros as unsandboxed code with the current user's ambient filesystem, environment, and network authority; install only trusted packages."
                .to_string(),
        );
    }
    lines
}

fn print_permission_summary(permissions: &PackagePermissionProfileDocument) {
    println!("Permissions:");
    for line in permission_profile_lines(permissions) {
        println!("  {line}");
    }
}

fn validate_hosted_response_matches_request(
    response: &Value,
    requested_package_name: &str,
    requested_version: Option<&str>,
) -> Result<(), String> {
    let resolved_package_name = optional_response_string(response, "display_name")
        .unwrap_or(required_response_string(response, "project")?);
    if !requested_package_name.is_empty() && resolved_package_name != requested_package_name {
        return Err(format!(
            "Hosted pull response returned package `{resolved_package_name}` for requested package `{requested_package_name}`."
        ));
    }

    let resolved_version_raw = required_response_string(response, "project_version")?;
    let resolved_version = Version::parse(resolved_version_raw.as_str()).map_err(|error| {
        format!(
            "Hosted pull response returned invalid package version `{resolved_version_raw}`: {error}"
        )
    })?;
    if let Some(requested_version_raw) = requested_version {
        let requested_version = Version::parse(requested_version_raw).map_err(|error| {
            format!(
                "Requested hosted package version `{requested_version_raw}` is invalid: {error}"
            )
        })?;
        if resolved_version != requested_version {
            return Err(format!(
                "Hosted pull response returned version {resolved_version} for exact requested version {requested_version}."
            ));
        }
    }
    Ok(())
}

fn validate_hosted_response_provenance(
    response: &Value,
    requested_owner_handle: Option<&str>,
    requested_hosted_source_id: Option<&str>,
) -> Result<(), String> {
    if let Some(requested_source_id) = requested_hosted_source_id {
        let returned_source_id = required_response_string(response, "hosted_source_id")?;
        if returned_source_id != requested_source_id {
            return Err(format!(
                "Hosted pull response returned source id `{returned_source_id}` for requested source id `{requested_source_id}`."
            ));
        }
    }

    if let Some(requested_handle) = requested_owner_handle {
        let returned_handle = required_response_string(response, "owner_handle")?;
        let normalized_requested = requested_handle.trim().to_ascii_lowercase();
        let normalized_returned = returned_handle.trim().to_ascii_lowercase();
        if normalized_returned != normalized_requested {
            return Err(format!(
                "Hosted pull response returned owner handle `{returned_handle}` for requested owner `{requested_handle}`."
            ));
        }
    }
    Ok(())
}

fn validate_package_archive_size(compressed_bytes: usize) -> Result<(), String> {
    if compressed_bytes > MAX_PACKAGE_ARCHIVE_COMPRESSED_BYTES {
        return Err(format!(
            "Package archive is {} bytes after decoding; the client limit is {} bytes.",
            compressed_bytes, MAX_PACKAGE_ARCHIVE_COMPRESSED_BYTES
        ));
    }
    Ok(())
}

fn decode_package_archive_base64(archive_base64: &str) -> Result<Vec<u8>, String> {
    if archive_base64.len() > MAX_PACKAGE_ARCHIVE_BASE64_BYTES {
        return Err(format!(
            "Package archive base64 payload exceeds the {}-byte client limit.",
            MAX_PACKAGE_ARCHIVE_BASE64_BYTES
        ));
    }
    let archive_bytes = BASE64_STANDARD
        .decode(archive_base64.as_bytes())
        .map_err(|error| format!("Failed to decode hosted package archive: {error}"))?;
    validate_package_archive_size(archive_bytes.len())?;
    Ok(archive_bytes)
}

fn backend_response_message(response: &Value, fallback: &str) -> String {
    response
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            response
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            response
                .get("type")
                .and_then(Value::as_str)
                .map(|kind| format!("{fallback} Backend response type: {kind}."))
        })
        .unwrap_or_else(|| fallback.to_string())
}

fn required_response_string(response: &Value, field: &str) -> Result<String, String> {
    response
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("Hosted pull response did not include `{field}`."))
}

fn optional_response_string(response: &Value, field: &str) -> Option<String> {
    response
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn validate_hosted_response_matches_manifest(
    response: &Value,
    manifest: &PackageManifestDocument,
) -> Result<(), String> {
    let response_project = required_response_string(response, "project")?;
    let response_version = required_response_string(response, "project_version")?;
    let manifest_project = required_package_name(manifest)?;
    let manifest_version = required_package_version(manifest)?;
    if response_project != manifest_project {
        return Err(format!(
            "Hosted package response project `{}` did not match package manifest project `{}`.",
            response_project, manifest_project
        ));
    }
    if response_version != manifest_version {
        return Err(format!(
            "Hosted package response version `{}` did not match package manifest version `{}`.",
            response_version, manifest_version
        ));
    }
    Ok(())
}

fn installed_package_runtime_is_complete(
    alias: &str,
    declared_tools: &[String],
    materialized_tools_required: bool,
) -> Result<bool, String> {
    let install_root = installed_package_root(alias);
    let runtime_root = install_root.join(INSTALLED_PACKAGE_RUNTIME_DIR_NAME);
    let runtime_metadata = match fs::symlink_metadata(&runtime_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Failed to inspect package alias `{alias}` runtime root '{}': {}",
                runtime_root.display(),
                error
            ));
        }
    };
    if metadata_is_link_like(&runtime_metadata) || !runtime_metadata.is_dir() {
        return Err(format!(
            "Package alias `{alias}` runtime root '{}' must be a real directory and not a symbolic link or reparse point.",
            runtime_root.display()
        ));
    }
    let runtime_tools_root = runtime_root.join(INSTALLED_PACKAGE_RUNTIME_TOOLS_DIR_NAME);
    let runtime_tools_metadata = match fs::symlink_metadata(&runtime_tools_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Failed to inspect package alias `{alias}` runtime tools root '{}': {}",
                runtime_tools_root.display(),
                error
            ));
        }
    };
    if metadata_is_link_like(&runtime_tools_metadata) || !runtime_tools_metadata.is_dir() {
        return Err(format!(
            "Package alias `{alias}` runtime tools root '{}' must be a real directory and not a symbolic link or reparse point.",
            runtime_tools_root.display()
        ));
    }
    ensure_runtime_tree_is_safe(runtime_tools_root.as_path(), alias)?;
    if !materialized_tools_required {
        return fs::read_dir(&runtime_tools_root)
            .map_err(|error| {
                format!(
                    "Failed to inspect package alias `{alias}` runtime tools root '{}': {}",
                    runtime_tools_root.display(),
                    error
                )
            })?
            .next()
            .transpose()
            .map(|entry| entry.is_none())
            .map_err(|error| {
                format!(
                    "Failed to inspect an entry under package alias `{alias}` runtime tools root '{}': {}",
                    runtime_tools_root.display(),
                    error
                )
            });
    }

    for tool_name in declared_tools {
        let tool_relative = normalize_portable_relative_path(
            tool_name,
            format!("Package alias `{alias}` runtime tool name").as_str(),
        )?;
        if tool_relative.components().count() != 1 {
            return Err(format!(
                "Package alias `{alias}` runtime tool name '{}' must be a single portable path component.",
                tool_name
            ));
        }
        let tool_root = runtime_tools_root.join(tool_relative);
        let tool_metadata = match fs::symlink_metadata(&tool_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "Failed to inspect package alias `{alias}` runtime tool '{}' root '{}': {}",
                    tool_name,
                    tool_root.display(),
                    error
                ));
            }
        };
        if metadata_is_link_like(&tool_metadata) {
            return Err(format!(
                "Package alias `{alias}` runtime tool '{}' root '{}' must not be a symbolic link or reparse point.",
                tool_name,
                tool_root.display()
            ));
        }
        if !tool_metadata.is_dir() {
            return Ok(false);
        }
        let manifest_path = tool_root.join("tool.json");
        let manifest_metadata = match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "Failed to inspect package alias `{alias}` runtime tool '{}' metadata '{}': {}",
                    tool_name,
                    manifest_path.display(),
                    error
                ));
            }
        };
        if metadata_is_link_like(&manifest_metadata) {
            return Err(format!(
                "Package alias `{alias}` runtime tool '{}' metadata '{}' must not be a symbolic link or reparse point.",
                tool_name,
                manifest_path.display()
            ));
        }
        if !manifest_metadata.is_file() {
            return Ok(false);
        }
        if !runtime_tool_manifest_artifact_paths_are_safe(
            manifest_path.as_path(),
            alias,
            tool_name,
        )? {
            return Ok(false);
        }
    }
    match crate::commands::tools::validate_package_runtime_tools(
        runtime_tools_root.as_path(),
        declared_tools,
        crate::cargo_ai_metadata::current_build_target().as_str(),
    ) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

fn ensure_runtime_tree_is_safe(runtime_tools_root: &Path, alias: &str) -> Result<(), String> {
    let entries = fs::read_dir(runtime_tools_root).map_err(|error| {
        format!(
            "Failed to inspect package alias `{alias}` runtime tools tree '{}': {}",
            runtime_tools_root.display(),
            error
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to inspect an entry under package alias `{alias}` runtime tools tree '{}': {}",
                runtime_tools_root.display(),
                error
            )
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "Failed to inspect package alias `{alias}` runtime path '{}': {}",
                path.display(),
                error
            )
        })?;
        if metadata_is_link_like(&metadata) {
            return Err(format!(
                "Package alias `{alias}` runtime path '{}' must not be a symbolic link or reparse point.",
                path.display()
            ));
        }
        if metadata.is_dir() {
            ensure_runtime_tree_is_safe(path.as_path(), alias)?;
        } else if !metadata.is_file() {
            return Err(format!(
                "Package alias `{alias}` runtime path '{}' must be a regular file or directory.",
                path.display()
            ));
        }
    }
    Ok(())
}

fn runtime_tool_manifest_artifact_paths_are_safe(
    manifest_path: &Path,
    alias: &str,
    tool_name: &str,
) -> Result<bool, String> {
    let contents = fs::read_to_string(manifest_path).map_err(|error| {
        format!(
            "Failed to read package alias `{alias}` runtime tool '{}' metadata '{}': {}",
            tool_name,
            manifest_path.display(),
            error
        )
    })?;
    let manifest: Value = match serde_json::from_str(contents.as_str()) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    let Some(artifacts) = manifest.get("artifacts").and_then(Value::as_object) else {
        return Ok(false);
    };
    for artifact in artifacts.values() {
        let Some(path) = artifact
            .as_object()
            .and_then(|artifact| artifact.get("path"))
            .and_then(Value::as_str)
        else {
            return Ok(false);
        };
        normalize_portable_relative_path(
            path,
            format!("Package alias `{alias}` runtime tool '{tool_name}' artifact path").as_str(),
        )?;
    }
    Ok(true)
}

fn write_staged_install(
    alias: &str,
    package_root: &Path,
    document: &InstalledPackageDocument,
    preserve_existing_data: bool,
    declared_tools: &[String],
    materialize_source_tools: bool,
) -> Result<(), String> {
    let packages_root = packages_root();
    let packages_staging_root = ensure_packages_staging_root()?;
    let transaction_id = Uuid::new_v4();
    let staging_root = packages_staging_root.join(format!("{alias}-{transaction_id}-replacement"));
    let backup_root = packages_staging_root.join(format!("{alias}-{transaction_id}-backup"));
    let staged_package_root = staging_root.join(INSTALLED_PACKAGE_DIR_NAME);
    let prepare_result = (|| {
        fs::create_dir(&staging_root).map_err(|error| {
            format!(
                "Failed to create staged package transaction directory '{}': {}",
                staging_root.display(),
                error
            )
        })?;
        fs::create_dir(&staged_package_root).map_err(|error| {
            format!(
                "Failed to create staged package directory '{}': {}",
                staged_package_root.display(),
                error
            )
        })?;
        let staging_metadata = fs::symlink_metadata(&staging_root).map_err(|error| {
            format!(
                "Failed to inspect staged package root '{}': {}",
                staging_root.display(),
                error
            )
        })?;
        let staged_package_metadata =
            fs::symlink_metadata(&staged_package_root).map_err(|error| {
                format!(
                    "Failed to inspect staged package payload '{}': {}",
                    staged_package_root.display(),
                    error
                )
            })?;
        if metadata_is_link_like(&staging_metadata)
            || !staging_metadata.is_dir()
            || metadata_is_link_like(&staged_package_metadata)
            || !staged_package_metadata.is_dir()
        {
            return Err(format!(
                "Staged package paths under '{}' must be real directories and not symbolic links or reparse points.",
                staging_root.display()
            ));
        }
        copy_directory_recursive(package_root, staged_package_root.as_path())?;
        let staged_archive_before_materialization =
            crate::commands::account::create_package_archive_bytes(&staged_package_root)?;
        let staged_payload_sha256_before_materialization =
            crate::commands::account::sha256_hex(staged_archive_before_materialization.as_slice());
        if document.source.kind != "hosted"
            && staged_payload_sha256_before_materialization != document.content_sha256
        {
            return Err(format!(
                "Package payload changed while staging: expected SHA-256 {}, found {}.",
                document.content_sha256, staged_payload_sha256_before_materialization
            ));
        }
        let staged_runtime_tools_root = ensure_directory_path_under_root(
            staging_root.as_path(),
            Path::new(INSTALLED_PACKAGE_RUNTIME_DIR_NAME)
                .join(INSTALLED_PACKAGE_RUNTIME_TOOLS_DIR_NAME)
                .as_path(),
            "Staged package runtime tools directory",
        )?;
        let tool_build_scratch_guard = if materialize_source_tools {
            Some(create_tool_build_scratch_root(staging_root.as_path())?)
        } else {
            None
        };
        if let Some(tool_build_scratch_guard) = tool_build_scratch_guard.as_ref() {
            let target = crate::cargo_ai_metadata::current_build_target();
            let build_target =
                crate::agent_builder::build_target::BuildTarget::from_cli(Some(target.as_str()))?;
            let mut materialized = BTreeSet::new();
            for tool_name in declared_tools {
                if materialized.insert(tool_name.as_str()) {
                    crate::commands::tools::materialize_source_tool_for_package_runtime(
                        tool_name,
                        &build_target,
                        staged_package_root.as_path(),
                        staged_runtime_tools_root.as_path(),
                        tool_build_scratch_guard.path(),
                    )?;
                }
            }
            crate::commands::tools::validate_package_runtime_tools(
                staged_runtime_tools_root.as_path(),
                declared_tools,
                target.as_str(),
            )?;
        }
        if let Some(tool_build_scratch_guard) = tool_build_scratch_guard {
            tool_build_scratch_guard.cleanup("temporary package tool build root")?;
        }
        let staged_archive =
            crate::commands::account::create_package_archive_bytes(&staged_package_root)?;
        let staged_content_sha256 = crate::commands::account::sha256_hex(staged_archive.as_slice());
        if staged_content_sha256 != staged_payload_sha256_before_materialization {
            return Err(format!(
                "Installed package payload changed while materializing runtime tools: expected SHA-256 {}, found {}.",
                staged_payload_sha256_before_materialization, staged_content_sha256
            ));
        }
        write_install_manifest(
            staging_root.join(INSTALL_MANIFEST_FILE_NAME).as_path(),
            document,
        )
    })();
    if let Err(error) = prepare_result {
        let _ = fs::remove_dir_all(&staging_root);
        return Err(error);
    }

    let staged_data_root = staging_root.join(INSTALLED_PACKAGE_DATA_DIR_NAME);
    let install_root = packages_root.join(alias);
    let install_metadata = match fs::symlink_metadata(&install_root) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_root);
            return Err(format!(
                "Failed to inspect installed package alias '{}': {}",
                install_root.display(),
                error
            ));
        }
    };

    let Some(install_metadata) = install_metadata else {
        fs::create_dir_all(&staged_data_root).map_err(|error| {
            let _ = fs::remove_dir_all(&staging_root);
            format!(
                "Failed to create staged package data directory '{}': {}",
                staged_data_root.display(),
                error
            )
        })?;
        if let Err(error) =
            maybe_fail_staged_install(StagedInstallFailurePoint::FinalAliasReplacement)
        {
            let _ = fs::remove_dir_all(&staging_root);
            return Err(error);
        }
        return fs::rename(&staging_root, &install_root).map_err(|error| {
            let _ = fs::remove_dir_all(&staging_root);
            format!(
                "Failed to move staged package install from '{}' to '{}': {}",
                staging_root.display(),
                install_root.display(),
                error
            )
        });
    };

    if metadata_is_link_like(&install_metadata) || !install_metadata.is_dir() {
        let _ = fs::remove_dir_all(&staging_root);
        return Err(format!(
            "Installed package alias '{}' at '{}' must be a real directory and not a symbolic link or reparse point.",
            alias,
            install_root.display()
        ));
    }

    let current_data_root = install_root.join(INSTALLED_PACKAGE_DATA_DIR_NAME);
    let existing_data_available = match fs::symlink_metadata(&current_data_root) {
        Ok(metadata) if metadata_is_link_like(&metadata) || !metadata.is_dir() => {
            let _ = fs::remove_dir_all(&staging_root);
            return Err(format!(
                "Installed package alias '{}' has an unsafe data root at '{}'; expected a real directory and not a symbolic link or reparse point.",
                alias,
                current_data_root.display()
            ));
        }
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_root);
            return Err(format!(
                "Failed to inspect package data directory '{}' while replacing alias '{}': {}",
                current_data_root.display(),
                alias,
                error
            ));
        }
    };

    fs::rename(&install_root, &backup_root).map_err(|error| {
        let _ = fs::remove_dir_all(&staging_root);
        format!(
            "Failed to create recoverable backup '{}' for package alias '{}': {}",
            backup_root.display(),
            alias,
            error
        )
    })?;
    let backup_metadata = maybe_fail_staged_install(StagedInstallFailurePoint::BackupInspection)
        .and_then(|_| {
            fs::symlink_metadata(&backup_root).map_err(|error| {
                format!(
                    "Failed to inspect recoverable backup '{}' for package alias '{}': {}",
                    backup_root.display(),
                    alias,
                    error
                )
            })
        });
    let backup_metadata = match backup_metadata {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(staged_install_failure_with_recovery(
                alias,
                error,
                &install_root,
                &backup_root,
                &staging_root,
                false,
            ));
        }
    };
    if metadata_is_link_like(&backup_metadata) || !backup_metadata.is_dir() {
        return Err(staged_install_failure_with_recovery(
            alias,
            format!(
                "Recoverable backup '{}' is not a real directory.",
                backup_root.display()
            ),
            &install_root,
            &backup_root,
            &staging_root,
            false,
        ));
    }

    if let Err(error) = maybe_fail_staged_install(StagedInstallFailurePoint::AfterBackup) {
        return Err(staged_install_failure_with_recovery(
            alias,
            error,
            &install_root,
            &backup_root,
            &staging_root,
            false,
        ));
    }

    let preserved_data_moved = if preserve_existing_data && existing_data_available {
        let backup_data_root = backup_root.join(INSTALLED_PACKAGE_DATA_DIR_NAME);
        if let Err(error) = fs::rename(&backup_data_root, &staged_data_root) {
            return Err(staged_install_failure_with_recovery(
                alias,
                format!(
                    "Failed to preserve package data directory '{}' while replacing alias '{}': {}",
                    backup_data_root.display(),
                    alias,
                    error
                ),
                &install_root,
                &backup_root,
                &staging_root,
                false,
            ));
        }
        true
    } else {
        if let Err(error) = fs::create_dir_all(&staged_data_root) {
            return Err(staged_install_failure_with_recovery(
                alias,
                format!(
                    "Failed to create staged package data directory '{}': {}",
                    staged_data_root.display(),
                    error
                ),
                &install_root,
                &backup_root,
                &staging_root,
                false,
            ));
        }
        false
    };

    if let Err(error) = maybe_fail_staged_install(StagedInstallFailurePoint::AfterDataTransfer) {
        return Err(staged_install_failure_with_recovery(
            alias,
            error,
            &install_root,
            &backup_root,
            &staging_root,
            preserved_data_moved,
        ));
    }

    let replacement_result = maybe_fail_staged_install(
        StagedInstallFailurePoint::FinalAliasReplacement,
    )
    .and_then(|_| {
        fs::rename(&staging_root, &install_root).map_err(|error| {
            format!(
                "Failed to move staged package install from '{}' to '{}': {}",
                staging_root.display(),
                install_root.display(),
                error
            )
        })
    });
    if let Err(error) = replacement_result {
        return Err(staged_install_failure_with_recovery(
            alias,
            error,
            &install_root,
            &backup_root,
            &staging_root,
            preserved_data_moved,
        ));
    }

    if let Err(error) = fs::remove_dir_all(&backup_root) {
        eprintln!(
            "Warning: package alias `{alias}` was replaced, but the prior package backup '{}' could not be removed: {error}",
            backup_root.display()
        );
    }
    Ok(())
}

fn staged_install_failure_with_recovery(
    alias: &str,
    failure: String,
    install_root: &Path,
    backup_root: &Path,
    staging_root: &Path,
    preserved_data_moved: bool,
) -> String {
    match restore_previous_install(
        install_root,
        backup_root,
        staging_root,
        preserved_data_moved,
    ) {
        Ok(()) => format!("{failure} Previous package alias `{alias}` was restored."),
        Err(recovery_error) => format!(
            "{failure} Automatic recovery for package alias `{alias}` failed: {recovery_error} Backup retained at '{}'; staged replacement retained at '{}'.",
            backup_root.display(),
            staging_root.display()
        ),
    }
}

fn restore_previous_install(
    install_root: &Path,
    backup_root: &Path,
    staging_root: &Path,
    preserved_data_moved: bool,
) -> Result<(), String> {
    if preserved_data_moved {
        maybe_fail_staged_install(StagedInstallFailurePoint::RestoreData)?;
        let staged_data_root = staging_root.join(INSTALLED_PACKAGE_DATA_DIR_NAME);
        let backup_data_root = backup_root.join(INSTALLED_PACKAGE_DATA_DIR_NAME);
        fs::rename(&staged_data_root, &backup_data_root).map_err(|error| {
            format!(
                "failed to restore package data from '{}' to '{}': {}",
                staged_data_root.display(),
                backup_data_root.display(),
                error
            )
        })?;
    }

    maybe_fail_staged_install(StagedInstallFailurePoint::RestoreAlias)?;
    fs::rename(backup_root, install_root).map_err(|error| {
        format!(
            "failed to restore package alias from '{}' to '{}': {}",
            backup_root.display(),
            install_root.display(),
            error
        )
    })?;
    let _ = fs::remove_dir_all(staging_root);
    Ok(())
}

fn maybe_fail_staged_install(point: StagedInstallFailurePoint) -> Result<(), String> {
    #[cfg(test)]
    {
        let should_fail = TEST_STAGED_INSTALL_FAILURES.with(|failures| {
            let mut failures = failures.borrow_mut();
            if failures.first().copied() == Some(point) {
                failures.remove(0);
                true
            } else {
                false
            }
        });
        if should_fail {
            return Err(format!(
                "Injected staged install failure at `{point:?}` for testing."
            ));
        }
    }
    #[cfg(not(test))]
    let _ = point;
    Ok(())
}

#[cfg(test)]
fn set_test_staged_install_failures(points: &[StagedInstallFailurePoint]) {
    TEST_STAGED_INSTALL_FAILURES.with(|failures| {
        failures.replace(points.to_vec());
    });
}

fn write_install_manifest(path: &Path, document: &InstalledPackageDocument) -> Result<(), String> {
    let rendered = toml::to_string_pretty(document)
        .map_err(|error| format!("Failed to render install metadata TOML: {error}"))?;
    fs::write(path, rendered).map_err(|error| {
        format!(
            "Failed to write install metadata '{}': {}",
            path.display(),
            error
        )
    })
}

fn list_installed_packages() -> Result<Vec<InstalledPackageDocument>, String> {
    let root = packages_root();
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata_is_link_like(&metadata) || !metadata.is_dir() => {
            return Err(format!(
                "Package store '{}' must be a real directory and not a symbolic link or reparse point.",
                root.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "Failed to inspect package store '{}': {}",
                root.display(),
                error
            ));
        }
    }
    let mut packages = Vec::new();
    for entry in fs::read_dir(root.as_path()).map_err(|error| {
        format!(
            "Failed to read package store '{}': {}",
            root.display(),
            error
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to read package store entry under '{}': {}",
                root.display(),
                error
            )
        })?;
        let alias = entry.file_name().to_string_lossy().to_string();
        if alias.starts_with('.') {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            format!(
                "Failed to inspect package store entry '{}': {}",
                entry.path().display(),
                error
            )
        })?;
        if metadata_is_link_like(&metadata) {
            return Err(format!(
                "Installed package alias '{}' must not be a symbolic link or reparse point.",
                alias
            ));
        }
        if !metadata.is_dir() {
            continue;
        }
        packages.push(load_installed_package(alias.as_str())?);
    }
    Ok(packages)
}

fn load_installed_package(alias: &str) -> Result<InstalledPackageDocument, String> {
    validate_package_alias(alias)?;
    let metadata_path = resolve_existing_path_under_root(
        packages_root().as_path(),
        Path::new(alias).join(INSTALL_MANIFEST_FILE_NAME).as_path(),
        "Installed package metadata",
    )?;
    if !metadata_path.is_file() {
        return Err(format!(
            "Installed package metadata '{}' must be a regular file.",
            metadata_path.display()
        ));
    }
    let contents = fs::read_to_string(&metadata_path).map_err(|error| {
        format!(
            "Failed to read installed package metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    toml::from_str(contents.as_str()).map_err(|error| {
        format!(
            "Failed to parse installed package metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })
}

fn load_installed_package_if_present(
    alias: &str,
) -> Result<Option<InstalledPackageDocument>, String> {
    validate_package_alias(alias)?;
    match fs::symlink_metadata(installed_package_root(alias)) {
        Ok(_) => load_installed_package(alias).map(Some),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "Failed to inspect installed package alias `{alias}` before mutation: {error}"
        )),
    }
}

fn uninstall_package(alias: &str, delete_data: bool) -> Result<(), String> {
    validate_package_alias(alias)?;
    let _lock = acquire_package_alias_lock(alias)?;
    let packages_root = packages_root();
    let packages_metadata = fs::symlink_metadata(&packages_root).map_err(|error| {
        format!(
            "Failed to inspect package store '{}': {}",
            packages_root.display(),
            error
        )
    })?;
    if metadata_is_link_like(&packages_metadata) || !packages_metadata.is_dir() {
        return Err(format!(
            "Package store '{}' must be a real directory and not a symbolic link or reparse point.",
            packages_root.display()
        ));
    }
    let candidate_root = installed_package_root(alias);
    match fs::symlink_metadata(&candidate_root) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(format!("Package alias `{alias}` is not installed."));
        }
        Err(error) => {
            return Err(format!(
                "Failed to inspect installed package alias '{}': {}",
                candidate_root.display(),
                error
            ));
        }
    }
    let install_root = resolve_existing_path_under_root(
        packages_root.as_path(),
        Path::new(alias),
        "Installed package alias",
    )?;
    if !install_root.is_dir() {
        return Err(format!(
            "Installed package alias '{}' must be a real directory.",
            alias
        ));
    }
    let data_root = install_root.join(INSTALLED_PACKAGE_DATA_DIR_NAME);
    let data_is_nonempty = match fs::symlink_metadata(&data_root) {
        Ok(metadata) if metadata_is_link_like(&metadata) || !metadata.is_dir() => {
            return Err(format!(
                "Package alias `{alias}` has an unsafe data root at '{}'; expected a real directory and not a symbolic link or reparse point.",
                data_root.display()
            ));
        }
        Ok(_) => fs::read_dir(&data_root)
            .map_err(|error| {
                format!(
                    "Failed to inspect package alias `{alias}` data at '{}': {}",
                    data_root.display(),
                    error
                )
            })?
            .next()
            .transpose()
            .map_err(|error| {
                format!(
                    "Failed to inspect package alias `{alias}` data at '{}': {}",
                    data_root.display(),
                    error
                )
            })?
            .is_some(),
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "Failed to inspect package alias `{alias}` data at '{}': {}",
                data_root.display(),
                error
            ));
        }
    };
    if data_is_nonempty && !delete_data {
        return Err(format!(
            "Package alias `{alias}` has persistent data at '{}'. Back up or export that directory, then re-run with --delete-data to confirm permanent deletion.",
            data_root.display()
        ));
    }
    fs::remove_dir_all(&install_root).map_err(|error| {
        format!(
            "Failed to uninstall package alias '{}' from '{}': {}",
            alias,
            install_root.display(),
            error
        )
    })
}

fn cleanup_prepared_package(prepared: &PreparedPackage) {
    if let Some(path) = prepared.temporary_root.as_ref() {
        let _ = fs::remove_dir_all(path);
    }
}

fn copy_directory_recursive(source: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|error| {
        format!(
            "Failed to create destination directory '{}': {}",
            dest.display(),
            error
        )
    })?;
    let mut entries = fs::read_dir(source)
        .map_err(|error| format!("Failed to read directory '{}': {}", source.display(), error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            format!(
                "Failed to read directory entry under '{}': {}",
                source.display(),
                error
            )
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory_recursive(source_path.as_path(), dest_path.as_path())?;
        } else {
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!(
                        "Failed to create destination directory '{}': {}",
                        parent.display(),
                        error
                    )
                })?;
            }
            fs::copy(source_path.as_path(), dest_path.as_path()).map_err(|error| {
                format!(
                    "Failed to copy '{}' into '{}': {}",
                    source_path.display(),
                    dest_path.display(),
                    error
                )
            })?;
        }
    }
    Ok(())
}

fn validate_package_relative_json_path(relative_path: &str) -> Result<PathBuf, String> {
    let path = normalize_portable_relative_path(relative_path, "Package entrypoint path")?;
    let is_json = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if !is_json {
        return Err(format!(
            "Package entrypoint path '{}' must point to a JSON file.",
            relative_path
        ));
    }
    Ok(path)
}

fn validate_package_alias(alias: &str) -> Result<(), String> {
    validate_identifier(alias, "Package alias")
}

fn validate_entrypoint_name(name: &str) -> Result<(), String> {
    validate_identifier(name, "Package entrypoint")
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    if !value
        .chars()
        .next()
        .map(|ch| ch.is_ascii_alphanumeric())
        .unwrap_or(false)
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(format!(
            "{label} '{}' is invalid. Start with a letter or number, then use only letters, numbers, '-' or '_'.",
            value
        ));
    }
    Ok(())
}

fn entrypoint_summary(entrypoints: &[InstalledPackageEntrypointDocument]) -> String {
    if entrypoints.is_empty() {
        return "0 entrypoints".to_string();
    }
    let run_count = entrypoints.iter().filter(|entry| entry.runnable).count();
    let hatch_count = entrypoints.iter().filter(|entry| entry.hatchable).count();
    format!("{run_count} run / {hatch_count} hatch")
}

fn now_rfc3339() -> Result<String, String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| format!("Failed to format install timestamp: {error}"))
}

fn packages_root() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = TEST_PACKAGES_ROOT.with(|value| value.borrow().clone()) {
        return root;
    }

    crate::config::paths::cargo_ai_root().join("packages")
}

fn packages_staging_root() -> PathBuf {
    packages_root().join(".staging")
}

fn packages_lock_root() -> PathBuf {
    packages_root().join(".locks")
}

fn acquire_package_alias_lock(
    alias: &str,
) -> Result<crate::commands::package_lock::PackageAliasLockGuard, String> {
    ensure_real_directory(packages_root().as_path(), "Package store")?;
    crate::commands::package_lock::try_acquire_package_alias_lock(
        packages_lock_root().as_path(),
        alias,
    )
}

fn acquire_package_alias_read_lock(
    alias: &str,
) -> Result<crate::commands::package_lock::PackageAliasLockGuard, String> {
    ensure_real_directory(packages_root().as_path(), "Package store")?;
    crate::commands::package_lock::try_acquire_package_alias_read_lock(
        packages_lock_root().as_path(),
        alias,
    )
}

fn ensure_packages_staging_root() -> Result<PathBuf, String> {
    let packages_root = packages_root();
    ensure_real_directory(&packages_root, "Package store")?;

    let staging_root = packages_staging_root();
    ensure_real_directory(&staging_root, "Package staging root")?;
    Ok(staging_root)
}

fn create_tool_build_scratch_root(
    _staging_root: &Path,
) -> Result<TemporaryPackageRootGuard, String> {
    #[cfg(windows)]
    {
        create_unique_tool_build_scratch_root(std::env::temp_dir().as_path())
    }

    #[cfg(not(windows))]
    {
        let path = ensure_directory_path_under_root(
            _staging_root,
            Path::new(".tool-build"),
            "Staged package tool build scratch directory",
        )?;
        Ok(TemporaryPackageRootGuard::new(path))
    }
}

#[cfg(any(windows, test))]
fn create_unique_tool_build_scratch_root(
    parent: &Path,
) -> Result<TemporaryPackageRootGuard, String> {
    ensure_real_directory(parent, "Temporary package tool build parent")?;
    for _ in 0..8 {
        let candidate = parent.join(format!("cai-t-{}", Uuid::new_v4().simple()));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                let metadata = match fs::symlink_metadata(&candidate) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        let _ = fs::remove_dir(&candidate);
                        return Err(format!(
                            "Failed to inspect temporary package tool build root '{}': {}",
                            candidate.display(),
                            error
                        ));
                    }
                };
                if metadata_is_link_like(&metadata) || !metadata.is_dir() {
                    let _ = fs::remove_dir(&candidate);
                    return Err(format!(
                        "Temporary package tool build root '{}' must be a real directory and not a symbolic link or reparse point.",
                        candidate.display()
                    ));
                }
                return Ok(TemporaryPackageRootGuard::new(candidate));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Failed to create temporary package tool build root '{}': {}",
                    candidate.display(),
                    error
                ));
            }
        }
    }
    Err(format!(
        "Failed to allocate a unique temporary package tool build root under '{}'.",
        parent.display()
    ))
}

pub(crate) fn ensure_real_directory(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_link_like(&metadata) || !metadata.is_dir() => {
            return Err(format!(
                "{label} '{}' must be a real directory and not a symbolic link or reparse point.",
                path.display()
            ));
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Failed to inspect {label} '{}': {}",
                path.display(),
                error
            ));
        }
    }

    fs::create_dir_all(path)
        .map_err(|error| format!("Failed to create {label} '{}': {}", path.display(), error))?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "Failed to inspect created {label} '{}': {}",
            path.display(),
            error
        )
    })?;
    if metadata_is_link_like(&metadata) || !metadata.is_dir() {
        return Err(format!(
            "{label} '{}' must be a real directory and not a symbolic link or reparse point.",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn ensure_directory_path_under_root(
    root: &Path,
    relative_path: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    ensure_real_directory(root, format!("{label} root").as_str())?;
    let mut resolved = root.to_path_buf();
    for component in relative_path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(segment) => resolved.push(segment),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("{label} must stay beneath '{}'.", root.display()));
            }
        }
        match fs::symlink_metadata(&resolved) {
            Ok(metadata) if metadata_is_link_like(&metadata) || !metadata.is_dir() => {
                return Err(format!(
                    "{label} '{}' must be a real directory and not a symbolic link or reparse point.",
                    resolved.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir(&resolved).map_err(|error| {
                    format!(
                        "Failed to create {label} '{}': {}",
                        resolved.display(),
                        error
                    )
                })?;
                let metadata = fs::symlink_metadata(&resolved).map_err(|error| {
                    format!(
                        "Failed to inspect created {label} '{}': {}",
                        resolved.display(),
                        error
                    )
                })?;
                if metadata_is_link_like(&metadata) || !metadata.is_dir() {
                    return Err(format!(
                        "{label} '{}' must be a real directory and not a symbolic link or reparse point.",
                        resolved.display()
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "Failed to inspect {label} '{}': {}",
                    resolved.display(),
                    error
                ));
            }
        }
    }
    Ok(resolved)
}

fn installed_package_root(alias: &str) -> PathBuf {
    packages_root().join(alias)
}

fn installed_package_data_root(alias: &str) -> PathBuf {
    installed_package_root(alias).join(INSTALLED_PACKAGE_DATA_DIR_NAME)
}

#[cfg(test)]
fn installed_package_runtime_tools_root(alias: &str) -> PathBuf {
    installed_package_root(alias)
        .join(INSTALLED_PACKAGE_RUNTIME_DIR_NAME)
        .join(INSTALLED_PACKAGE_RUNTIME_TOOLS_DIR_NAME)
}

#[cfg(test)]
mod tests {
    use super::{
        build_entrypoints, checked_runtime_context_for_path,
        checked_runtime_context_for_project_root, determine_install_action,
        ensure_hosted_permissions_are_accepted, install_local_package, load_installed_package,
        normalize_portable_relative_path, resolve_entrypoint_reference, resolve_package_data_path,
        runtime_context_for_package_root, uninstall_package, InstallAction, InstallRequest,
        InstalledPackageRuntimeContext, PackageManifestDocument, PackagePermissionProfileDocument,
        StagedInstallFailurePoint,
    };
    use base64::Engine as _;
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    fn manifest(
        agent_definitions: Vec<&str>,
        hatched_agents: Vec<&str>,
    ) -> PackageManifestDocument {
        PackageManifestDocument {
            format_version: 1,
            project_name: Some("demo".to_string()),
            project_version: Some("1.0.0".to_string()),
            profile: "default".to_string(),
            agent_definitions: agent_definitions.into_iter().map(str::to_string).collect(),
            hatched_agents: hatched_agents.into_iter().map(str::to_string).collect(),
            tools: Vec::new(),
            assets: Vec::new(),
            permissions: PackagePermissionProfileDocument::default(),
        }
    }

    fn hosted_source(source_id: &str) -> super::InstalledPackageSourceDocument {
        super::InstalledPackageSourceDocument {
            kind: "hosted".to_string(),
            path: None,
            account_selector: Some("self".to_string()),
            requested_owner_handle: None,
            hosted_source_id: Some(source_id.to_string()),
            hosted_version_id: Some("version-id".to_string()),
            owner_handle: None,
        }
    }

    struct PackagesRootGuard {
        path: PathBuf,
    }

    impl PackagesRootGuard {
        fn new(stem: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "cargo-ai-local-package-store-{stem}-{}",
                uuid::Uuid::new_v4()
            ));
            super::TEST_PACKAGES_ROOT.with(|value| {
                value.replace(Some(path.join("packages")));
            });
            Self { path }
        }
    }

    impl Drop for PackagesRootGuard {
        fn drop(&mut self) {
            super::TEST_PACKAGES_ROOT.with(|value| {
                value.replace(None);
            });
            super::TEST_STAGED_INSTALL_FAILURES.with(|failures| {
                failures.replace(Vec::new());
            });
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn temp_package_root(stem: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cargo-ai-local-package-root-{stem}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("agents")).expect("agents dir should be writable");
        std::fs::create_dir_all(root.join(".cargo-ai"))
            .expect("project metadata dir should be writable");
        std::fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be writable");
        std::fs::write(
            root.join("cargo-ai-package.toml"),
            r#"format_version = 1
project_name = "data_integration"
project_version = "1.0.0"
profile = "default"
agent_definitions = ["agents/lookup_account.json"]
hatched_agents = ["agents/daily_digest.json"]
tools = []
assets = ["schemas/customer.sql"]
"#,
        )
        .expect("package manifest should be writable");
        std::fs::write(root.join("agents/lookup_account.json"), "{}")
            .expect("run definition should be writable");
        std::fs::write(root.join("agents/daily_digest.json"), "{}")
            .expect("hatch definition should be writable");
        root
    }

    #[test]
    fn unique_tool_build_scratch_roots_are_short_and_cleaned_on_every_exit() {
        let parent = std::env::temp_dir().join(format!(
            "cargo-ai-tool-build-parent-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&parent).expect("temporary build parent should be writable");

        let first = super::create_unique_tool_build_scratch_root(parent.as_path())
            .expect("first scratch root should be created");
        let second = super::create_unique_tool_build_scratch_root(parent.as_path())
            .expect("second scratch root should be created");
        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();

        assert_ne!(first_path, second_path);
        for path in [&first_path, &second_path] {
            assert_eq!(path.parent(), Some(parent.as_path()));
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("scratch root should have a UTF-8 name");
            assert!(name.starts_with("cai-t-"));
            assert_eq!(name.len(), 38);
            assert!(path.is_dir());
        }

        std::fs::write(first_path.join("artifact"), "built")
            .expect("scratch output should be writable");
        first
            .cleanup("test package tool build root")
            .expect("explicit success cleanup should remove the first root");
        assert!(!first_path.exists());

        std::fs::write(second_path.join("partial-artifact"), "failed")
            .expect("partial scratch output should be writable");
        drop(second);
        assert!(!second_path.exists());

        std::fs::remove_dir(&parent).expect("empty build parent should be removable");
    }

    #[cfg(unix)]
    #[test]
    fn unique_tool_build_scratch_root_rejects_linked_parent() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "cargo-ai-tool-build-linked-parent-{}",
            uuid::Uuid::new_v4()
        ));
        let real_parent = root.join("real");
        let linked_parent = root.join("linked");
        std::fs::create_dir_all(&real_parent).expect("real build parent should be writable");
        symlink(&real_parent, &linked_parent).expect("linked build parent should be created");

        let error = super::create_unique_tool_build_scratch_root(linked_parent.as_path())
            .expect_err("linked build parent must be rejected");

        assert!(error.contains("symbolic link") || error.contains("reparse point"));
        assert_eq!(
            std::fs::read_dir(&real_parent)
                .expect("real parent should remain readable")
                .count(),
            0
        );
        std::fs::remove_dir_all(&root).expect("linked-parent fixture should be removable");
    }

    fn add_source_tool_fixture(package_root: &Path, tool_name: &str, marker: &str) {
        let tool_source_root = package_root.join("tools").join(tool_name);
        std::fs::create_dir_all(tool_source_root.join("src"))
            .expect("tool source directory should be writable");
        std::fs::create_dir_all(package_root.join(".cargo-ai/tools").join(tool_name))
            .expect("tool metadata directory should be writable");
        std::fs::write(
            tool_source_root.join("Cargo.toml"),
            format!("[package]\nname = \"{tool_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .expect("tool Cargo.toml should be writable");
        std::fs::write(
            tool_source_root.join("Cargo.lock"),
            format!(
                "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"{tool_name}\"\nversion = \"0.1.0\"\n"
            ),
        )
        .expect("tool Cargo.lock should be writable");
        write_source_tool_fixture_main(package_root, tool_name, marker);
        std::fs::write(
            package_root
                .join(".cargo-ai/tools")
                .join(tool_name)
                .join("tool.json"),
            serde_json::json!({
                "schema_version": 1,
                "tool_id": tool_name,
                "source": {
                    "manifest_path": format!("tools/{tool_name}/Cargo.toml")
                },
                "binary": {
                    "default_name": tool_name
                },
                "artifacts": {}
            })
            .to_string(),
        )
        .expect("tool metadata should be writable");

        let manifest_path = package_root.join("cargo-ai-package.toml");
        let manifest = std::fs::read_to_string(&manifest_path).expect("manifest should read");
        std::fs::write(
            &manifest_path,
            manifest.replace("tools = []", format!("tools = [\"{tool_name}\"]").as_str()),
        )
        .expect("package manifest tools should update");
    }

    fn write_source_tool_fixture_main(package_root: &Path, tool_name: &str, marker: &str) {
        std::fs::write(
            package_root
                .join("tools")
                .join(tool_name)
                .join("src/main.rs"),
            format!("fn main() {{ println!(\"{marker}\"); }}\n"),
        )
        .expect("tool source should be writable");
    }

    fn write_broken_source_tool_fixture_main(package_root: &Path, tool_name: &str) {
        std::fs::write(
            package_root
                .join("tools")
                .join(tool_name)
                .join("src/main.rs"),
            "compile_error!(\"intentional package tool build failure\");\nfn main() {}\n",
        )
        .expect("broken tool source should be writable");
    }

    fn set_package_version(package_root: &Path, from: &str, to: &str) {
        let manifest_path = package_root.join("cargo-ai-package.toml");
        let manifest = std::fs::read_to_string(&manifest_path).expect("manifest should read");
        std::fs::write(
            &manifest_path,
            manifest.replace(
                format!("project_version = \"{from}\"").as_str(),
                format!("project_version = \"{to}\"").as_str(),
            ),
        )
        .expect("package version should update");
    }

    fn installed_runtime_tool_binary(alias: &str, tool_name: &str) -> PathBuf {
        let target = crate::cargo_ai_metadata::current_build_target();
        let build_target =
            crate::agent_builder::build_target::BuildTarget::from_cli(Some(target.as_str()))
                .expect("current target should resolve");
        build_target.exported_binary_path(
            super::installed_package_runtime_tools_root(alias)
                .join(tool_name)
                .join("bin")
                .join(target)
                .join("release")
                .as_path(),
            tool_name,
        )
    }

    fn package_content_sha256(package_root: &Path) -> String {
        let archive = crate::commands::account::create_package_archive_bytes(package_root)
            .expect("package archive should build");
        crate::commands::account::sha256_hex(archive.as_slice())
    }

    fn legacy_package_archive_bytes(package_root: &Path) -> Vec<u8> {
        fn append_entries(package_root: &Path, current_root: &Path, entries: &mut Vec<Value>) {
            let mut children = std::fs::read_dir(current_root)
                .expect("legacy archive fixture directory should read")
                .collect::<Result<Vec<_>, _>>()
                .expect("legacy archive fixture entries should read");
            children.sort_by_key(std::fs::DirEntry::file_name);
            for child in children {
                let path = child.path();
                let relative = path
                    .strip_prefix(package_root)
                    .expect("legacy archive fixture path should be relative")
                    .to_string_lossy()
                    .replace('\\', "/");
                let metadata = std::fs::symlink_metadata(&path)
                    .expect("legacy archive fixture metadata should read");
                if metadata.is_dir() {
                    entries.push(serde_json::json!({
                        "path": relative,
                        "kind": "dir"
                    }));
                    append_entries(package_root, path.as_path(), entries);
                } else {
                    entries.push(serde_json::json!({
                        "path": relative,
                        "kind": "file",
                        "contents_base64": base64::engine::general_purpose::STANDARD.encode(
                            std::fs::read(&path)
                                .expect("legacy archive fixture file should read")
                        )
                    }));
                }
            }
        }

        let mut entries = Vec::new();
        append_entries(package_root, package_root, &mut entries);
        serde_json::to_vec(&serde_json::json!({
            "format_version": 1,
            "entries": entries
        }))
        .expect("legacy archive fixture should serialize")
    }

    fn add_package_mutating_build_script(package_root: &Path, tool_name: &str) {
        std::fs::write(
            package_root.join("tools").join(tool_name).join("build.rs"),
            r#"use std::path::PathBuf;

fn main() {
    let package_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::write(
        package_root.join("agents/lookup_account.json"),
        "{\"mutated_by_build\":true}",
    )
    .expect("staged package mutation should succeed");
}
"#,
        )
        .expect("package-mutating build script should be writable");
    }

    fn run_fixture_tool(path: &Path) -> String {
        let output = std::process::Command::new(path)
            .output()
            .expect("fixture tool should start");
        assert!(output.status.success(), "fixture tool should succeed");
        String::from_utf8(output.stdout)
            .expect("fixture output should be utf8")
            .trim()
            .to_string()
    }

    fn remove_temp_dir_if_present(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    fn local_install_request(package_root: &Path) -> InstallRequest {
        InstallRequest {
            source: Some(package_root.to_string_lossy().to_string()),
            alias: Some("data_integration".to_string()),
            profile: "default".to_string(),
            replace: false,
            downgrade: false,
            keep_data: false,
            delete_data: false,
        }
    }

    fn prepare_package_upgrade(stem: &str) -> (PackagesRootGuard, PathBuf, InstallRequest) {
        let store = PackagesRootGuard::new(stem);
        let package_root = temp_package_root(stem);
        let request = local_install_request(&package_root);
        let action = install_local_package(&request).expect("first install should succeed");
        assert!(matches!(action, InstallAction::New));

        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), r#"{"kept":true}"#)
            .expect("data file should be writable");

        let manifest_path = package_root.join("cargo-ai-package.toml");
        let manifest = std::fs::read_to_string(&manifest_path).expect("manifest should read");
        std::fs::write(
            &manifest_path,
            manifest.replace(
                r#"project_version = "1.0.0""#,
                r#"project_version = "1.1.0""#,
            ),
        )
        .expect("manifest should update");
        std::fs::write(
            package_root.join("agents/lookup_account.json"),
            r#"{"version":2}"#,
        )
        .expect("updated definition should be writable");

        (store, package_root, request)
    }

    fn assert_failed_upgrade_restores_previous_install(
        point: StagedInstallFailurePoint,
        stem: &str,
    ) {
        let (_store, package_root, request) = prepare_package_upgrade(stem);
        super::set_test_staged_install_failures(&[point]);

        let error = install_local_package(&request).expect_err("upgrade should fail");

        assert!(error.contains("Previous package alias `data_integration` was restored"));
        let installed =
            load_installed_package("data_integration").expect("previous metadata should load");
        assert_eq!(installed.package_version, "1.0.0");
        assert_eq!(
            std::fs::read_to_string(
                super::installed_package_data_root("data_integration").join("state.json")
            )
            .expect("previous data should be restored"),
            r#"{"kept":true}"#
        );
        assert_eq!(
            std::fs::read_to_string(
                super::installed_package_root("data_integration")
                    .join("package/agents/lookup_account.json")
            )
            .expect("previous package payload should be restored"),
            "{}"
        );
        assert_eq!(
            std::fs::read_dir(super::packages_staging_root())
                .expect("staging root should remain readable")
                .count(),
            0,
            "recoverable failure should clean transaction artifacts"
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    fn runtime_context(package_data_root: PathBuf) -> InstalledPackageRuntimeContext {
        let package_payload_root = package_data_root
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join("package");
        InstalledPackageRuntimeContext {
            alias: "data_integration".to_string(),
            source_kind: "hosted".to_string(),
            package_payload_root,
            package_data_root,
            current_entrypoint_path: None,
            entrypoints: Vec::new(),
            permissions: PackagePermissionProfileDocument::default(),
        }
    }

    #[test]
    fn duplicate_entrypoint_merges_run_and_hatch_capability() {
        let root = std::env::temp_dir().join(format!(
            "cargo-ai-local-package-entrypoints-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("agents")).expect("temp package dir should exist");
        std::fs::write(root.join("agents/demo.json"), "{}").expect("definition should be written");

        let entrypoints = build_entrypoints(
            &manifest(vec!["agents/demo.json"], vec!["agents/demo.json"]),
            root.as_path(),
        )
        .expect("entrypoints should build");

        assert_eq!(entrypoints.len(), 1);
        assert_eq!(entrypoints[0].name, "demo");
        assert!(entrypoints[0].runnable);
        assert!(entrypoints[0].hatchable);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn version_decision_requires_downgrade_for_older_version() {
        let existing = super::InstalledPackageDocument {
            format_version: 1,
            alias: "demo".to_string(),
            package_name: "demo".to_string(),
            package_version: "2.0.0".to_string(),
            profile: "default".to_string(),
            content_sha256: "aaa".to_string(),
            source: super::InstalledPackageSourceDocument {
                kind: "test".to_string(),
                path: None,
                account_selector: None,
                requested_owner_handle: None,
                hosted_source_id: None,
                hosted_version_id: None,
                owner_handle: None,
            },
            installed_at: "2026-06-22T00:00:00Z".to_string(),
            permissions: PackagePermissionProfileDocument::default(),
            entrypoints: Vec::new(),
        };

        let error = determine_install_action(Some(&existing), "demo", "1.0.0", "bbb", false, false)
            .expect_err("downgrade should require flag");

        assert!(error.contains("--downgrade"));
    }

    #[test]
    fn local_root_install_resolves_entrypoints_and_uninstalls_alias() {
        let _store = PackagesRootGuard::new("install-resolve");
        let package_root = temp_package_root("install-resolve");

        let request = InstallRequest {
            source: Some(package_root.to_string_lossy().to_string()),
            alias: Some("data_integration".to_string()),
            profile: "default".to_string(),
            replace: false,
            downgrade: false,
            keep_data: false,
            delete_data: false,
        };
        let action = install_local_package(&request).expect("install should succeed");
        assert!(matches!(action, InstallAction::New));

        let installed = load_installed_package("data_integration").expect("metadata should load");
        assert_eq!(installed.alias, "data_integration");
        assert_eq!(installed.package_name, "data_integration");
        assert_eq!(installed.package_version, "1.0.0");
        assert_eq!(installed.entrypoints.len(), 2);

        let run_entrypoint =
            resolve_entrypoint_reference("data_integration::lookup_account", false)
                .expect("run entrypoint should resolve")
                .expect("run entrypoint should be installed");
        assert!(run_entrypoint
            .definition_path
            .ends_with("package/agents/lookup_account.json"));

        let hatch_entrypoint = resolve_entrypoint_reference("data_integration::daily_digest", true)
            .expect("hatch entrypoint should resolve")
            .expect("hatch entrypoint should be installed");
        assert!(hatch_entrypoint
            .definition_path
            .ends_with("package/agents/daily_digest.json"));

        let error = uninstall_package("data_integration", false)
            .expect_err("active run/hatch resolutions should lease the alias");
        assert!(error.contains("another Cargo AI process"));
        drop((run_entrypoint, hatch_entrypoint));
        uninstall_package("data_integration", false).expect("uninstall should succeed");
        assert!(load_installed_package("data_integration").is_err());

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn local_install_materializes_declared_source_tool_without_mutating_source() {
        let _store = PackagesRootGuard::new("materialize-source-tool");
        let package_root = temp_package_root("materialize-source-tool");
        add_source_tool_fixture(&package_root, "usage_importer", "materialized-v1");
        let source_sha256_before = package_content_sha256(&package_root);

        install_local_package(&local_install_request(&package_root))
            .expect("source-backed package should install");

        assert_eq!(package_content_sha256(&package_root), source_sha256_before);
        assert!(!package_root.join("tools/usage_importer/target").exists());
        let installed_package_root =
            super::installed_package_root("data_integration").join("package");
        assert_eq!(
            package_content_sha256(installed_package_root.as_path()),
            source_sha256_before
        );
        assert!(!installed_package_root
            .join("tools/usage_importer/target")
            .exists());
        let installed = load_installed_package("data_integration").expect("receipt should load");
        assert_eq!(installed.content_sha256, source_sha256_before);

        let runtime_tool_binary =
            installed_runtime_tool_binary("data_integration", "usage_importer");
        assert_eq!(run_fixture_tool(&runtime_tool_binary), "materialized-v1");
        assert!(
            super::installed_package_runtime_tools_root("data_integration")
                .join("usage_importer/tool.json")
                .is_file()
        );
        assert!(!super::installed_package_root("data_integration")
            .join(".tool-build")
            .exists());

        let context = super::runtime_context_for_package_root(installed_package_root.as_path())
            .expect("installed package runtime context should resolve");
        assert_eq!(
            super::resolve_package_runtime_tools_root(&context)
                .expect("runtime tools root lookup should succeed")
                .expect("runtime tools root should resolve"),
            super::installed_package_runtime_tools_root("data_integration")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                std::fs::metadata(&runtime_tool_binary)
                    .expect("runtime tool metadata should load")
                    .permissions()
                    .mode()
                    & 0o111,
                0,
                "runtime tool should remain executable"
            );
        }

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn source_tool_build_cannot_mutate_verified_package_payload() {
        let _store = PackagesRootGuard::new("reject-tool-package-mutation");
        let package_root = temp_package_root("reject-tool-package-mutation");
        add_source_tool_fixture(&package_root, "usage_importer", "materialized-v1");
        add_package_mutating_build_script(&package_root, "usage_importer");

        let error = install_local_package(&local_install_request(&package_root))
            .expect_err("a tool build that mutates the staged package must fail");

        assert!(error.contains("payload changed while materializing runtime tools"));
        assert!(!super::installed_package_root("data_integration").exists());
        assert_eq!(
            std::fs::read_to_string(package_root.join("agents/lookup_account.json"))
                .expect("source payload should remain readable"),
            "{}"
        );
        assert_eq!(
            std::fs::read_dir(super::packages_staging_root())
                .expect("staging root should remain readable")
                .count(),
            0
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn same_content_install_is_noop_but_repairs_missing_legacy_runtime() {
        let _store = PackagesRootGuard::new("repair-legacy-runtime");
        let package_root = temp_package_root("repair-legacy-runtime");
        let request = local_install_request(&package_root);
        install_local_package(&request).expect("initial package should install");
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), "preserve")
            .expect("package data should be writable");

        let noop = install_local_package(&request).expect("valid reinstall should succeed");
        assert!(matches!(noop, InstallAction::Noop));

        std::fs::remove_dir_all(
            super::installed_package_root("data_integration")
                .join(super::INSTALLED_PACKAGE_RUNTIME_DIR_NAME),
        )
        .expect("legacy runtime should be removable in the fixture");
        let repaired = install_local_package(&request).expect("legacy runtime should repair");

        assert!(matches!(repaired, InstallAction::Replace));
        assert!(super::installed_package_runtime_tools_root("data_integration").is_dir());
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn same_content_install_repairs_wrong_target_runtime_and_preserves_data() {
        let _store = PackagesRootGuard::new("repair-wrong-target-runtime");
        let package_root = temp_package_root("repair-wrong-target-runtime");
        add_source_tool_fixture(&package_root, "usage_importer", "materialized-v1");
        let request = local_install_request(&package_root);
        install_local_package(&request).expect("initial package should install");
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), "preserve")
            .expect("package data should be writable");
        let manifest_path = super::installed_package_runtime_tools_root("data_integration")
            .join("usage_importer/tool.json");
        let mut manifest: Value = serde_json::from_slice(
            std::fs::read(&manifest_path)
                .expect("runtime manifest should read")
                .as_slice(),
        )
        .expect("runtime manifest should parse");
        let target = crate::cargo_ai_metadata::current_build_target();
        let artifacts = manifest
            .get_mut("artifacts")
            .and_then(Value::as_object_mut)
            .expect("runtime manifest should contain artifacts");
        let artifact = artifacts
            .remove(target.as_str())
            .expect("current target artifact should exist");
        artifacts.insert("unsupported-test-target".to_string(), artifact);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("runtime manifest should serialize"),
        )
        .expect("wrong-target runtime manifest should be writable");

        let action = install_local_package(&request).expect("wrong-target runtime should repair");

        assert!(matches!(action, InstallAction::Replace));
        assert_eq!(
            run_fixture_tool(&installed_runtime_tool_binary(
                "data_integration",
                "usage_importer"
            )),
            "materialized-v1"
        );
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn same_content_install_repairs_missing_and_corrupt_runtime_artifacts() {
        let _store = PackagesRootGuard::new("repair-invalid-runtime-artifact");
        let package_root = temp_package_root("repair-invalid-runtime-artifact");
        add_source_tool_fixture(&package_root, "usage_importer", "materialized-v1");
        let request = local_install_request(&package_root);
        install_local_package(&request).expect("initial package should install");
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), "preserve")
            .expect("package data should be writable");
        let runtime_binary = installed_runtime_tool_binary("data_integration", "usage_importer");
        let manifest_path = super::installed_package_runtime_tools_root("data_integration")
            .join("usage_importer/tool.json");

        std::fs::remove_file(&runtime_binary).expect("runtime artifact should be removable");
        let missing_action =
            install_local_package(&request).expect("missing runtime artifact should repair");
        assert!(matches!(missing_action, InstallAction::Replace));
        assert_eq!(run_fixture_tool(&runtime_binary), "materialized-v1");

        std::fs::remove_file(&runtime_binary).expect("runtime artifact should be removable");
        std::fs::create_dir(&runtime_binary)
            .expect("corrupt runtime artifact directory should be creatable");
        let corrupt_action =
            install_local_package(&request).expect("non-file runtime artifact should repair");
        assert!(matches!(corrupt_action, InstallAction::Replace));
        assert_eq!(run_fixture_tool(&runtime_binary), "materialized-v1");

        std::fs::write(&manifest_path, "{not valid json")
            .expect("malformed runtime manifest should be writable");
        let malformed_action =
            install_local_package(&request).expect("malformed runtime manifest should repair");
        assert!(matches!(malformed_action, InstallAction::Replace));
        assert_eq!(run_fixture_tool(&runtime_binary), "materialized-v1");
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn same_content_install_rejects_runtime_artifact_path_escape() {
        let _store = PackagesRootGuard::new("reject-runtime-artifact-escape");
        let package_root = temp_package_root("reject-runtime-artifact-escape");
        add_source_tool_fixture(&package_root, "usage_importer", "materialized-v1");
        let request = local_install_request(&package_root);
        install_local_package(&request).expect("initial package should install");
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), "preserve")
            .expect("package data should be writable");
        let manifest_path = super::installed_package_runtime_tools_root("data_integration")
            .join("usage_importer/tool.json");
        let mut manifest: Value = serde_json::from_slice(
            std::fs::read(&manifest_path)
                .expect("runtime manifest should read")
                .as_slice(),
        )
        .expect("runtime manifest should parse");
        let target = crate::cargo_ai_metadata::current_build_target();
        manifest["artifacts"][target.as_str()]["path"] = serde_json::json!("../../outside");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("runtime manifest should serialize"),
        )
        .expect("escaping runtime manifest should be writable");

        let error = install_local_package(&request)
            .expect_err("runtime artifact path traversal must fail closed");

        assert!(error.contains("parent traversal"));
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[cfg(unix)]
    #[test]
    fn same_content_install_rejects_runtime_artifact_symlink() {
        use std::os::unix::fs::symlink;

        let store = PackagesRootGuard::new("reject-runtime-artifact-symlink");
        let package_root = temp_package_root("reject-runtime-artifact-symlink");
        add_source_tool_fixture(&package_root, "usage_importer", "materialized-v1");
        let request = local_install_request(&package_root);
        install_local_package(&request).expect("initial package should install");
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), "preserve")
            .expect("package data should be writable");
        let runtime_binary = installed_runtime_tool_binary("data_integration", "usage_importer");
        let outside_binary = store.path.join("outside-binary");
        std::fs::write(&outside_binary, "outside").expect("outside fixture should be writable");
        std::fs::remove_file(&runtime_binary).expect("runtime artifact should be removable");
        symlink(&outside_binary, &runtime_binary).expect("runtime symlink should be creatable");

        let error =
            install_local_package(&request).expect_err("runtime artifact symlink must fail closed");

        assert!(error.contains("symbolic link") || error.contains("reparse point"));
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn legacy_runtime_tools_root_absence_is_nonfatal() {
        let install_root = std::env::temp_dir().join(format!(
            "cargo-ai-legacy-runtime-root-{}",
            uuid::Uuid::new_v4()
        ));
        let data_root = install_root.join("data");
        std::fs::create_dir_all(install_root.join("package")).expect("package root should exist");
        std::fs::create_dir_all(&data_root).expect("data root should exist");
        let context = runtime_context(data_root);

        assert!(super::resolve_package_runtime_tools_root(&context)
            .expect("legacy runtime lookup should succeed")
            .is_none());

        remove_temp_dir_if_present(install_root.as_path());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_tools_root_rejects_symbolic_link_redirect() {
        use std::os::unix::fs::symlink;

        let install_root = std::env::temp_dir().join(format!(
            "cargo-ai-runtime-root-link-{}",
            uuid::Uuid::new_v4()
        ));
        let outside_root = std::env::temp_dir().join(format!(
            "cargo-ai-runtime-root-link-outside-{}",
            uuid::Uuid::new_v4()
        ));
        let data_root = install_root.join("data");
        std::fs::create_dir_all(install_root.join("package")).expect("package root should exist");
        std::fs::create_dir_all(&data_root).expect("data root should exist");
        std::fs::create_dir_all(outside_root.join("tools")).expect("outside runtime should exist");
        symlink(&outside_root, install_root.join("runtime"))
            .expect("runtime symlink should be created");
        let context = runtime_context(data_root);

        let error = super::resolve_package_runtime_tools_root(&context)
            .expect_err("runtime redirect should fail");

        assert!(error.contains("symbolic link") || error.contains("reparse point"));
        remove_temp_dir_if_present(install_root.as_path());
        remove_temp_dir_if_present(outside_root.as_path());
    }

    #[test]
    fn failed_source_tool_materialization_preserves_existing_alias_and_data() {
        let _store = PackagesRootGuard::new("failed-tool-materialization");
        let package_root = temp_package_root("failed-tool-materialization");
        add_source_tool_fixture(&package_root, "usage_importer", "materialized-v1");
        let request = local_install_request(&package_root);
        install_local_package(&request).expect("initial package should install");
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), "preserve")
            .expect("package data should be writable");
        let runtime_tool_binary =
            installed_runtime_tool_binary("data_integration", "usage_importer");
        let runtime_binary_before =
            std::fs::read(&runtime_tool_binary).expect("runtime tool should read");

        set_package_version(&package_root, "1.0.0", "1.1.0");
        write_broken_source_tool_fixture_main(&package_root, "usage_importer");
        let error = install_local_package(&request)
            .expect_err("broken source tool should fail before alias replacement");

        assert!(error.contains("Locked Cargo build failed"));
        let installed = load_installed_package("data_integration").expect("receipt should load");
        assert_eq!(installed.package_version, "1.0.0");
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );
        assert_eq!(
            std::fs::read(&runtime_tool_binary).expect("prior runtime tool should remain"),
            runtime_binary_before
        );
        assert_eq!(run_fixture_tool(&runtime_tool_binary), "materialized-v1");
        assert_eq!(
            std::fs::read_dir(super::packages_staging_root())
                .expect("staging should remain readable")
                .count(),
            0
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn package_upgrade_replaces_runtime_tools_and_preserves_data() {
        let _store = PackagesRootGuard::new("replace-runtime-tools");
        let package_root = temp_package_root("replace-runtime-tools");
        add_source_tool_fixture(&package_root, "usage_importer", "materialized-v1");
        let request = local_install_request(&package_root);
        install_local_package(&request).expect("initial package should install");
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), "preserve")
            .expect("package data should be writable");
        let runtime_tool_binary =
            installed_runtime_tool_binary("data_integration", "usage_importer");
        let runtime_binary_before =
            std::fs::read(&runtime_tool_binary).expect("runtime tool should read");

        set_package_version(&package_root, "1.0.0", "1.1.0");
        write_source_tool_fixture_main(&package_root, "usage_importer", "materialized-v2");
        let expected_package_sha256 = package_content_sha256(&package_root);
        let action = install_local_package(&request).expect("package upgrade should succeed");

        assert!(matches!(action, InstallAction::Upgrade));
        let installed = load_installed_package("data_integration").expect("receipt should load");
        assert_eq!(installed.package_version, "1.1.0");
        assert_eq!(installed.content_sha256, expected_package_sha256);
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );
        assert_ne!(
            std::fs::read(&runtime_tool_binary).expect("new runtime tool should read"),
            runtime_binary_before
        );
        assert_eq!(run_fixture_tool(&runtime_tool_binary), "materialized-v2");
        assert_eq!(
            package_content_sha256(
                super::installed_package_root("data_integration")
                    .join("package")
                    .as_path()
            ),
            expected_package_sha256
        );
        assert!(!super::installed_package_root("data_integration")
            .join(".tool-build")
            .exists());

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn hosted_blocked_package_does_not_build_declared_source_tools() {
        let _store = PackagesRootGuard::new("hosted-blocked-tool");
        let package_root = temp_package_root("hosted-blocked-tool");
        add_source_tool_fixture(&package_root, "usage_importer", "must-not-build");
        write_broken_source_tool_fixture_main(&package_root, "usage_importer");
        let prepared =
            super::prepare_package_root(package_root.clone(), hosted_source("source-a"), None)
                .expect("hosted package should prepare");

        super::materialize_prepared_package(
            &prepared,
            Some("data_integration"),
            false,
            false,
            false,
            false,
            false,
        )
        .expect("blocked hosted package should install without executing tool source");

        assert_eq!(
            std::fs::read_dir(super::installed_package_runtime_tools_root(
                "data_integration"
            ))
            .expect("runtime tools root should exist")
            .count(),
            0
        );
        assert!(!super::installed_package_root("data_integration")
            .join(".tool-build")
            .exists());

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn hosted_allowed_tool_build_requires_acceptance_then_materializes() {
        let _store = PackagesRootGuard::new("hosted-allowed-tool");
        let package_root = temp_package_root("hosted-allowed-tool");
        add_source_tool_fixture(&package_root, "usage_importer", "materialized-hosted");
        write_broken_source_tool_fixture_main(&package_root, "usage_importer");
        let mut unaccepted =
            super::prepare_package_root(package_root.clone(), hosted_source("source-a"), None)
                .expect("hosted package should prepare");
        unaccepted.manifest.permissions.subprocess = "allowed".to_string();

        let error = super::materialize_prepared_package(
            &unaccepted,
            Some("data_integration"),
            false,
            false,
            false,
            false,
            false,
        )
        .expect_err("subprocess permission should be accepted before any tool build");

        assert!(error.contains("--accept-permissions"));
        assert!(!super::installed_package_root("data_integration").exists());
        assert!(!package_root.join("tools/usage_importer/target").exists());

        write_source_tool_fixture_main(&package_root, "usage_importer", "materialized-hosted");
        let mut accepted =
            super::prepare_package_root(package_root.clone(), hosted_source("source-a"), None)
                .expect("hosted package should prepare after source repair");
        accepted.manifest.permissions.subprocess = "allowed".to_string();

        super::materialize_prepared_package(
            &accepted,
            Some("data_integration"),
            false,
            false,
            true,
            false,
            false,
        )
        .expect("accepted subprocess permission should allow tool materialization");

        let runtime_tool_binary =
            installed_runtime_tool_binary("data_integration", "usage_importer");
        assert_eq!(
            run_fixture_tool(&runtime_tool_binary),
            "materialized-hosted"
        );
        assert!(!package_root.join("tools/usage_importer/target").exists());
        assert!(!super::installed_package_root("data_integration")
            .join(".tool-build")
            .exists());

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn hosted_archive_must_match_all_inspected_declarations_before_tool_build() {
        let store = PackagesRootGuard::new("hosted-manifest-binding");
        let root = temp_package_root("hosted-manifest-binding");
        add_source_tool_fixture(&root, "usage_importer", "must-not-build");
        write_broken_source_tool_fixture_main(&root, "usage_importer");
        let archive = crate::commands::account::create_package_archive_bytes(&root).unwrap();
        let manifest: Value =
            toml::from_str(&std::fs::read_to_string(root.join("cargo-ai-package.toml")).unwrap())
                .unwrap();
        let mut response = serde_json::json!({
            "project": "data_integration", "project_version": "1.0.0",
            "hosted_source_id":"source-id", "hosted_version_id":"version-id",
            "package_sha256":crate::commands::account::sha256_hex(&archive),
            "package_size_bytes":archive.len(), "package_manifest":manifest,
            "package_archive_base64":base64::engine::general_purpose::STANDARD.encode(&archive)
        });
        response["package_manifest"]["permissions"]["subprocess"] = "allowed".into();
        let error = super::prepare_hosted_response(&response, None, None).unwrap_err();
        assert!(error.contains("archive manifest differs"), "{error}");
        assert!(!super::installed_package_root("data_integration").exists());
        assert!(!root.join("tools/usage_importer/target").exists());
        assert_eq!(
            std::fs::read_dir(super::packages_staging_root())
                .unwrap()
                .count(),
            0
        );
        assert!(store.path.exists());
        remove_temp_dir_if_present(&root);
    }

    #[test]
    fn hosted_legacy_wire_hash_survives_install_and_runtime_repair() {
        let _store = PackagesRootGuard::new("hosted-legacy-wire-hash");
        let package_root = temp_package_root("hosted-legacy-wire-hash");
        let archive = legacy_package_archive_bytes(&package_root);
        let wire_sha256 = crate::commands::account::sha256_hex(archive.as_slice());
        let response = serde_json::json!({
            "project": "data_integration",
            "project_version": "1.0.0",
            "hosted_source_id": "source-id",
            "hosted_version_id": "version-id",
            "package_sha256": wire_sha256,
            "package_manifest": toml::from_str::<serde_json::Value>(&std::fs::read_to_string(package_root.join("cargo-ai-package.toml")).unwrap()).unwrap(),
            "package_size_bytes": archive.len(),
            "package_archive_base64": base64::engine::general_purpose::STANDARD.encode(&archive)
        });
        let prepared = super::prepare_hosted_response(&response, None, None)
            .expect("supported legacy hosted archive should prepare");
        let canonical_sha256 = package_content_sha256(prepared.package_root.as_path());
        assert_ne!(
            canonical_sha256, wire_sha256,
            "legacy wire bytes should differ from the canonical archive"
        );

        let initial = super::materialize_prepared_package(
            &prepared,
            Some("data_integration"),
            false,
            false,
            false,
            false,
            false,
        )
        .expect("legacy hosted package should install");
        assert!(matches!(initial.action, InstallAction::New));
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), "preserve")
            .expect("package data should be writable");
        let installed = load_installed_package("data_integration").expect("receipt should load");
        assert_eq!(installed.content_sha256, wire_sha256);

        std::fs::remove_dir_all(
            super::installed_package_root("data_integration")
                .join(super::INSTALLED_PACKAGE_RUNTIME_DIR_NAME),
        )
        .expect("derived runtime should be removable");
        let repaired = super::materialize_prepared_package(
            &prepared,
            Some("data_integration"),
            false,
            false,
            false,
            false,
            false,
        )
        .expect("legacy hosted package runtime should repair");

        assert!(matches!(repaired.action, InstallAction::Replace));
        assert!(super::installed_package_runtime_tools_root("data_integration").is_dir());
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );
        let installed = load_installed_package("data_integration").expect("receipt should load");
        assert_eq!(installed.content_sha256, wire_sha256);

        super::cleanup_prepared_package(&prepared);
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn hosted_permission_lifecycle_replaces_runtime_transactionally() {
        let _store = PackagesRootGuard::new("hosted-permission-lifecycle");
        let allowed_root = temp_package_root("hosted-permission-lifecycle-allowed");
        set_package_version(&allowed_root, "1.0.0", "2.0.0");
        add_source_tool_fixture(&allowed_root, "usage_importer", "allowed-v2");
        let mut allowed =
            super::prepare_package_root(allowed_root.clone(), hosted_source("source-a"), None)
                .expect("allowed hosted package should prepare");
        allowed.manifest.permissions.subprocess = "allowed".to_string();

        let blocked_root = temp_package_root("hosted-permission-lifecycle-blocked");
        add_source_tool_fixture(&blocked_root, "usage_importer", "must-not-run");
        write_broken_source_tool_fixture_main(&blocked_root, "usage_importer");
        let blocked =
            super::prepare_package_root(blocked_root.clone(), hosted_source("source-a"), None)
                .expect("blocked hosted package should prepare");

        let initial = super::materialize_prepared_package(
            &allowed,
            Some("data_integration"),
            false,
            false,
            true,
            false,
            false,
        )
        .expect("accepted allowed package should install");
        assert!(matches!(initial.action, InstallAction::New));
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), "preserve")
            .expect("package data should be writable");
        assert_eq!(
            run_fixture_tool(&installed_runtime_tool_binary(
                "data_integration",
                "usage_importer"
            )),
            "allowed-v2"
        );

        let rollback = super::materialize_prepared_package(
            &blocked,
            Some("data_integration"),
            false,
            true,
            false,
            false,
            false,
        )
        .expect("permission contraction should roll back without building blocked tools");
        assert!(matches!(rollback.action, InstallAction::Downgrade));
        assert_eq!(
            std::fs::read_dir(super::installed_package_runtime_tools_root(
                "data_integration"
            ))
            .expect("blocked runtime tools root should exist")
            .count(),
            0
        );
        let installed = load_installed_package("data_integration").expect("receipt should load");
        assert_eq!(installed.package_version, "1.0.0");
        assert_eq!(
            installed.permissions.subprocess,
            "blocked_without_explicit_grant"
        );
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );

        let unaccepted = super::materialize_prepared_package(
            &allowed,
            Some("data_integration"),
            false,
            false,
            false,
            false,
            false,
        )
        .expect_err("permission expansion should require fresh acceptance");
        assert!(unaccepted.contains("--accept-permissions"));
        let installed = load_installed_package("data_integration").expect("receipt should load");
        assert_eq!(installed.package_version, "1.0.0");
        assert_eq!(
            std::fs::read_dir(super::installed_package_runtime_tools_root(
                "data_integration"
            ))
            .expect("blocked runtime tools root should remain")
            .count(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );

        let update = super::materialize_prepared_package(
            &allowed,
            Some("data_integration"),
            false,
            false,
            true,
            false,
            false,
        )
        .expect("accepted permission expansion should rebuild runtime tools");
        assert!(matches!(update.action, InstallAction::Upgrade));
        assert_eq!(
            run_fixture_tool(&installed_runtime_tool_binary(
                "data_integration",
                "usage_importer"
            )),
            "allowed-v2"
        );
        let installed = load_installed_package("data_integration").expect("receipt should load");
        assert_eq!(installed.package_version, "2.0.0");
        assert_eq!(installed.permissions.subprocess, "allowed");
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("data should remain"),
            "preserve"
        );

        remove_temp_dir_if_present(allowed_root.as_path());
        remove_temp_dir_if_present(blocked_root.as_path());
    }

    #[test]
    fn local_alias_run_and_hatch_remain_available_without_hosted_declaration() {
        let _store = PackagesRootGuard::new("local-alias-project-compatibility");
        let package_root = temp_package_root("local-alias-project-compatibility");
        install_local_package(&local_install_request(&package_root))
            .expect("local package should install");
        let declaring_project = std::env::temp_dir().join(format!(
            "cargo-ai-local-alias-declaring-project-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(declaring_project.join(".cargo-ai"))
            .expect("declaring project metadata dir should exist");
        std::fs::write(
            declaring_project.join(".cargo-ai/project.toml"),
            "format_version = 1\n",
        )
        .expect("declaring project metadata should exist");

        assert!(super::resolve_entrypoint_reference_for_project(
            "data_integration::lookup_account",
            false,
            Some(declaring_project.as_path()),
        )
        .expect("run should allow an undeclared local alias")
        .is_some());
        assert!(super::resolve_entrypoint_reference_for_project(
            "data_integration::daily_digest",
            true,
            Some(declaring_project.as_path()),
        )
        .expect("hatch should allow an undeclared local alias")
        .is_some());

        std::fs::write(
            declaring_project.join(".cargo-ai/project.toml"),
            r#"format_version = 1

[package_dependencies.data_integration]
hosted_source_id = "hosted-source"
version = "^1"
"#,
        )
        .expect("hosted dependency declaration should be writable");
        let error = super::resolve_entrypoint_reference_for_project(
            "data_integration::lookup_account",
            false,
            Some(declaring_project.as_path()),
        )
        .expect_err("a hosted declaration must reject a local alias");
        assert!(error.contains("installed alias came from `local_root`"));

        remove_temp_dir_if_present(declaring_project.as_path());
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn hosted_alias_resolution_rejects_declared_source_mismatch() {
        let _store = PackagesRootGuard::new("hosted-alias-source-mismatch");
        let package_root = temp_package_root("hosted-alias-source-mismatch");
        let prepared =
            super::prepare_package_root(package_root.clone(), hosted_source("actual-source"), None)
                .expect("hosted package should prepare");
        super::materialize_prepared_package(
            &prepared,
            Some("data_integration"),
            false,
            false,
            false,
            false,
            false,
        )
        .expect("hosted package should install");
        let declaring_project = std::env::temp_dir().join(format!(
            "cargo-ai-hosted-alias-declaring-project-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(declaring_project.join(".cargo-ai"))
            .expect("declaring project metadata dir should exist");
        std::fs::write(
            declaring_project.join(".cargo-ai/project.toml"),
            r#"format_version = 1

[package_dependencies.data_integration]
hosted_source_id = "different-source"
version = "^1"
"#,
        )
        .expect("hosted dependency declaration should be writable");

        let error = super::resolve_entrypoint_reference_for_project(
            "data_integration::lookup_account",
            false,
            Some(declaring_project.as_path()),
        )
        .expect_err("a mismatched hosted source must fail closed");
        assert!(error.contains("expects hosted source id"));

        remove_temp_dir_if_present(declaring_project.as_path());
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn direct_hosted_entrypoint_uses_caller_project_binding_when_present() {
        let _store = PackagesRootGuard::new("direct-hosted-caller-binding");
        let package_root = temp_package_root("direct-hosted-caller-binding");
        let prepared =
            super::prepare_package_root(package_root.clone(), hosted_source("actual-source"), None)
                .expect("hosted package should prepare");
        super::materialize_prepared_package(
            &prepared,
            Some("data_integration"),
            false,
            false,
            false,
            false,
            false,
        )
        .expect("hosted package should install");
        let direct_definition = super::installed_package_root("data_integration")
            .join("package/agents/lookup_account.json");
        assert!(super::checked_runtime_lease_for_path(
            &direct_definition,
            Some(super::InstalledEntrypointCapability::Run),
        )
        .expect("outside-project direct hosted entrypoint should follow top-level policy")
        .is_some());

        let caller_project = std::env::temp_dir().join(format!(
            "cargo-ai-direct-hosted-caller-project-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(caller_project.join(".cargo-ai"))
            .expect("caller metadata dir should exist");
        let metadata_path = caller_project.join(".cargo-ai/project.toml");
        std::fs::write(&metadata_path, "format_version = 1\n")
            .expect("caller metadata should exist");
        let undeclared = super::validate_installed_alias_dependency_for_project(
            "data_integration",
            &caller_project,
        )
        .expect_err("hosted direct path must be declared inside a caller project");
        assert!(undeclared.contains("package_dependencies.data_integration"));

        std::fs::write(
            &metadata_path,
            r#"[package_dependencies.data_integration]
hosted_source_id = "different-source"
version = "^1"
"#,
        )
        .expect("source mismatch declaration should be writable");
        let source_error = super::validate_installed_alias_dependency_for_project(
            "data_integration",
            &caller_project,
        )
        .expect_err("wrong source must fail");
        assert!(source_error.contains("expects hosted source id"));

        std::fs::write(
            &metadata_path,
            r#"[package_dependencies.data_integration]
hosted_source_id = "actual-source"
version = "^2"
"#,
        )
        .expect("version mismatch declaration should be writable");
        let version_error = super::validate_installed_alias_dependency_for_project(
            "data_integration",
            &caller_project,
        )
        .expect_err("wrong version must fail");
        assert!(version_error.contains("does not match"));

        remove_temp_dir_if_present(caller_project.as_path());
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn uninstall_requires_confirmation_for_nonempty_persistent_data() {
        let _store = PackagesRootGuard::new("uninstall-data");
        let package_root = temp_package_root("uninstall-data");
        install_local_package(&local_install_request(&package_root))
            .expect("install should succeed");
        let data_root = super::installed_package_data_root("data_integration");
        std::fs::write(data_root.join("state.json"), r#"{"kept":true}"#)
            .expect("persistent data should be writable");

        let error = uninstall_package("data_integration", false)
            .expect_err("nonempty data should require explicit deletion");
        assert!(error.contains("--delete-data"));
        assert!(data_root.join("state.json").exists());

        uninstall_package("data_integration", true)
            .expect("explicit deletion should uninstall the package");
        assert!(!super::installed_package_root("data_integration").exists());
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn replacement_preserves_alias_and_data_when_existing_receipt_is_malformed() {
        let _store = PackagesRootGuard::new("malformed-existing-receipt");
        let package_root = temp_package_root("malformed-existing-receipt");
        install_local_package(&local_install_request(&package_root))
            .expect("install should succeed");
        let install_root = super::installed_package_root("data_integration");
        let receipt_path = install_root.join(super::INSTALL_MANIFEST_FILE_NAME);
        let data_path = install_root
            .join(super::INSTALLED_PACKAGE_DATA_DIR_NAME)
            .join("state.json");
        std::fs::write(&data_path, "keep").expect("persistent data should be writable");
        std::fs::write(&receipt_path, "not = [valid").expect("receipt should be corruptible");

        let mut replacement = local_install_request(&package_root);
        replacement.replace = true;
        replacement.delete_data = true;
        let error = install_local_package(&replacement)
            .expect_err("malformed existing receipt must fail before replacement");
        assert!(error.contains("Failed to parse installed package metadata"));
        assert_eq!(
            std::fs::read_to_string(&receipt_path).expect("receipt should remain untouched"),
            "not = [valid"
        );
        assert_eq!(
            std::fs::read_to_string(&data_path).expect("data should remain untouched"),
            "keep"
        );
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn cross_source_replacement_requires_explicit_data_disposition() {
        let existing = super::InstalledPackageDocument {
            format_version: 1,
            alias: "reports".to_string(),
            package_name: "reports".to_string(),
            package_version: "1.0.0".to_string(),
            profile: "default".to_string(),
            content_sha256: "abc".to_string(),
            source: hosted_source("source-a"),
            installed_at: "2026-07-25T00:00:00Z".to_string(),
            permissions: PackagePermissionProfileDocument::default(),
            entrypoints: Vec::new(),
        };
        let replacement = hosted_source("source-b");

        let error = super::preserve_existing_data_for_install(
            Some(&existing),
            &replacement,
            "reports",
            false,
            false,
        )
        .expect_err("cross-source replacement should require a disposition");
        assert!(error.contains("--keep-data"));
        assert!(error.contains("--delete-data"));
        assert!(super::preserve_existing_data_for_install(
            Some(&existing),
            &replacement,
            "reports",
            true,
            false,
        )
        .expect("explicit transfer should be accepted"));
        assert!(!super::preserve_existing_data_for_install(
            Some(&existing),
            &replacement,
            "reports",
            false,
            true,
        )
        .expect("explicit deletion should be accepted"));
    }

    #[test]
    fn same_content_cross_source_replacement_writes_new_receipt_and_honors_data_choice() {
        let _store = PackagesRootGuard::new("cross-source-materialization");
        let package_root = temp_package_root("cross-source-materialization");
        let prepared_a =
            super::prepare_package_root(package_root.clone(), hosted_source("source-a"), None)
                .expect("first hosted package should prepare");
        super::materialize_prepared_package(
            &prepared_a,
            Some("reports"),
            false,
            false,
            false,
            false,
            false,
        )
        .expect("first hosted package should install");
        let data_root = super::installed_package_data_root("reports");
        std::fs::write(data_root.join("state.json"), "keep")
            .expect("package data should be writable");

        let prepared_b =
            super::prepare_package_root(package_root.clone(), hosted_source("source-b"), None)
                .expect("replacement hosted package should prepare");
        let kept = super::materialize_prepared_package(
            &prepared_b,
            Some("reports"),
            true,
            false,
            false,
            true,
            false,
        )
        .expect("explicit cross-source data transfer should replace same content");
        assert!(matches!(kept.action, InstallAction::Replace));
        let installed = load_installed_package("reports").expect("new receipt should load");
        assert_eq!(
            installed.source.hosted_source_id.as_deref(),
            Some("source-b")
        );
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json")).expect("kept data should remain"),
            "keep"
        );

        let deleted = super::materialize_prepared_package(
            &prepared_b,
            Some("reports"),
            true,
            false,
            false,
            false,
            true,
        )
        .expect("explicit deletion should not be treated as a no-op");
        assert!(matches!(deleted.action, InstallAction::Replace));
        assert!(!data_root.join("state.json").exists());

        let manifest_path = package_root.join("cargo-ai-package.toml");
        let manifest = std::fs::read_to_string(&manifest_path).expect("manifest should read");
        std::fs::write(
            &manifest_path,
            manifest.replace(
                r#"project_version = "1.0.0""#,
                r#"project_version = "0.5.0""#,
            ),
        )
        .expect("manifest should downgrade");
        let prepared_c =
            super::prepare_package_root(package_root.clone(), hosted_source("source-c"), None)
                .expect("older cross-source package should prepare");
        let older = super::materialize_prepared_package(
            &prepared_c,
            Some("reports"),
            true,
            false,
            false,
            false,
            true,
        )
        .expect("cross-source replacement should not require --downgrade");
        assert!(matches!(older.action, InstallAction::Replace));
        let installed = load_installed_package("reports").expect("older receipt should load");
        assert_eq!(installed.package_version, "0.5.0");
        assert_eq!(
            installed.source.hosted_source_id.as_deref(),
            Some("source-c")
        );

        let manifest = std::fs::read_to_string(&manifest_path).expect("manifest should read");
        std::fs::write(
            &manifest_path,
            manifest.replace(
                r#"project_version = "0.5.0""#,
                r#"project_version = "9.0.0""#,
            ),
        )
        .expect("manifest should upgrade");
        let prepared_d =
            super::prepare_package_root(package_root.clone(), hosted_source("source-d"), None)
                .expect("newer cross-source package should prepare");
        let newer = super::materialize_prepared_package(
            &prepared_d,
            Some("reports"),
            true,
            false,
            false,
            false,
            true,
        )
        .expect("newer cross-source identity should remain a replacement");
        assert!(matches!(newer.action, InstallAction::Replace));
        let installed = load_installed_package("reports").expect("newer receipt should load");
        assert_eq!(installed.package_version, "9.0.0");
        assert_eq!(
            installed.source.hosted_source_id.as_deref(),
            Some("source-d")
        );
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn package_upgrade_preserves_alias_data_directory() {
        let _store = PackagesRootGuard::new("preserve-data");
        let package_root = temp_package_root("preserve-data");
        let request = local_install_request(&package_root);
        let action = install_local_package(&request).expect("first install should succeed");
        assert!(matches!(action, InstallAction::New));

        let data_root = super::installed_package_data_root("data_integration");
        std::fs::create_dir_all(&data_root).expect("data root should exist");
        std::fs::write(data_root.join("state.json"), r#"{"kept":true}"#)
            .expect("data file should be writable");

        let manifest_path = package_root.join("cargo-ai-package.toml");
        let manifest = std::fs::read_to_string(&manifest_path).expect("manifest should read");
        std::fs::write(
            &manifest_path,
            manifest.replace(
                r#"project_version = "1.0.0""#,
                r#"project_version = "1.1.0""#,
            ),
        )
        .expect("manifest should update");

        let action = install_local_package(&request).expect("upgrade should succeed");
        assert!(matches!(action, InstallAction::Upgrade));
        assert_eq!(
            std::fs::read_to_string(data_root.join("state.json"))
                .expect("data file should be preserved"),
            r#"{"kept":true}"#
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn package_upgrade_restores_alias_after_failure_following_backup() {
        assert_failed_upgrade_restores_previous_install(
            StagedInstallFailurePoint::AfterBackup,
            "failure-after-backup",
        );
    }

    #[test]
    fn package_upgrade_restores_alias_after_backup_inspection_failure() {
        assert_failed_upgrade_restores_previous_install(
            StagedInstallFailurePoint::BackupInspection,
            "failure-backup-inspection",
        );
    }

    #[test]
    fn package_upgrade_restores_alias_after_failure_following_data_transfer() {
        assert_failed_upgrade_restores_previous_install(
            StagedInstallFailurePoint::AfterDataTransfer,
            "failure-after-data-transfer",
        );
    }

    #[test]
    fn package_upgrade_restores_alias_after_final_replacement_failure() {
        assert_failed_upgrade_restores_previous_install(
            StagedInstallFailurePoint::FinalAliasReplacement,
            "failure-final-replacement",
        );
    }

    #[test]
    fn package_upgrade_retains_recovery_paths_when_alias_restore_fails() {
        let (_store, package_root, request) = prepare_package_upgrade("failure-restore-alias");
        super::set_test_staged_install_failures(&[
            StagedInstallFailurePoint::FinalAliasReplacement,
            StagedInstallFailurePoint::RestoreAlias,
        ]);

        let error = install_local_package(&request).expect_err("upgrade should fail");
        assert!(error.contains("Automatic recovery for package alias `data_integration` failed"));

        let mut transaction_paths = std::fs::read_dir(super::packages_staging_root())
            .expect("staging root should remain readable")
            .map(|entry| entry.expect("staging entry should be readable").path())
            .collect::<Vec<_>>();
        transaction_paths.sort();
        assert_eq!(transaction_paths.len(), 2);
        let backup_root = transaction_paths
            .iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.ends_with("-backup"))
                    .unwrap_or(false)
            })
            .expect("backup should be retained");
        let replacement_root = transaction_paths
            .iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.ends_with("-replacement"))
                    .unwrap_or(false)
            })
            .expect("replacement should be retained");
        assert!(error.contains(backup_root.to_string_lossy().as_ref()));
        assert!(error.contains(replacement_root.to_string_lossy().as_ref()));
        assert_eq!(
            std::fs::read_to_string(backup_root.join("data/state.json"))
                .expect("backup should retain package data"),
            r#"{"kept":true}"#
        );
        assert!(backup_root.join("install.toml").is_file());
        assert!(replacement_root.join("install.toml").is_file());
        assert!(!super::installed_package_root("data_integration").exists());

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[cfg(unix)]
    #[test]
    fn package_upgrade_rejects_symlinked_installed_data_root() {
        use std::os::unix::fs::symlink;

        let (store, package_root, request) = prepare_package_upgrade("upgrade-data-root-symlink");
        let data_root = super::installed_package_data_root("data_integration");
        let external_data_root = store.path.join("external-data");
        std::fs::remove_dir_all(&data_root).expect("installed data root should be removable");
        std::fs::create_dir_all(&external_data_root)
            .expect("external data root should be writable");
        std::fs::write(external_data_root.join("state.json"), "outside")
            .expect("external data should be writable");
        symlink(&external_data_root, &data_root).expect("data root symlink should be created");

        let error = install_local_package(&request).expect_err("upgrade should reject symlink");

        assert!(error.contains("has an unsafe data root"));
        assert_eq!(
            load_installed_package("data_integration")
                .expect("previous install should remain")
                .package_version,
            "1.0.0"
        );
        assert_eq!(
            std::fs::read_to_string(external_data_root.join("state.json"))
                .expect("external data should remain untouched"),
            "outside"
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[cfg(unix)]
    #[test]
    fn package_install_rejects_symlinked_staging_root() {
        use std::os::unix::fs::symlink;

        let store = PackagesRootGuard::new("staging-root-symlink");
        let package_root = temp_package_root("staging-root-symlink");
        let packages_root = super::packages_root();
        let external_staging_root = store.path.join("external-staging");
        std::fs::create_dir_all(&packages_root).expect("package store should be writable");
        std::fs::create_dir_all(&external_staging_root)
            .expect("external staging root should be writable");
        symlink(&external_staging_root, super::packages_staging_root())
            .expect("staging root symlink should be created");

        let error = install_local_package(&local_install_request(&package_root))
            .expect_err("symlinked staging root should be rejected");

        assert!(error.contains("Package staging root"));
        assert!(
            std::fs::read_dir(&external_staging_root)
                .expect("external staging root should remain readable")
                .next()
                .is_none(),
            "external staging root must remain untouched"
        );

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[cfg(unix)]
    #[test]
    fn installed_package_operations_reject_symlinked_alias_root() {
        use std::os::unix::fs::symlink;

        let store = PackagesRootGuard::new("installed-alias-symlink");
        let packages_root = super::packages_root();
        let external_alias_root = store.path.join("external-alias");
        std::fs::create_dir_all(&packages_root).expect("package store should be writable");
        std::fs::create_dir_all(&external_alias_root)
            .expect("external alias root should be writable");
        std::fs::write(external_alias_root.join("install.toml"), "outside")
            .expect("external metadata should be writable");
        symlink(
            &external_alias_root,
            super::installed_package_root("data_integration"),
        )
        .expect("installed alias symlink should be created");

        let load_error = load_installed_package("data_integration")
            .expect_err("symlinked alias metadata should not be read");
        assert!(load_error.contains("symbolic link"));
        let uninstall_error = uninstall_package("data_integration", false)
            .expect_err("symlinked alias should not be uninstalled");
        assert!(uninstall_error.contains("symbolic link"));
        assert_eq!(
            std::fs::read_to_string(external_alias_root.join("install.toml"))
                .expect("external metadata should remain untouched"),
            "outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn package_alias_lock_rejects_symlinked_package_store_root() {
        use std::os::unix::fs::symlink;

        let store = PackagesRootGuard::new("package-lock-store-symlink");
        let packages_root = super::packages_root();
        let external_store = store.path.join("external-packages");
        std::fs::create_dir_all(&external_store).expect("external store should exist");
        std::fs::create_dir_all(
            packages_root
                .parent()
                .expect("package store should have a parent"),
        )
        .expect("package store parent should exist");
        symlink(&external_store, &packages_root).expect("package store symlink should be created");

        let error = super::acquire_package_alias_lock("reports")
            .err()
            .expect("symlinked package store must not redirect lock creation");
        assert!(error.contains("Package store"));
        assert!(error.contains("symbolic link or reparse point"));
        assert!(!external_store.join(".locks").exists());
    }

    #[test]
    fn package_data_path_allows_missing_trailing_components() {
        let store = PackagesRootGuard::new("data-path-missing");
        let data_root = store.path.join("data");
        std::fs::create_dir_all(&data_root).expect("data root should be writable");
        let context = runtime_context(data_root.clone());

        let resolved = resolve_package_data_path(&context, Path::new("usage/2026/07/events.jsonl"))
            .expect("missing trailing components should be allowed");

        assert_eq!(resolved, data_root.join("usage/2026/07/events.jsonl"));
    }

    #[test]
    fn package_data_path_allows_missing_data_root() {
        let store = PackagesRootGuard::new("data-root-missing");
        let data_root = store.path.join("data");
        let context = runtime_context(data_root.clone());

        let resolved = resolve_package_data_path(&context, Path::new("usage/events.jsonl"))
            .expect("missing data root should be creatable by the caller");

        assert_eq!(resolved, data_root.join("usage/events.jsonl"));
    }

    #[cfg(unix)]
    #[test]
    fn package_data_path_rejects_symlinked_data_root() {
        use std::os::unix::fs::symlink;

        let store = PackagesRootGuard::new("data-path-root-symlink");
        let target_root = store.path.join("target");
        let data_root = store.path.join("data");
        std::fs::create_dir_all(&target_root).expect("target should be writable");
        symlink(&target_root, &data_root).expect("data root symlink should be created");
        let context = runtime_context(data_root);

        let error = resolve_package_data_path(&context, Path::new("events.jsonl"))
            .expect_err("symlinked data root should be rejected");

        assert!(error.contains("must be a real directory and not a symbolic link"));
    }

    #[cfg(unix)]
    #[test]
    fn package_data_path_rejects_nested_symlink() {
        use std::os::unix::fs::symlink;

        let store = PackagesRootGuard::new("data-path-nested-symlink");
        let data_root = store.path.join("data");
        let target_root = store.path.join("target");
        std::fs::create_dir_all(&data_root).expect("data root should be writable");
        std::fs::create_dir_all(&target_root).expect("target should be writable");
        symlink(&target_root, data_root.join("external"))
            .expect("nested symlink should be created");
        let context = runtime_context(data_root);

        let error = resolve_package_data_path(&context, Path::new("external/events.jsonl"))
            .expect_err("nested symlink should be rejected");

        assert!(error.contains("must not traverse symbolic link"));
    }

    #[cfg(unix)]
    #[test]
    fn package_data_path_rejects_symlink_target_leaf() {
        use std::os::unix::fs::symlink;

        let store = PackagesRootGuard::new("data-path-leaf-symlink");
        let data_root = store.path.join("data");
        let target_file = store.path.join("outside.jsonl");
        std::fs::create_dir_all(&data_root).expect("data root should be writable");
        std::fs::write(&target_file, "outside").expect("target should be writable");
        symlink(&target_file, data_root.join("events.jsonl"))
            .expect("leaf symlink should be created");
        let context = runtime_context(data_root);

        let error = resolve_package_data_path(&context, Path::new("events.jsonl"))
            .expect_err("symlink target leaf should be rejected");

        assert!(error.contains("must not traverse symbolic link"));
    }

    #[test]
    fn hosted_first_install_requires_acceptance_for_subprocess_permission() {
        let permissions = PackagePermissionProfileDocument {
            subprocess: "allowed".to_string(),
            ..PackagePermissionProfileDocument::default()
        };

        let source = hosted_source("source-id");
        let error = ensure_hosted_permissions_are_accepted(
            None,
            &source,
            &permissions,
            "demo",
            "1.0.0",
            false,
        )
        .expect_err("subprocess permission should require explicit acceptance");
        assert!(error.contains("--accept-permissions"));

        ensure_hosted_permissions_are_accepted(None, &source, &permissions, "demo", "1.0.0", true)
            .expect("explicit acceptance should allow subprocess permission");
    }

    #[test]
    fn hosted_subprocess_permission_summary_discloses_unsandboxed_build_authority() {
        let permissions = PackagePermissionProfileDocument {
            subprocess: "allowed".to_string(),
            ..PackagePermissionProfileDocument::default()
        };

        let summary = super::permission_profile_lines(&permissions).join("\n");

        assert!(summary.contains("publisher build scripts and proc macros"));
        assert!(summary.contains("unsandboxed code"));
        assert!(summary.contains("ambient filesystem, environment, and network authority"));
        assert!(summary.contains("install only trusted packages"));
    }

    #[test]
    fn hosted_permission_expansion_requires_acceptance() {
        let existing = super::InstalledPackageDocument {
            format_version: 1,
            alias: "demo".to_string(),
            package_name: "demo".to_string(),
            package_version: "1.0.0".to_string(),
            profile: "default".to_string(),
            content_sha256: "abc".to_string(),
            source: super::InstalledPackageSourceDocument {
                kind: "hosted".to_string(),
                path: None,
                account_selector: Some("self".to_string()),
                requested_owner_handle: None,
                hosted_source_id: Some("source-id".to_string()),
                hosted_version_id: Some("version-id".to_string()),
                owner_handle: None,
            },
            installed_at: "2026-07-25T00:00:00Z".to_string(),
            permissions: PackagePermissionProfileDocument::default(),
            entrypoints: Vec::new(),
        };
        let mut expanded = existing.permissions.clone();
        expanded.subprocess = "allowed".to_string();

        assert!(ensure_hosted_permissions_are_accepted(
            Some(&existing),
            &existing.source,
            &expanded,
            "demo",
            "1.1.0",
            false,
        )
        .is_err());
        ensure_hosted_permissions_are_accepted(
            Some(&existing),
            &existing.source,
            &expanded,
            "demo",
            "1.1.0",
            true,
        )
        .expect("accepted transition should succeed");

        let different_source = hosted_source("different-source");
        let reset_error = ensure_hosted_permissions_are_accepted(
            Some(&existing),
            &different_source,
            &expanded,
            "demo",
            "1.1.0",
            false,
        )
        .expect_err("a different hosted source must not inherit permission acceptance");
        assert!(reset_error.contains("--accept-permissions"));
    }

    #[test]
    fn hosted_project_workspace_access_is_unsupported_even_when_accepted() {
        let permissions = PackagePermissionProfileDocument {
            project_workspace: "read".to_string(),
            ..PackagePermissionProfileDocument::default()
        };

        let error = ensure_hosted_permissions_are_accepted(
            None,
            &hosted_source("source-id"),
            &permissions,
            "demo",
            "1.0.0",
            true,
        )
        .expect_err("project access must remain unsupported");
        assert!(error.contains("unsupported project/workspace access"));
        assert!(error.contains("even with `--accept-permissions`"));
    }

    #[test]
    fn hosted_install_response_must_match_requested_identity_and_version() {
        let response = serde_json::json!({
            "project": "demo",
            "project_version": "1.2.3"
        });
        super::validate_hosted_response_matches_request(&response, "demo", Some("1.2.3"))
            .expect("matching hosted response should pass");

        let package_error =
            super::validate_hosted_response_matches_request(&response, "other", None)
                .expect_err("wrong package identity should fail");
        assert!(package_error.contains("requested package `other`"));
        let version_error =
            super::validate_hosted_response_matches_request(&response, "demo", Some("1.2.2"))
                .expect_err("wrong exact version should fail");
        assert!(version_error.contains("exact requested version 1.2.2"));
    }

    #[test]
    fn hosted_pull_provenance_validates_handle_and_stable_source_id() {
        let response = serde_json::json!({
            "owner_handle": "alice",
            "hosted_source_id": "source-id"
        });
        super::validate_hosted_response_provenance(&response, Some("Alice"), None)
            .expect("normalized requested handle should match response provenance");
        super::validate_hosted_response_provenance(&response, None, Some("source-id"))
            .expect("stable source id should match response provenance");

        let owner_error = super::validate_hosted_response_provenance(&response, Some("bob"), None)
            .expect_err("different owner should fail closed");
        assert!(owner_error.contains("requested owner"));
        let source_error =
            super::validate_hosted_response_provenance(&response, None, Some("other-source"))
                .expect_err("different source id should fail closed");
        assert!(source_error.contains("requested source id"));
    }

    #[test]
    fn invalid_prepared_archives_remove_their_staging_roots() {
        let store = PackagesRootGuard::new("invalid-prepared-cleanup");
        let source_root = temp_package_root("invalid-prepared-cleanup");
        std::fs::remove_file(source_root.join("cargo-ai-package.toml"))
            .expect("fixture manifest should be removable");
        let archive = crate::commands::account::create_package_archive_bytes(&source_root)
            .expect("invalid package fixture should still archive");
        std::fs::create_dir_all(&store.path).expect("archive parent should be writable");
        let archive_path = store.path.join("invalid-package.tar.gz");
        std::fs::write(&archive_path, &archive).expect("archive fixture should be writable");

        let local_error = super::prepare_archive_source(&archive_path, "invalid-package.tar.gz")
            .expect_err("archive without a manifest should fail");
        assert!(local_error.contains("Failed to read package manifest"));
        assert_eq!(
            std::fs::read_dir(super::packages_staging_root())
                .expect("package staging root should remain readable")
                .count(),
            0,
            "local archive validation failure must remove its staging tree"
        );

        let hosted_root = temp_package_root("invalid-hosted-prepared-cleanup");
        let hosted_archive = crate::commands::account::create_package_archive_bytes(&hosted_root)
            .expect("hosted fixture should archive");
        let response = serde_json::json!({
            "project": "unexpected_project",
            "project_version": "1.0.0",
            "hosted_source_id": "source-id",
            "hosted_version_id": "version-id",
            "package_sha256": crate::commands::account::sha256_hex(&hosted_archive),
            "package_size_bytes": hosted_archive.len(),
            "package_archive_base64": base64::engine::general_purpose::STANDARD.encode(&hosted_archive)
        });
        let hosted_error = super::prepare_hosted_response(&response, None, None)
            .expect_err("hosted response with mismatched manifest should fail");
        assert!(hosted_error.contains("did not match package manifest"));
        assert_eq!(
            std::fs::read_dir(super::packages_staging_root())
                .expect("package staging root should remain readable")
                .count(),
            0,
            "hosted response validation failure must remove its staging tree"
        );

        remove_temp_dir_if_present(source_root.as_path());
        remove_temp_dir_if_present(hosted_root.as_path());
    }

    #[test]
    fn installed_manifest_reads_legacy_owner_id_but_does_not_emit_it() {
        let legacy = r#"
format_version = 1
alias = "demo"
package_name = "demo"
package_version = "1.0.0"
profile = "default"
content_sha256 = "abc"
installed_at = "2026-07-25T00:00:00Z"
entrypoints = []

[source]
kind = "hosted"
hosted_source_id = "source-id"
hosted_version_id = "version-id"
owner_account_id = "private-account-id"

[permissions]
package_payload = "read"
package_data = "read_write"
project_workspace = "explicit_grant_required"
subprocess = "blocked_without_explicit_grant"
"#;
        let document: super::InstalledPackageDocument =
            toml::from_str(legacy).expect("legacy install manifest should remain readable");
        assert_eq!(
            document.source.hosted_source_id.as_deref(),
            Some("source-id")
        );

        let rendered =
            toml::to_string_pretty(&document).expect("install manifest should serialize");
        assert!(!rendered.contains("owner_account_id"));
    }

    #[test]
    fn portable_package_paths_reject_windows_prefix_forms_on_every_platform() {
        for candidate in [
            "C:relative/file.json",
            "C:\\absolute\\file.json",
            "\\\\server\\share\\file.json",
            "\\\\?\\C:\\file.json",
            "/absolute/file.json",
            "../escape.json",
        ] {
            let error = normalize_portable_relative_path(candidate, "Test path")
                .expect_err("non-portable path should be rejected");
            assert!(
                error.contains("path") || error.contains("traversal") || error.contains("prefix"),
                "unexpected error for {candidate}: {error}"
            );
        }
    }

    #[test]
    fn package_alias_and_entrypoint_reject_option_like_identifiers() {
        let alias_error = super::validate_package_alias("-reports")
            .expect_err("package alias must not be parsed as a CLI option");
        assert!(alias_error.contains("Start with a letter or number"));
        let entrypoint_error = super::validate_entrypoint_name("-daily")
            .expect_err("entrypoint must not be parsed as a CLI option");
        assert!(entrypoint_error.contains("Start with a letter or number"));
    }

    #[test]
    fn runtime_context_rejects_unverified_directory_named_package() {
        let _store = PackagesRootGuard::new("runtime-context-identity");
        let package_root = temp_package_root("runtime-context-identity");
        let request = local_install_request(&package_root);
        install_local_package(&request).expect("package should install");

        let fake_install_root = std::env::temp_dir()
            .join(format!("fake-package-root-{}", uuid::Uuid::new_v4()))
            .join("data_integration");
        let fake_package_root = fake_install_root.join("package");
        std::fs::create_dir_all(&fake_package_root).expect("fake package root should exist");

        assert!(runtime_context_for_package_root(fake_package_root.as_path()).is_none());

        remove_temp_dir_if_present(
            fake_install_root
                .parent()
                .expect("fake install root should have parent"),
        );
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn checked_runtime_context_rejects_malformed_receipt_for_exact_installed_root() {
        let _store = PackagesRootGuard::new("runtime-context-malformed-receipt");
        let package_root = temp_package_root("runtime-context-malformed-receipt");
        install_local_package(&local_install_request(&package_root))
            .expect("package should install");
        let installed_root = super::installed_package_root("data_integration");
        let installed_payload_root = installed_root.join(super::INSTALLED_PACKAGE_DIR_NAME);
        std::fs::write(
            installed_root.join(super::INSTALL_MANIFEST_FILE_NAME),
            "not = [valid",
        )
        .expect("receipt should be corruptible");

        let error = checked_runtime_context_for_project_root(&installed_payload_root)
            .expect_err("exact installed package root must fail closed on corrupt metadata");
        assert!(error.contains("Failed to parse installed package metadata"));

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[cfg(unix)]
    #[test]
    fn checked_runtime_context_classifies_symlink_into_installed_payload() {
        use std::os::unix::fs::symlink;

        let _store = PackagesRootGuard::new("runtime-context-symlink-alias");
        let package_root = temp_package_root("runtime-context-symlink-alias");
        install_local_package(&local_install_request(&package_root))
            .expect("package should install");
        let installed_root = super::installed_package_root("data_integration");
        let installed_payload_root = installed_root.join(super::INSTALLED_PACKAGE_DIR_NAME);
        let linked_payload_root = std::env::temp_dir().join(format!(
            "cargo-ai-linked-installed-payload-{}",
            uuid::Uuid::new_v4()
        ));
        symlink(&installed_payload_root, &linked_payload_root)
            .expect("payload symlink should be created");

        assert!(
            checked_runtime_context_for_project_root(&linked_payload_root)
                .expect("valid installed metadata should load through an external symlink")
                .is_some()
        );
        std::fs::write(
            installed_root.join(super::INSTALL_MANIFEST_FILE_NAME),
            "not = [valid",
        )
        .expect("receipt should be corruptible");
        let error = checked_runtime_context_for_project_root(&linked_payload_root)
            .expect_err("a symlink into corrupt installed metadata must fail closed");
        assert!(error.contains("Failed to parse installed package metadata"));

        std::fs::remove_file(&linked_payload_root).expect("payload symlink should be removable");
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn installed_definition_and_cwd_fail_closed_without_project_metadata() {
        let _store = PackagesRootGuard::new("runtime-context-missing-project-metadata");
        let package_root = temp_package_root("runtime-context-missing-project-metadata");
        install_local_package(&local_install_request(&package_root))
            .expect("package should install");
        let installed_payload_root = super::installed_package_root("data_integration")
            .join(super::INSTALLED_PACKAGE_DIR_NAME);
        let definition_path = installed_payload_root.join("agents/lookup_account.json");
        std::fs::remove_file(installed_payload_root.join(".cargo-ai/project.toml"))
            .expect("installed project metadata should be removable");

        let local_path_error = checked_runtime_context_for_path(&definition_path)
            .expect_err("a direct installed JSON path must require project metadata");
        assert!(local_path_error.contains("Installed package project metadata"));
        let cwd_error = checked_runtime_context_for_path(&installed_payload_root)
            .expect_err("inline/stdin from an installed payload must require project metadata");
        assert!(cwd_error.contains("Installed package project metadata"));

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[test]
    fn direct_installed_definition_requires_exported_entrypoint_capability() {
        let _store = PackagesRootGuard::new("runtime-context-entrypoint-capability");
        let package_root = temp_package_root("runtime-context-entrypoint-capability");
        install_local_package(&local_install_request(&package_root))
            .expect("package should install");
        let installed_payload_root = super::installed_package_root("data_integration")
            .join(super::INSTALLED_PACKAGE_DIR_NAME);
        let runnable = installed_payload_root.join("agents/lookup_account.json");
        let hatchable = installed_payload_root.join("agents/daily_digest.json");
        let private = installed_payload_root.join("agents/private.json");
        std::fs::write(&private, "{}").expect("private definition should exist");

        let checked = super::checked_runtime_lease_for_path(
            &runnable,
            Some(super::InstalledEntrypointCapability::Run),
        )
        .expect("runnable export should validate")
        .expect("runnable export should be recognized as installed");
        assert_eq!(
            checked.context.current_entrypoint_path.as_deref(),
            Some("agents/lookup_account.json")
        );
        let error = super::checked_runtime_lease_for_path(
            &runnable,
            Some(super::InstalledEntrypointCapability::Hatch),
        )
        .expect_err("run-only export must not hatch");
        assert!(error.contains("is not hatchable"));
        let checked = super::checked_runtime_lease_for_path(
            &hatchable,
            Some(super::InstalledEntrypointCapability::Hatch),
        )
        .expect("hatchable export should validate")
        .expect("hatchable export should be recognized as installed");
        assert_eq!(
            checked.context.current_entrypoint_path.as_deref(),
            Some("agents/daily_digest.json")
        );
        let error = super::checked_runtime_lease_for_path(
            &private,
            Some(super::InstalledEntrypointCapability::Run),
        )
        .expect_err("private installed JSON must not run as an exported entrypoint");
        assert!(error.contains("not an exported package entrypoint"));

        remove_temp_dir_if_present(package_root.as_path());
    }

    #[cfg(unix)]
    #[test]
    fn direct_symlink_to_exported_installed_definition_preserves_entrypoint_identity() {
        use std::os::unix::fs::symlink;

        let _store = PackagesRootGuard::new("runtime-context-entrypoint-symlink");
        let package_root = temp_package_root("runtime-context-entrypoint-symlink");
        install_local_package(&local_install_request(&package_root))
            .expect("package should install");
        let installed_definition = super::installed_package_root("data_integration")
            .join("package/agents/daily_digest.json");
        let linked_definition = std::env::temp_dir().join(format!(
            "cargo-ai-linked-installed-definition-{}.json",
            uuid::Uuid::new_v4()
        ));
        symlink(&installed_definition, &linked_definition)
            .expect("definition symlink should be created");

        let checked = super::checked_runtime_lease_for_path(
            &linked_definition,
            Some(super::InstalledEntrypointCapability::Hatch),
        )
        .expect("symlink to hatchable export should validate")
        .expect("symlink should be recognized as installed");
        assert_eq!(checked.context.alias, "data_integration");
        assert_eq!(
            checked.context.current_entrypoint_path.as_deref(),
            Some("agents/daily_digest.json")
        );

        std::fs::remove_file(linked_definition).expect("definition symlink should be removable");
        remove_temp_dir_if_present(package_root.as_path());
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_attributes_are_treated_as_unsafe() {
        assert!(super::windows_file_attributes_are_link_like(
            super::FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert!(!super::windows_file_attributes_are_link_like(0));
    }
}
