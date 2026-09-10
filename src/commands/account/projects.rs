//! Runtime behavior for account-backed package management.
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use clap::ArgMatches;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Cursor, Write};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use tar::{Archive, Builder, EntryType, HeaderMode};
use uuid::Uuid;

use crate::infra_api;
use crate::ui;

use super::helpers::{
    apply_projects_list_display_limit, load_account_auth, persist_refreshed_access_token,
    refresh_access_token_for_retry, RefreshAccessError, INFRA_BASE_URL,
};

#[derive(Clone, Debug)]
enum ProjectsCommand {
    List {
        owner_handle: Option<String>,
        include_archived: bool,
        display_limit: Option<usize>,
    },
    #[cfg(feature = "developer-tools")]
    Publish {
        profile: String,
        source_id: Option<String>,
    },
    Pull {
        source_id: Option<String>,
        version_id: Option<String>,
        name: String,
        owner_handle: Option<String>,
        version: Option<String>,
        output_dir: Option<PathBuf>,
        force: bool,
    },
    Rename {
        source_id: String,
        name: String,
    },
    Visibility {
        name: String,
        is_public: bool,
    },
    Archive {
        name: String,
        is_archived: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PackageArchiveDocument {
    format_version: u32,
    entries: Vec<PackageArchiveEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PackageArchiveEntry {
    path: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    contents_base64: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PulledPackageHostedReceiptDocument {
    format_version: u32,
    source_kind: String,
    hosted_source_id: String,
    hosted_version_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_handle: Option<String>,
    package_name: String,
    resolved_version: String,
    package_sha256: String,
}

#[cfg(feature = "developer-tools")]
#[derive(Clone, Debug)]
struct PublishPayload {
    receipt_root: PathBuf,
    request_id: String,
    hosted_source_id: Option<String>,
    project_name: String,
    project_version: String,
    package_manifest: Value,
    package_sha256: String,
    package_size_bytes: i64,
    package_archive_base64: String,
}

#[cfg(feature = "developer-tools")]
struct PublishStagingGuard {
    path: PathBuf,
}

#[cfg(feature = "developer-tools")]
impl PublishStagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[cfg(feature = "developer-tools")]
impl Drop for PublishStagingGuard {
    fn drop(&mut self) {
        let _ = remove_path_without_following_links(self.path.as_path());
    }
}

const PULLED_PROJECT_METADATA_RELATIVE_PATH: &str = ".cargo-ai/project.toml";
const PACKAGE_MANIFEST_FILE_NAME: &str = "cargo-ai-package.toml";
const PULLED_PACKAGE_RECEIPT_RELATIVE_PATH: &str = ".cargo-ai/origin/cargo-ai-package.toml";
const PULLED_PACKAGE_HOSTED_RECEIPT_RELATIVE_PATH: &str =
    ".cargo-ai/origin/cargo-ai-package-receipt.toml";
const ESTIMATED_PUBLISH_ACCESS_TOKEN: &str = "__publish-size-estimate__";
#[cfg(feature = "developer-tools")]
const SAFE_PROJECT_PUBLISH_REQUEST_LIMIT_BYTES: u64 = 5_500_000;
const HOSTED_ARCHIVE_MAX_COMPRESSED_BYTES: usize = 10 * 1024 * 1024;
const HOSTED_ARCHIVE_MAX_EXPANDED_BYTES: u64 = 100 * 1024 * 1024;
const HOSTED_ARCHIVE_MAX_ENTRIES: usize = 10_000;
const HOSTED_ARCHIVE_MAX_PATH_BYTES: usize = 1_024;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[cfg(test)]
thread_local! {
    static TEST_FAIL_PULL_ACTIVATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Clone, Copy)]
struct ArchiveLimits {
    compressed_bytes: usize,
    expanded_bytes: u64,
    entries: usize,
    path_bytes: usize,
}

const HOSTED_ARCHIVE_LIMITS: ArchiveLimits = ArchiveLimits {
    compressed_bytes: HOSTED_ARCHIVE_MAX_COMPRESSED_BYTES,
    expanded_bytes: HOSTED_ARCHIVE_MAX_EXPANDED_BYTES,
    entries: HOSTED_ARCHIVE_MAX_ENTRIES,
    path_bytes: HOSTED_ARCHIVE_MAX_PATH_BYTES,
};

pub async fn run(projects_m: &ArgMatches) -> bool {
    let projects_command = if let Some(list_m) = projects_m.subcommand_matches("list") {
        let owner_handle =
            crate::commands::local_packages::account_handle_from_list_matches(list_m).flatten();
        ProjectsCommand::List {
            owner_handle,
            include_archived: list_m.get_flag("include_archived"),
            display_limit: if list_m.get_flag("all") {
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
            },
        }
    } else if let Some(pull_m) = projects_m.subcommand_matches("pull") {
        let name = pull_m
            .get_one::<String>("name")
            .or_else(|| pull_m.get_one::<String>("name_positional"))
            .map(String::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() && pull_m.get_one::<String>("source_id").is_none() {
            eprintln!("x Missing package name. Provide NAME, --name <NAME>, or --source-id <ID>.");
            return false;
        }

        ProjectsCommand::Pull {
            name,
            source_id: pull_m.get_one::<String>("source_id").cloned(),
            version_id: pull_m.get_one::<String>("version_id").cloned(),
            owner_handle: pull_m
                .get_one::<String>("owner_handle")
                .or_else(|| pull_m.get_one::<String>("account"))
                .filter(|s| !s.is_empty())
                .cloned(),
            version: pull_m.get_one::<String>("version").map(|s| s.to_string()),
            output_dir: match pull_m.get_one::<String>("output_dir") {
                Some(raw) if raw.trim().is_empty() => {
                    eprintln!("x Output directory cannot be empty. Provide --output-dir <DIR>.");
                    return false;
                }
                Some(raw) => Some(PathBuf::from(raw)),
                None => None,
            },
            force: pull_m.get_flag("force"),
        }
    } else if let Some(rename_m) = projects_m.subcommand_matches("rename") {
        ProjectsCommand::Rename {
            source_id: rename_m.get_one::<String>("source_id").unwrap().clone(),
            name: rename_m.get_one::<String>("name").unwrap().clone(),
        }
    } else if let Some(visibility_m) = projects_m.subcommand_matches("visibility") {
        let Some(name) = visibility_m.get_one::<String>("name") else {
            eprintln!("x Missing package name. Provide --name <NAME>.");
            return false;
        };

        ProjectsCommand::Visibility {
            name: name.to_string(),
            is_public: visibility_m.get_flag("public"),
        }
    } else if let Some(archive_m) = projects_m.subcommand_matches("archive") {
        let Some(name) = archive_m.get_one::<String>("name") else {
            eprintln!("x Missing package name. Provide --name <NAME>.");
            return false;
        };

        ProjectsCommand::Archive {
            name: name.to_string(),
            is_archived: archive_m.get_flag("archive"),
        }
    } else {
        #[cfg(feature = "developer-tools")]
        if let Some(publish_m) = projects_m.subcommand_matches("publish") {
            ProjectsCommand::Publish {
                source_id: publish_m.get_one::<String>("source_id").cloned(),
                profile: publish_m
                    .get_one::<String>("profile")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "default".to_string()),
            }
        } else {
            eprintln!(
                "No packages subcommand found. Try 'cargo ai packages list|publish|pull|visibility|archive'."
            );
            return false;
        }

        #[cfg(not(feature = "developer-tools"))]
        {
            eprintln!(
                "No packages subcommand found. Try 'cargo ai packages list|pull|visibility|archive'."
            );
            return false;
        }
    };

    let auth = match load_account_auth() {
        Ok(auth) => auth,
        Err(message) => {
            eprintln!("{}", ui::account_status::normalize_leading_glyph(&message));
            return false;
        }
    };
    let access_token_owned = auth.access_token;
    let refresh_token = auth.refresh_token;

    #[cfg(feature = "developer-tools")]
    let mut prepared_publish_payload: Option<PublishPayload> = None;

    #[cfg(feature = "developer-tools")]
    if let ProjectsCommand::Publish { profile, source_id } = &projects_command {
        match prepare_publish_payload(profile.as_str()).and_then(|mut payload| {
            let root = current_project_root()?.ok_or("No project root")?;
            payload.receipt_root = root.clone();
            payload.hosted_source_id = source_id.clone();
            payload.request_id = crate::commands::package_publication::request_id(
                &root,
                &payload.project_name,
                &payload.project_version,
                &payload.package_sha256,
                source_id.as_deref(),
            )?;
            Ok(payload)
        }) {
            Ok(payload) => prepared_publish_payload = Some(payload),
            Err(error) => {
                eprintln!("x {error}");
                return false;
            }
        }
    }

    let mut response = match &projects_command {
        ProjectsCommand::List {
            owner_handle,
            include_archived,
            ..
        } => match infra_api::account::projects::list_projects(
            INFRA_BASE_URL,
            access_token_owned.as_str(),
            owner_handle.as_deref(),
            *include_archived,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("x Request failed: {e}");
                return false;
            }
        },
        #[cfg(feature = "developer-tools")]
        ProjectsCommand::Publish { .. } => {
            let payload = prepared_publish_payload
                .as_ref()
                .expect("publish payload should be prepared");

            match transmit_publish_payload(INFRA_BASE_URL, access_token_owned.as_str(), payload)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("x Request failed: {e}");
                    return false;
                }
            }
        }
        ProjectsCommand::Pull {
            source_id,
            version_id,
            name,
            owner_handle,
            version,
            ..
        } => match crate::commands::local_packages::pull_inspected_hosted_package(
            name,
            owner_handle.as_deref(),
            source_id.as_deref(),
            version.as_deref(),
            version_id.as_deref(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("x Request failed: {e}");
                return false;
            }
        },
        ProjectsCommand::Rename { source_id, name } => {
            match infra_api::account::projects::rename_project(
                INFRA_BASE_URL,
                access_token_owned.as_str(),
                source_id,
                name,
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    eprintln!("x Rename failed: {error}");
                    return false;
                }
            }
        }
        ProjectsCommand::Visibility { name, is_public } => {
            match infra_api::account::projects::set_project_visibility(
                INFRA_BASE_URL,
                access_token_owned.as_str(),
                name,
                *is_public,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("x Request failed: {e}");
                    return false;
                }
            }
        }
        ProjectsCommand::Archive { name, is_archived } => {
            match infra_api::account::projects::set_project_archive(
                INFRA_BASE_URL,
                access_token_owned.as_str(),
                name,
                *is_archived,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("x Request failed: {e}");
                    return false;
                }
            }
        }
    };

    let is_expired_error = response
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| t == "access_token_expired")
        .unwrap_or(false);

    if is_expired_error {
        match refresh_access_token_for_retry(access_token_owned.as_str(), refresh_token.as_deref())
            .await
        {
            Err(RefreshAccessError::MissingRefreshToken) => {
                eprintln!("! Access token expired, and no refresh token exists in credential store. Run `cargo ai account status` or re-confirm account.");
                render_account_projects_response(&response);
                return false;
            }
            Err(RefreshAccessError::RequestFailed(error)) => {
                eprintln!("x Request failed while refreshing session: {error}");
                return false;
            }
            Err(RefreshAccessError::MissingRefreshedToken(refresh_response)) => {
                eprintln!("! Session refresh did not return a new access token. Cannot retry projects request.");
                render_account_projects_response(&refresh_response);
                return false;
            }
            Ok((retry_access_token, refreshed_expires_in)) => {
                if let Some(rt) = refresh_token.as_deref() {
                    persist_refreshed_access_token(
                        retry_access_token.as_str(),
                        rt,
                        refreshed_expires_in,
                    );
                }

                response = match &projects_command {
                    ProjectsCommand::List {
                        owner_handle,
                        include_archived,
                        ..
                    } => match infra_api::account::projects::list_projects(
                        INFRA_BASE_URL,
                        retry_access_token.as_str(),
                        owner_handle.as_deref(),
                        *include_archived,
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("x Request failed after session refresh: {e}");
                            return false;
                        }
                    },
                    #[cfg(feature = "developer-tools")]
                    ProjectsCommand::Publish { .. } => {
                        let payload = prepared_publish_payload
                            .as_ref()
                            .expect("publish payload should be prepared");

                        match transmit_publish_payload(
                            INFRA_BASE_URL,
                            retry_access_token.as_str(),
                            payload,
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("x Request failed after session refresh: {e}");
                                return false;
                            }
                        }
                    }
                    ProjectsCommand::Pull {
                        source_id,
                        version_id,
                        name,
                        owner_handle,
                        version,
                        ..
                    } => match crate::commands::local_packages::pull_inspected_hosted_package(
                        name,
                        owner_handle.as_deref(),
                        source_id.as_deref(),
                        version.as_deref(),
                        version_id.as_deref(),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("x Request failed after session refresh: {e}");
                            return false;
                        }
                    },
                    ProjectsCommand::Rename { source_id, name } => {
                        match infra_api::account::projects::rename_project(
                            INFRA_BASE_URL,
                            retry_access_token.as_str(),
                            source_id,
                            name,
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(error) => {
                                eprintln!("x Rename failed: {error}");
                                return false;
                            }
                        }
                    }
                    ProjectsCommand::Visibility { name, is_public } => {
                        match infra_api::account::projects::set_project_visibility(
                            INFRA_BASE_URL,
                            retry_access_token.as_str(),
                            name,
                            *is_public,
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("x Request failed after session refresh: {e}");
                                return false;
                            }
                        }
                    }
                    ProjectsCommand::Archive { name, is_archived } => {
                        match infra_api::account::projects::set_project_archive(
                            INFRA_BASE_URL,
                            retry_access_token.as_str(),
                            name,
                            *is_archived,
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("x Request failed after session refresh: {e}");
                                return false;
                            }
                        }
                    }
                };
            }
        }
    }

    if let ProjectsCommand::List { display_limit, .. } = &projects_command {
        let _ = apply_projects_list_display_limit(&mut response, *display_limit);
    }

    if let ProjectsCommand::Pull {
        source_id,
        name,
        owner_handle,
        version,
        output_dir,
        force,
        ..
    } = &projects_command
    {
        if is_project_pull_success(&response) {
            if let Err(error) = validate_project_pull_response_matches_request(
                &response,
                if source_id.is_some() { "" } else { name },
                owner_handle.as_deref(),
                version.as_deref(),
            ) {
                eprintln!("x {error}");
                return false;
            }
            let resolved_name = response["project"].as_str().unwrap_or(name);
            if resolved_name.is_empty()
                || !resolved_name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                eprintln!("x Invalid package name in hosted response.");
                return false;
            }
            let output_path = output_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(resolved_name));

            if let Err(error) = restore_pulled_project(&response, &output_path, *force) {
                eprintln!("x {error}");
                return false;
            }

            response["ui"] = build_local_pull_ui(
                response
                    .get("owner_handle")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                response
                    .get("project")
                    .and_then(|value| value.as_str())
                    .unwrap_or(name),
                response
                    .get("project_version")
                    .and_then(|value| value.as_str())
                    .or(version.as_deref())
                    .unwrap_or("latest"),
                output_path.as_path(),
            );
        }
    }

    render_account_projects_response(&response);
    response
        .get("status")
        .and_then(|v| v.as_str())
        .map(|status| status.eq_ignore_ascii_case("success"))
        .unwrap_or(false)
}

#[cfg(feature = "developer-tools")]
async fn transmit_publish_payload(
    base_url: &str,
    token: &str,
    payload: &PublishPayload,
) -> Result<Value, String> {
    let response = infra_api::account::projects::publish_project(
        base_url,
        token,
        &payload.project_name,
        &payload.project_version,
        payload.package_manifest.clone(),
        &payload.package_sha256,
        payload.package_size_bytes,
        &payload.package_archive_base64,
        &payload.request_id,
        payload.hosted_source_id.as_deref(),
    )
    .await?;
    if response["status"] == "success" {
        crate::commands::package_publication::record_terminal(
            &payload.receipt_root,
            &payload.request_id,
            &payload.project_version,
            &payload.package_sha256,
            &response,
        )?;
    }
    Ok(response)
}

#[cfg(feature = "developer-tools")]
fn prepare_publish_payload(profile_name: &str) -> Result<PublishPayload, String> {
    let project_root = current_project_root()?.ok_or_else(|| {
        "No Cargo AI project metadata was found from the current directory upward.".to_string()
    })?;
    let staging_output_dir = project_root
        .join("target")
        .join("cargo-ai")
        .join("publish-tmp")
        .join(Uuid::new_v4().to_string());
    let staging_guard = PublishStagingGuard::new(staging_output_dir.clone());
    let staging_output_dir_raw = staging_output_dir.to_string_lossy().to_string();

    println!("Packaging profile `{profile_name}`...");
    println!("Project: {}", project_root.display());

    let assembled = crate::commands::package::assemble_current_project_package(
        profile_name,
        Some(staging_output_dir_raw.as_str()),
        true,
        false,
    )?;

    finish_publish_payload(assembled, staging_guard)
}

#[cfg(feature = "developer-tools")]
fn finish_publish_payload(
    assembled: crate::commands::package::AssembledPackage,
    _staging_guard: PublishStagingGuard,
) -> Result<PublishPayload, String> {
    let project_name = assembled.manifest_project_name.ok_or_else(|| {
        "Project publish requires `.cargo-ai/project.toml` `[project].name`.".to_string()
    })?;
    let project_version = assembled.manifest_project_version.ok_or_else(|| {
        "Project publish requires `.cargo-ai/project.toml` `[project].version`.".to_string()
    })?;

    Version::parse(project_version.as_str()).map_err(|error| {
        format!(
            "Project version '{}' is not valid semver: {}",
            project_version, error
        )
    })?;

    let archive_bytes = assembled.archive_bytes.clone();
    let package_sha256 = sha256_hex(archive_bytes.as_slice());
    let package_size_bytes = i64::try_from(archive_bytes.len())
        .map_err(|_| "Package archive size exceeded supported limits.".to_string())?;
    let package_archive_base64 = BASE64_STANDARD.encode(archive_bytes.as_slice());
    let estimated_request_size_bytes =
        crate::infra_api::account::projects::estimate_publish_project_request_size(
            ESTIMATED_PUBLISH_ACCESS_TOKEN,
            project_name.as_str(),
            project_version.as_str(),
            assembled.manifest_value.clone(),
            package_sha256.as_str(),
            package_size_bytes,
            package_archive_base64.as_str(),
        )?;

    println!(
        "Package size on disk: {}",
        format_bytes(assembled.assembled_size_bytes)
    );
    println!(
        "Archive size:         {}",
        format_bytes(assembled.archive_size_bytes)
    );
    println!(
        "Estimated request:    {}",
        format_bytes(estimated_request_size_bytes)
    );
    println!();

    if estimated_request_size_bytes.saturating_add(512) > SAFE_PROJECT_PUBLISH_REQUEST_LIMIT_BYTES {
        return Err(format!(
            "Estimated publish request size {} exceeds the current safe package-publish ceiling of about {}. Keep packaged assets minimal and remove large sample files before publishing.",
            format_bytes(estimated_request_size_bytes),
            format_bytes(SAFE_PROJECT_PUBLISH_REQUEST_LIMIT_BYTES),
        ));
    }

    Ok(PublishPayload {
        receipt_root: PathBuf::new(),
        request_id: String::new(),
        hosted_source_id: None,
        project_name,
        project_version,
        package_manifest: assembled.manifest_value,
        package_sha256,
        package_size_bytes,
        package_archive_base64,
    })
}

fn render_account_projects_response(response: &Value) {
    if !ui::account_status::render_backend_ui(response) {
        match serde_json::to_string_pretty(response) {
            Ok(pretty) => println!("{pretty}"),
            Err(_) => println!("{response:?}"),
        }
    }
}

fn current_project_root() -> Result<Option<PathBuf>, String> {
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("Failed to inspect the current project directory: {error}"))?;
    crate::commands::package_dependencies::find_project_root(current_dir.as_path())
}

fn is_project_pull_success(response: &Value) -> bool {
    response
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| t == "account_projects_pull_succeeded")
        .unwrap_or(false)
}

pub(crate) fn validate_project_pull_response_matches_request(
    response: &Value,
    requested_name: &str,
    requested_owner_handle: Option<&str>,
    requested_version: Option<&str>,
) -> Result<(), String> {
    let resolved_name = response
        .get("display_name")
        .or_else(|| response.get("project"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Hosted pull response did not include `project`.".to_string())?;
    if !requested_name.is_empty() && resolved_name != requested_name {
        return Err(format!(
            "Hosted pull response returned package `{resolved_name}` for requested package `{requested_name}`."
        ));
    }

    if let Some(requested_handle) = requested_owner_handle {
        let returned_handle = response
            .get("owner_handle")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "Hosted pull response did not include `owner_handle` for an explicit owner request."
                    .to_string()
            })?;
        if returned_handle.to_ascii_lowercase() != requested_handle.trim().to_ascii_lowercase() {
            return Err(format!(
                "Hosted pull response returned owner handle `{returned_handle}` for requested owner `{requested_handle}`."
            ));
        }
    }

    let resolved_version_raw = response
        .get("project_version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Hosted pull response did not include `project_version`.".to_string())?;
    let resolved_version = Version::parse(resolved_version_raw).map_err(|error| {
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

fn restore_pulled_project(response: &Value, output_path: &Path, force: bool) -> Result<(), String> {
    let archive_base64 = response
        .get("package_archive_base64")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            "Pull succeeded but response did not include `package_archive_base64`.".to_string()
        })?;
    let package_sha256 = response
        .get("package_sha256")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            "Pull succeeded but response did not include `package_sha256`.".to_string()
        })?;
    let package_size_bytes = response
        .get("package_size_bytes")
        .and_then(|value| value.as_i64())
        .ok_or_else(|| {
            "Pull succeeded but response did not include `package_size_bytes`.".to_string()
        })?;
    let declared_size_bytes = usize::try_from(package_size_bytes).map_err(|_| {
        "Pull response declared an invalid negative or unsupported package archive size."
            .to_string()
    })?;
    validate_package_archive_size(declared_size_bytes)?;

    let archive_bytes = decode_package_archive_base64(archive_base64)?;
    let decoded_size_bytes = i64::try_from(archive_bytes.len())
        .map_err(|_| "Decoded package archive exceeded supported size limits.".to_string())?;
    if decoded_size_bytes != package_size_bytes {
        return Err(format!(
            "Package archive size mismatch. Expected {} bytes, got {} bytes after decoding.",
            package_size_bytes, decoded_size_bytes
        ));
    }

    let decoded_sha256 = sha256_hex(archive_bytes.as_slice());
    if decoded_sha256 != package_sha256 {
        return Err(format!(
            "Package archive checksum mismatch. Expected {}, got {}.",
            package_sha256, decoded_sha256
        ));
    }

    let output_path = resolve_pull_output_path(output_path, force)?;
    let transaction_id = Uuid::new_v4();
    let output_parent = output_path
        .parent()
        .expect("validated pull output path should have a parent");
    let staging_path = output_parent.join(format!(".cargo-ai-pull-{transaction_id}-staging"));
    let backup_path = output_parent.join(format!(".cargo-ai-pull-{transaction_id}-backup"));
    fs::create_dir(&staging_path).map_err(|error| {
        format!(
            "Failed to create pull staging directory '{}': {}",
            staging_path.display(),
            error
        )
    })?;

    let prepare_result = (|| {
        ensure_archive_path_is_safe(&staging_path, Path::new(""))?;
        extract_package_archive_bytes(archive_bytes.as_slice(), &staging_path)?;
        validate_restored_package_manifest(response, &staging_path)?;
        relocate_pulled_package_receipt(&staging_path)?;
        write_pulled_package_hosted_receipt(response, &staging_path)
    })();
    if let Err(error) = prepare_result {
        let _ = remove_path_without_following_links(&staging_path);
        return Err(error);
    }

    replace_output_with_staged(&output_path, &staging_path, &backup_path, force)
}

fn validate_restored_package_manifest(response: &Value, project_root: &Path) -> Result<(), String> {
    let manifest_path = project_root.join(PACKAGE_MANIFEST_FILE_NAME);
    let contents = fs::read_to_string(&manifest_path).map_err(|error| {
        format!(
            "Failed to read restored package manifest '{}': {}",
            manifest_path.display(),
            error
        )
    })?;
    let manifest: toml::Value = toml::from_str(contents.as_str()).map_err(|error| {
        format!(
            "Failed to parse restored package manifest '{}': {}",
            manifest_path.display(),
            error
        )
    })?;
    let manifest_name = manifest
        .get("project_name")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "Restored package manifest '{}' is missing `project_name`.",
                manifest_path.display()
            )
        })?;
    let manifest_version_raw = manifest
        .get("project_version")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "Restored package manifest '{}' is missing `project_version`.",
                manifest_path.display()
            )
        })?;
    let response_name = response
        .get("project")
        .and_then(Value::as_str)
        .ok_or_else(|| "Hosted pull response did not include `project`.".to_string())?;
    let response_version_raw = response
        .get("project_version")
        .and_then(Value::as_str)
        .ok_or_else(|| "Hosted pull response did not include `project_version`.".to_string())?;
    let manifest_version = Version::parse(manifest_version_raw).map_err(|error| {
        format!("Restored package manifest version `{manifest_version_raw}` is invalid: {error}")
    })?;
    let response_version = Version::parse(response_version_raw).map_err(|error| {
        format!("Hosted pull response version `{response_version_raw}` is invalid: {error}")
    })?;

    if manifest_name != response_name || manifest_version != response_version {
        return Err(format!(
            "Restored package manifest identity `{manifest_name}` {manifest_version} did not match hosted response identity `{response_name}` {response_version}."
        ));
    }
    crate::commands::package_inspection::validate_archive_manifest(response, project_root)
}

fn relocate_pulled_package_receipt(project_root: &Path) -> Result<(), String> {
    let project_metadata_path = project_root.join(PULLED_PROJECT_METADATA_RELATIVE_PATH);
    if !project_metadata_path.exists() {
        return Err(format!(
            "Pulled project is missing '{}'.",
            project_metadata_path.display()
        ));
    }

    let root_receipt_path = project_root.join(PACKAGE_MANIFEST_FILE_NAME);
    if !root_receipt_path.exists() {
        return Ok(());
    }

    let origin_receipt_path = project_root.join(PULLED_PACKAGE_RECEIPT_RELATIVE_PATH);
    if let Some(parent) = origin_receipt_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create pulled-project receipt directory '{}': {}",
                parent.display(),
                error
            )
        })?;
    }

    fs::rename(&root_receipt_path, &origin_receipt_path).map_err(|error| {
        format!(
            "Failed to move pulled package receipt from '{}' to '{}': {}",
            root_receipt_path.display(),
            origin_receipt_path.display(),
            error
        )
    })
}

fn write_pulled_package_hosted_receipt(
    response: &Value,
    project_root: &Path,
) -> Result<(), String> {
    let hosted_source_id = response
        .get("hosted_source_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "Pull succeeded but response did not include `hosted_source_id`.".to_string()
        })?;
    let hosted_version_id = response
        .get("hosted_version_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "Pull succeeded but response did not include `hosted_version_id`.".to_string()
        })?;
    let package_name = response
        .get("project")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Pull succeeded but response did not include `project`.".to_string())?;
    let resolved_version = response
        .get("project_version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "Pull succeeded but response did not include `project_version`.".to_string()
        })?;
    let package_sha256 = response
        .get("package_sha256")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "Pull succeeded but response did not include `package_sha256`.".to_string()
        })?;
    let receipt = PulledPackageHostedReceiptDocument {
        format_version: 1,
        source_kind: "hosted".to_string(),
        hosted_source_id: hosted_source_id.to_string(),
        hosted_version_id: hosted_version_id.to_string(),
        owner_handle: response
            .get("owner_handle")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        package_name: package_name.to_string(),
        resolved_version: resolved_version.to_string(),
        package_sha256: package_sha256.to_string(),
    };
    let receipt_path = project_root.join(PULLED_PACKAGE_HOSTED_RECEIPT_RELATIVE_PATH);
    if let Some(parent) = receipt_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create pulled package hosted receipt directory '{}': {}",
                parent.display(),
                error
            )
        })?;
    }
    let rendered = toml::to_string_pretty(&receipt)
        .map_err(|error| format!("Failed to render pulled package hosted receipt: {error}"))?;
    fs::write(&receipt_path, rendered).map_err(|error| {
        format!(
            "Failed to write pulled package hosted receipt '{}': {}",
            receipt_path.display(),
            error
        )
    })
}

fn resolve_pull_output_path(path: &Path, force: bool) -> Result<PathBuf, String> {
    let file_name = path.file_name().ok_or_else(|| {
        format!(
            "Output path '{}' must name a project directory and cannot be a filesystem root.",
            path.display()
        )
    })?;
    let raw_parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let absolute_parent = if raw_parent.is_absolute() {
        normalize_filesystem_path(raw_parent)
    } else {
        normalize_filesystem_path(
            std::env::current_dir()
                .map_err(|error| format!("Failed to resolve current directory: {error}"))?
                .join(raw_parent),
        )
    };
    inspect_real_directory_components(&absolute_parent)?;
    fs::create_dir_all(&absolute_parent).map_err(|error| {
        format!(
            "Failed to create pull output parent directory '{}': {}",
            absolute_parent.display(),
            error
        )
    })?;
    inspect_real_directory_components(&absolute_parent)?;
    let canonical_parent = fs::canonicalize(&absolute_parent).map_err(|error| {
        format!(
            "Failed to resolve pull output parent directory '{}': {}",
            absolute_parent.display(),
            error
        )
    })?;
    let resolved_path = canonical_parent.join(file_name);

    match fs::symlink_metadata(&resolved_path) {
        Ok(metadata) => {
            if archive_metadata_is_link_like(&metadata) {
                return Err(format!(
                    "Output path '{}' must not be a symbolic link or reparse point.",
                    resolved_path.display()
                ));
            }
            if !force {
                return Err(format!(
                    "Output directory '{}' already exists. Re-run with --force to replace it, or choose --output-dir <DIR>.",
                    path.display()
                ));
            }
            if !metadata.is_dir() && !metadata.is_file() {
                return Err(format!(
                    "Output path '{}' must be a regular file or directory before replacement.",
                    resolved_path.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Failed to inspect output path '{}': {}",
                resolved_path.display(),
                error
            ));
        }
    }
    Ok(resolved_path)
}

fn replace_output_with_staged(
    output_path: &Path,
    staging_path: &Path,
    backup_path: &Path,
    force: bool,
) -> Result<(), String> {
    let output_metadata = match fs::symlink_metadata(output_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            let _ = remove_path_without_following_links(staging_path);
            return Err(format!(
                "Failed to inspect output path '{}' before replacement: {}",
                output_path.display(),
                error
            ));
        }
    };

    let Some(output_metadata) = output_metadata else {
        return fs::rename(staging_path, output_path).map_err(|error| {
            let _ = remove_path_without_following_links(staging_path);
            format!(
                "Failed to move validated pull output from '{}' to '{}': {}",
                staging_path.display(),
                output_path.display(),
                error
            )
        });
    };
    if archive_metadata_is_link_like(&output_metadata) {
        let _ = remove_path_without_following_links(staging_path);
        return Err(format!(
            "Output path '{}' must not be a symbolic link or reparse point.",
            output_path.display()
        ));
    }
    if !force {
        let _ = remove_path_without_following_links(staging_path);
        return Err(format!(
            "Output directory '{}' already exists. Re-run with --force to replace it, or choose --output-dir <DIR>.",
            output_path.display()
        ));
    }
    if !output_metadata.is_dir() && !output_metadata.is_file() {
        let _ = remove_path_without_following_links(staging_path);
        return Err(format!(
            "Output path '{}' must be a regular file or directory before replacement.",
            output_path.display()
        ));
    }

    fs::rename(output_path, backup_path).map_err(|error| {
        let _ = remove_path_without_following_links(staging_path);
        format!(
            "Failed to create recoverable pull backup '{}' for '{}': {}",
            backup_path.display(),
            output_path.display(),
            error
        )
    })?;
    let replacement_result = maybe_fail_pull_activation().and_then(|_| {
        fs::rename(staging_path, output_path)
            .map_err(|error| format!("failed to rename staged output: {error}"))
    });
    if let Err(replacement_error) = replacement_result {
        return match fs::rename(backup_path, output_path) {
            Ok(()) => {
                let _ = remove_path_without_following_links(staging_path);
                Err(format!(
                    "Failed to activate validated pull output '{}': {} Previous output was restored.",
                    output_path.display(),
                    replacement_error
                ))
            }
            Err(recovery_error) => Err(format!(
                "Failed to activate validated pull output '{}': {} Automatic recovery failed: {}. Previous output remains at '{}'; staged output remains at '{}'.",
                output_path.display(),
                replacement_error,
                recovery_error,
                backup_path.display(),
                staging_path.display()
            )),
        };
    }

    if let Err(error) = remove_path_without_following_links(backup_path) {
        eprintln!(
            "Warning: pull output was replaced, but prior output backup '{}' could not be removed: {}",
            backup_path.display(),
            error
        );
    }
    Ok(())
}

fn maybe_fail_pull_activation() -> Result<(), String> {
    #[cfg(test)]
    if TEST_FAIL_PULL_ACTIVATION.with(|fail| fail.replace(false)) {
        return Err("Injected pull activation failure for testing.".to_string());
    }
    Ok(())
}

#[cfg(test)]
fn fail_next_pull_activation() {
    TEST_FAIL_PULL_ACTIVATION.with(|fail| fail.set(true));
}

fn remove_path_without_following_links(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Failed to inspect cleanup path '{}': {}",
                path.display(),
                error
            ));
        }
    };
    if metadata.is_dir() && !archive_metadata_is_link_like(&metadata) {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|error| format!("Failed to remove '{}': {}", path.display(), error))
}

fn inspect_real_directory_components(path: &Path) -> Result<(), String> {
    let trusted_boundaries = trusted_pull_output_boundaries();
    let trusted_boundary = trusted_boundaries
        .iter()
        .filter(|boundary| path.starts_with(boundary))
        .max_by_key(|boundary| boundary.components().count());
    let (mut current_path, remaining_path) = match trusted_boundary {
        Some(boundary) => {
            let canonical_boundary = fs::canonicalize(boundary).map_err(|error| {
                format!(
                    "Failed to resolve trusted pull output boundary '{}': {}",
                    boundary.display(),
                    error
                )
            })?;
            let remaining = path
                .strip_prefix(boundary)
                .map_err(|_| "Pull output escaped its trusted boundary.".to_string())?;
            (canonical_boundary, remaining)
        }
        None => (PathBuf::new(), path),
    };
    for component in remaining_path.components() {
        current_path.push(component.as_os_str());
        match fs::symlink_metadata(&current_path) {
            Ok(metadata) if archive_metadata_is_link_like(&metadata) => {
                return Err(format!(
                    "Output path must not traverse symbolic link or reparse point '{}'.",
                    current_path.display()
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!(
                    "Output path ancestor '{}' must be a real directory.",
                    current_path.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "Failed to inspect output path ancestor '{}': {}",
                    current_path.display(),
                    error
                ));
            }
        }
    }
    Ok(())
}

fn trusted_pull_output_boundaries() -> Vec<PathBuf> {
    let mut boundaries = vec![normalize_filesystem_path(std::env::temp_dir())];
    if let Ok(current_dir) = std::env::current_dir() {
        boundaries.push(normalize_filesystem_path(current_dir));
    }
    boundaries
        .into_iter()
        .filter(|boundary| boundary.is_absolute() && boundary.exists())
        .collect()
}

fn normalize_filesystem_path(path: impl AsRef<Path>) -> PathBuf {
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

fn build_local_pull_ui(
    owner_handle: Option<String>,
    project_name: &str,
    project_version: &str,
    output_path: &Path,
) -> Value {
    let mut source_items = vec![json!({"label": "Package", "value": project_name})];
    if let Some(owner_handle) = owner_handle {
        source_items.insert(0, json!({"label": "Owner", "value": owner_handle}));
    }

    json!({
        "schema": "1.0",
        "kind": "success",
        "icon": "✓",
        "title": "Package restored",
        "summary": format!("Restored `{project_name}` to `{}`.", display_path(output_path)),
        "sections": [
            {
                "type": "kv",
                "title": "Source",
                "title_style": "plain",
                "layout": "aligned",
                "items": source_items
            },
            {
                "type": "kv",
                "title": "Package",
                "title_style": "plain",
                "layout": "aligned",
                "items": [
                    {"label": "Version", "value": project_version}
                ]
            },
            {
                "type": "kv",
                "title": "Output",
                "title_style": "plain",
                "layout": "aligned",
                "items": [
                    {"label": "Directory", "value": format!("`{}`", display_path(output_path))}
                ]
            },
            {
                "type": "kv",
                "title": "Available commands",
                "title_style": "plain",
                "layout": "aligned",
                "items": [
                    {"label": "Build one tool", "value": "`cargo ai tools build <tool-name>`"},
                    {"label": "Build project", "value": "`cargo ai build`"},
                    {"label": "Package project", "value": "`cargo ai package`"}
                ]
            }
        ]
    })
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

pub(crate) fn create_package_archive_bytes(package_root: &Path) -> Result<Vec<u8>, String> {
    let canonical_package_root = validate_archive_source_root(package_root)?;
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive_builder = Builder::new(encoder);
    archive_builder.mode(HeaderMode::Deterministic);
    append_compressed_archive_entries(
        &mut archive_builder,
        package_root,
        &canonical_package_root,
        package_root,
    )?;
    let encoder = archive_builder
        .into_inner()
        .map_err(|error| format!("Failed to finalize compressed project archive: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("Failed to finish compressed project archive: {error}"))
}

fn append_compressed_archive_entries<W: Write>(
    archive_builder: &mut Builder<W>,
    package_root: &Path,
    canonical_package_root: &Path,
    current_path: &Path,
) -> Result<(), String> {
    let current_metadata =
        validate_archive_source_path(package_root, canonical_package_root, current_path)?;
    if !current_metadata.is_dir() {
        return Err(format!(
            "Packaged path '{}' must be a real directory.",
            current_path.display()
        ));
    }
    let mut children = fs::read_dir(current_path)
        .map_err(|error| {
            format!(
                "Failed to read package directory '{}' while building project archive: {}",
                current_path.display(),
                error
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            format!(
                "Failed to read package directory entry under '{}': {}",
                current_path.display(),
                error
            )
        })?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        let child_path = child.path();
        let child_metadata =
            validate_archive_source_path(package_root, canonical_package_root, &child_path)?;
        let relative_path = child_path
            .strip_prefix(package_root)
            .map_err(|_| {
                format!(
                    "Packaged path '{}' is not relative to the package root '{}'.",
                    child_path.display(),
                    package_root.display()
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");

        if relative_path == ".cargo-ai/publish-requests"
            || relative_path.starts_with(".cargo-ai/publish-requests/")
        {
            continue;
        }
        if child_metadata.is_dir() {
            archive_builder
                .append_dir(relative_path.as_str(), child_path.as_path())
                .map_err(|error| {
                    format!(
                        "Failed to append packaged directory '{}' to the compressed project archive: {}",
                        child_path.display(),
                        error
                    )
                })?;
            append_compressed_archive_entries(
                archive_builder,
                package_root,
                canonical_package_root,
                child_path.as_path(),
            )?;
        } else if child_metadata.is_file() {
            let mut file = File::open(child_path.as_path()).map_err(|error| {
                format!(
                    "Failed to read packaged file '{}' while building project archive: {}",
                    child_path.display(),
                    error
                )
            })?;
            archive_builder
                .append_file(relative_path.as_str(), &mut file)
                .map_err(|error| {
                    format!(
                        "Failed to append packaged file '{}' to the compressed project archive: {}",
                        child_path.display(),
                        error
                    )
                })?;
        } else {
            return Err(format!(
                "Packaged path '{}' must be a regular file or directory.",
                child_path.display()
            ));
        }
    }

    Ok(())
}

pub(crate) fn extract_package_archive_bytes(
    archive_bytes: &[u8],
    output_root: &Path,
) -> Result<(), String> {
    extract_package_archive_bytes_with_limits(archive_bytes, output_root, HOSTED_ARCHIVE_LIMITS)
}

pub(crate) fn validate_package_archive_size(compressed_bytes: usize) -> Result<(), String> {
    validate_archive_compressed_size(compressed_bytes, HOSTED_ARCHIVE_LIMITS)
}

pub(crate) fn decode_package_archive_base64(archive_base64: &str) -> Result<Vec<u8>, String> {
    validate_archive_base64_size(archive_base64.len(), HOSTED_ARCHIVE_LIMITS)?;
    let archive_bytes = BASE64_STANDARD
        .decode(archive_base64.as_bytes())
        .map_err(|error| format!("Failed to decode package archive: {error}"))?;
    validate_package_archive_size(archive_bytes.len())?;
    Ok(archive_bytes)
}

fn extract_package_archive_bytes_with_limits(
    archive_bytes: &[u8],
    output_root: &Path,
    limits: ArchiveLimits,
) -> Result<(), String> {
    validate_archive_compressed_size(archive_bytes.len(), limits)?;
    if archive_bytes.starts_with(&[0x1f, 0x8b]) {
        extract_compressed_package_archive_bytes(archive_bytes, output_root, limits)
    } else {
        extract_legacy_package_archive_bytes(archive_bytes, output_root, limits)
    }
}

fn extract_compressed_package_archive_bytes(
    archive_bytes: &[u8],
    output_root: &Path,
    limits: ArchiveLimits,
) -> Result<(), String> {
    let decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("Failed to read compressed project archive entries: {error}"))?;
    let mut entry_count = 0_usize;
    let mut expanded_bytes = 0_u64;

    for entry in entries {
        let mut entry = entry
            .map_err(|error| format!("Failed to read compressed project archive entry: {error}"))?;
        let entry_path = entry.path().map_err(|error| {
            format!("Failed to read compressed project archive entry path: {error}")
        })?;
        let relative_path = entry_path.to_string_lossy().replace('\\', "/");
        let relative_path = validate_relative_archive_path(relative_path.as_str())?;
        let relative_path_text = relative_path.to_string_lossy().replace('\\', "/");
        let entry_size = match entry.header().entry_type() {
            EntryType::Directory => 0,
            EntryType::Regular => entry.size(),
            other => {
                return Err(format!(
                    "Compressed archive entry '{}' has unsupported kind '{:?}'.",
                    relative_path_text, other
                ));
            }
        };
        update_archive_budget(
            &mut entry_count,
            &mut expanded_bytes,
            relative_path_text.as_str(),
            entry_size,
            limits,
        )?;
        let target_path = resolve_archive_target_path(output_root, relative_path.as_path())?;
        match entry.header().entry_type() {
            EntryType::Directory => {
                fs::create_dir_all(&target_path).map_err(|error| {
                    format!(
                        "Failed to create restored directory '{}': {}",
                        target_path.display(),
                        error
                    )
                })?;
                ensure_archive_target_is_safe(output_root, relative_path.as_path())?;
            }
            EntryType::Regular => {
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!(
                            "Failed to create restored parent directory '{}': {}",
                            parent.display(),
                            error
                        )
                    })?;
                }
                ensure_archive_target_parent_is_safe(output_root, relative_path.as_path())?;
                entry.unpack(&target_path).map_err(|error| {
                    format!(
                        "Failed to write restored file '{}': {}",
                        target_path.display(),
                        error
                    )
                })?;
                ensure_archive_target_is_safe(output_root, relative_path.as_path())?;
            }
            _ => unreachable!("unsupported archive entry kinds rejected before extraction"),
        }
    }

    Ok(())
}

fn extract_legacy_package_archive_bytes(
    archive_bytes: &[u8],
    output_root: &Path,
    limits: ArchiveLimits,
) -> Result<(), String> {
    let archive: PackageArchiveDocument = serde_json::from_slice(archive_bytes)
        .map_err(|error| format!("Failed to parse project package archive: {error}"))?;
    if archive.format_version != 1 {
        return Err(format!(
            "Unsupported project package archive format version '{}'.",
            archive.format_version
        ));
    }

    let mut entry_count = 0_usize;
    let mut expanded_bytes = 0_u64;
    for entry in archive.entries {
        let relative_path = validate_relative_archive_path(entry.path.as_str())?;
        let relative_path_text = relative_path.to_string_lossy().replace('\\', "/");
        match entry.kind.as_str() {
            "dir" => {
                update_archive_budget(
                    &mut entry_count,
                    &mut expanded_bytes,
                    relative_path_text.as_str(),
                    0,
                    limits,
                )?;
                let target_path =
                    resolve_archive_target_path(output_root, relative_path.as_path())?;
                fs::create_dir_all(&target_path).map_err(|error| {
                    format!(
                        "Failed to create restored directory '{}': {}",
                        target_path.display(),
                        error
                    )
                })?;
                ensure_archive_target_is_safe(output_root, relative_path.as_path())?;
            }
            "file" => {
                let encoded = entry.contents_base64.ok_or_else(|| {
                    format!("Archive entry '{}' is missing file contents.", entry.path)
                })?;
                let decoded = BASE64_STANDARD
                    .decode(encoded.as_bytes())
                    .map_err(|error| {
                        format!(
                            "Failed to decode file contents for archive entry '{}': {}",
                            entry.path, error
                        )
                    })?;
                update_archive_budget(
                    &mut entry_count,
                    &mut expanded_bytes,
                    relative_path_text.as_str(),
                    u64::try_from(decoded.len()).map_err(|_| {
                        "Legacy archive entry exceeded supported size limits.".to_string()
                    })?,
                    limits,
                )?;
                let target_path =
                    resolve_archive_target_path(output_root, relative_path.as_path())?;
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!(
                            "Failed to create restored parent directory '{}': {}",
                            parent.display(),
                            error
                        )
                    })?;
                }
                ensure_archive_target_parent_is_safe(output_root, relative_path.as_path())?;
                fs::write(&target_path, decoded).map_err(|error| {
                    format!(
                        "Failed to write restored file '{}': {}",
                        target_path.display(),
                        error
                    )
                })?;
                ensure_archive_target_is_safe(output_root, relative_path.as_path())?;
            }
            other => {
                return Err(format!(
                    "Archive entry '{}' has unsupported kind '{}'.",
                    entry.path, other
                ));
            }
        }
    }

    Ok(())
}

pub(crate) fn directory_size_bytes(root: &Path) -> Result<u64, String> {
    let canonical_root = validate_archive_source_root(root)?;
    directory_size_bytes_under_root(root, &canonical_root, root)
}

fn directory_size_bytes_under_root(
    root: &Path,
    canonical_root: &Path,
    current_path: &Path,
) -> Result<u64, String> {
    let metadata = validate_archive_source_path(root, canonical_root, current_path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "Packaged path '{}' must be a regular file or directory.",
            current_path.display()
        ));
    }

    let mut total = 0_u64;
    let entries = fs::read_dir(current_path).map_err(|error| {
        format!(
            "Failed to read packaged directory '{}' while measuring size: {}",
            current_path.display(),
            error
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to read packaged directory entry under '{}' while measuring size: {}",
                current_path.display(),
                error
            )
        })?;
        total = total
            .checked_add(directory_size_bytes_under_root(
                root,
                canonical_root,
                entry.path().as_path(),
            )?)
            .ok_or_else(|| "Packaged directory size exceeded supported limits.".to_string())?;
    }
    Ok(total)
}

fn validate_archive_source_root(package_root: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(package_root).map_err(|error| {
        format!(
            "Failed to inspect package root '{}' while building project archive: {}",
            package_root.display(),
            error
        )
    })?;
    if archive_metadata_is_link_like(&metadata) || !metadata.is_dir() {
        return Err(format!(
            "Package root '{}' must be a real directory and not a symbolic link or reparse point.",
            package_root.display()
        ));
    }
    fs::canonicalize(package_root).map_err(|error| {
        format!(
            "Failed to resolve package root '{}' while building project archive: {}",
            package_root.display(),
            error
        )
    })
}

fn validate_archive_source_path(
    package_root: &Path,
    canonical_package_root: &Path,
    source_path: &Path,
) -> Result<fs::Metadata, String> {
    let relative_path = source_path.strip_prefix(package_root).map_err(|_| {
        format!(
            "Packaged path '{}' is not inside package root '{}'.",
            source_path.display(),
            package_root.display()
        )
    })?;
    let mut current_path = package_root.to_path_buf();
    let mut metadata = fs::symlink_metadata(package_root).map_err(|error| {
        format!(
            "Failed to inspect package root '{}': {}",
            package_root.display(),
            error
        )
    })?;
    for component in relative_path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(segment) => current_path.push(segment),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("Packaged path escaped the package root.".to_string());
            }
        }
        metadata = fs::symlink_metadata(&current_path).map_err(|error| {
            format!(
                "Failed to inspect packaged path '{}': {}",
                current_path.display(),
                error
            )
        })?;
        if archive_metadata_is_link_like(&metadata) {
            return Err(format!(
                "Packaged path must not traverse symbolic link or reparse point '{}'.",
                current_path.display()
            ));
        }
    }
    let canonical_source_path = fs::canonicalize(source_path).map_err(|error| {
        format!(
            "Failed to resolve packaged path '{}': {}",
            source_path.display(),
            error
        )
    })?;
    if !canonical_source_path.starts_with(canonical_package_root) {
        return Err(format!(
            "Packaged path '{}' resolves outside package root '{}'.",
            source_path.display(),
            package_root.display()
        ));
    }
    Ok(metadata)
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes_f64 = bytes as f64;
    if bytes_f64 >= GIB {
        format!("{:.1} GiB", bytes_f64 / GIB)
    } else if bytes_f64 >= MIB {
        format!("{:.1} MiB", bytes_f64 / MIB)
    } else if bytes_f64 >= KIB {
        format!("{:.1} KiB", bytes_f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn validate_archive_compressed_size(
    compressed_bytes: usize,
    limits: ArchiveLimits,
) -> Result<(), String> {
    if compressed_bytes > limits.compressed_bytes {
        return Err(format!(
            "Package archive is {} bytes after decoding; the client limit is {} bytes.",
            compressed_bytes, limits.compressed_bytes
        ));
    }
    Ok(())
}

fn validate_archive_base64_size(encoded_bytes: usize, limits: ArchiveLimits) -> Result<(), String> {
    let max_encoded_bytes = limits
        .compressed_bytes
        .checked_add(2)
        .and_then(|value| value.checked_div(3))
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| "Package archive size limit overflowed supported bounds.".to_string())?;
    if encoded_bytes > max_encoded_bytes {
        return Err(format!(
            "Package archive base64 payload exceeds the {}-byte client limit.",
            max_encoded_bytes
        ));
    }
    Ok(())
}

fn update_archive_budget(
    entry_count: &mut usize,
    expanded_bytes: &mut u64,
    relative_path: &str,
    entry_size: u64,
    limits: ArchiveLimits,
) -> Result<(), String> {
    if relative_path.len() > limits.path_bytes {
        return Err(format!(
            "Archive entry path exceeds the {}-byte client limit.",
            limits.path_bytes
        ));
    }
    *entry_count = entry_count
        .checked_add(1)
        .ok_or_else(|| "Archive entry count exceeded supported limits.".to_string())?;
    if *entry_count > limits.entries {
        return Err(format!(
            "Package archive contains more than {} entries.",
            limits.entries
        ));
    }
    *expanded_bytes = expanded_bytes
        .checked_add(entry_size)
        .ok_or_else(|| "Expanded package archive size exceeded supported limits.".to_string())?;
    if *expanded_bytes > limits.expanded_bytes {
        return Err(format!(
            "Expanded package archive exceeds the {}-byte client limit.",
            limits.expanded_bytes
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn archive_metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    archive_windows_attributes_are_link_like(metadata.file_attributes())
}

#[cfg(windows)]
fn archive_windows_attributes_are_link_like(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn archive_metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn resolve_archive_target_path(
    output_root: &Path,
    relative_path: &Path,
) -> Result<PathBuf, String> {
    ensure_archive_target_parent_is_safe(output_root, relative_path)?;
    Ok(output_root.join(relative_path))
}

fn ensure_archive_target_parent_is_safe(
    output_root: &Path,
    relative_path: &Path,
) -> Result<(), String> {
    let parent = relative_path.parent().unwrap_or_else(|| Path::new(""));
    ensure_archive_path_is_safe(output_root, parent)
}

fn ensure_archive_target_is_safe(output_root: &Path, relative_path: &Path) -> Result<(), String> {
    ensure_archive_path_is_safe(output_root, relative_path)
}

fn ensure_archive_path_is_safe(output_root: &Path, relative_path: &Path) -> Result<(), String> {
    let root_metadata = fs::symlink_metadata(output_root).map_err(|error| {
        format!(
            "Failed to inspect archive output root '{}': {}",
            output_root.display(),
            error
        )
    })?;
    if archive_metadata_is_link_like(&root_metadata) || !root_metadata.is_dir() {
        return Err(format!(
            "Archive output root '{}' must be a real directory and not a symbolic link or reparse point.",
            output_root.display()
        ));
    }

    let mut current = output_root.to_path_buf();
    for component in relative_path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(segment) => current.push(segment),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("Archive target path escaped the output root.".to_string());
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if archive_metadata_is_link_like(&metadata) => {
                return Err(format!(
                    "Archive target path must not traverse symbolic link or reparse point '{}'.",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "Failed to inspect archive target component '{}': {}",
                    current.display(),
                    error
                ));
            }
        }
    }
    Ok(())
}

fn validate_relative_archive_path(raw_path: &str) -> Result<PathBuf, String> {
    crate::commands::local_packages::normalize_portable_relative_path(
        raw_path,
        "Archive entry path",
    )
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    let hash = digest.finalize();
    let mut rendered = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(&mut rendered, "{byte:02x}");
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::{
        create_package_archive_bytes, extract_compressed_package_archive_bytes,
        extract_legacy_package_archive_bytes, extract_package_archive_bytes,
        extract_package_archive_bytes_with_limits, relocate_pulled_package_receipt,
        restore_pulled_project, sha256_hex, validate_archive_base64_size,
        validate_project_pull_response_matches_request, validate_relative_archive_path,
        validate_restored_package_manifest, ArchiveLimits, PackageArchiveDocument,
        PackageArchiveEntry, PulledPackageHostedReceiptDocument,
    };
    #[cfg(feature = "developer-tools")]
    use super::{finish_publish_payload, PublishStagingGuard};
    #[cfg(feature = "developer-tools")]
    use crate::commands::package::AssembledPackage;
    use base64::Engine as _;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(stem: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cargo-ai-projects-command-test-{stem}-{nanos}"))
    }

    #[cfg(feature = "developer-tools")]
    #[tokio::test]
    async fn publication_transport_completes_success_and_preserves_retry_identity() {
        use std::io::{Read, Write};
        for scenario in ["initial", "refreshed", "lost"] {
            let root = temp_dir(scenario);
            fs::create_dir_all(&root).unwrap();
            let digest = "a".repeat(64);
            let id = crate::commands::package_publication::request_id(
                &root, "demo", "1.0.0", &digest, None,
            )
            .unwrap();
            let payload = super::PublishPayload {
                receipt_root: root.clone(),
                request_id: id.clone(),
                hosted_source_id: None,
                project_name: "demo".into(),
                project_version: "1.0.0".into(),
                package_manifest: serde_json::json!({}),
                package_sha256: digest.clone(),
                package_size_bytes: 1,
                package_archive_base64: "YQ==".into(),
            };
            let success = serde_json::json!({"status":"success", "hosted_source_id":"source-id",
                "hosted_version_id":"version-id", "project_version":"1.0.0", "package_sha256":digest});
            let responses = match scenario {
                "refreshed" => vec![
                    Some(serde_json::json!({"status":"error", "type":"access_token_expired"})),
                    Some(success),
                ],
                "lost" => vec![None, Some(success)],
                _ => vec![Some(success)],
            };
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let mut received = Vec::new();
                for response in responses {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut raw = Vec::new();
                    let (start, length) = loop {
                        let mut byte = [0];
                        socket.read_exact(&mut byte).unwrap();
                        raw.push(byte[0]);
                        if raw.ends_with(b"\r\n\r\n") {
                            let headers = String::from_utf8(raw.clone()).unwrap();
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                            break (raw.len(), length);
                        }
                    };
                    raw.resize(start + length, 0);
                    socket.read_exact(&mut raw[start..]).unwrap();
                    received
                        .push(serde_json::from_slice::<serde_json::Value>(&raw[start..]).unwrap());
                    if let Some(value) = response {
                        let body = serde_json::to_string(&value).unwrap();
                        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                    }
                }
                received
            });
            let first =
                super::transmit_publish_payload(&url, "isolated-first-token", &payload).await;
            let terminal = root
                .join(".cargo-ai/publish-requests")
                .join(format!("{id}.completed.json"));
            if scenario != "initial" {
                if scenario == "lost" {
                    assert!(first.is_err());
                } else {
                    assert_eq!(first.unwrap()["type"], "access_token_expired");
                }
                assert!(!terminal.exists());
                assert_eq!(
                    crate::commands::package_publication::request_id(
                        &root, "demo", "1.0.0", &digest, None
                    )
                    .unwrap(),
                    id
                );
                super::transmit_publish_payload(&url, "isolated-retry-token", &payload)
                    .await
                    .unwrap();
            } else {
                assert_eq!(first.unwrap()["status"], "success");
            }
            assert!(terminal.is_file());
            assert_ne!(
                crate::commands::package_publication::request_id(
                    &root, "demo", "1.0.0", &digest, None
                )
                .unwrap(),
                id
            );
            let requests = server.join().unwrap();
            for request in requests {
                assert_eq!(request["projects"]["publish"]["publish_request_id"], id);
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(feature = "developer-tools")]
    #[test]
    fn publish_staging_is_removed_after_post_assembly_validation_errors() {
        for (stem, project_name, project_version, expected_error) in [
            (
                "publish-cleanup-missing-name",
                None,
                Some("1.0.0"),
                "[project].name",
            ),
            (
                "publish-cleanup-invalid-version",
                Some("demo"),
                Some("not-semver"),
                "not valid semver",
            ),
        ] {
            let staging_root = temp_dir(stem);
            fs::create_dir_all(&staging_root).expect("staging root should exist");
            fs::write(staging_root.join("sentinel.txt"), "temporary")
                .expect("staging sentinel should be writable");
            let assembled = AssembledPackage {
                root_path: staging_root.clone(),
                manifest_project_name: project_name.map(str::to_string),
                manifest_project_version: project_version.map(str::to_string),
                manifest_value: serde_json::json!({"format_version": 1}),
                archive_bytes: b"archive".to_vec(),
                assembled_size_bytes: 7,
                archive_size_bytes: 7,
                estimated_publish_request_size_bytes: 7,
            };

            let error =
                finish_publish_payload(assembled, PublishStagingGuard::new(staging_root.clone()))
                    .expect_err("invalid post-assembly metadata should reject publish");

            assert!(error.contains(expected_error));
            assert!(
                !staging_root.exists(),
                "publish staging must be removed on every validation error"
            );
        }
    }

    #[test]
    fn archive_round_trip_preserves_directory_structure() {
        let source_root = temp_dir("archive-source");
        let dest_root = temp_dir("archive-dest");
        fs::create_dir_all(source_root.join(".cargo-ai"))
            .expect("source metadata dir should be created");
        fs::create_dir_all(source_root.join("assets"))
            .expect("source assets dir should be created");
        fs::write(
            source_root.join(".cargo-ai/project.toml"),
            "format_version = 1\n",
        )
        .expect("project metadata should be written");
        fs::write(
            source_root.join("cargo-ai-package.toml"),
            "format_version = 1\n",
        )
        .expect("package manifest should be written");
        fs::write(source_root.join("assets/demo.txt"), "hello").expect("asset should be written");

        let archive_bytes =
            create_package_archive_bytes(source_root.as_path()).expect("archive should serialize");

        fs::create_dir_all(&dest_root).expect("dest root should be created");
        extract_package_archive_bytes(archive_bytes.as_slice(), dest_root.as_path())
            .expect("archive should restore");
        relocate_pulled_package_receipt(dest_root.as_path())
            .expect("receipt should move into pulled-project origin metadata");

        assert_eq!(
            fs::read_to_string(dest_root.join(".cargo-ai/project.toml"))
                .expect("restored project metadata should be readable"),
            "format_version = 1\n"
        );
        assert_eq!(
            fs::read_to_string(dest_root.join(".cargo-ai/origin/cargo-ai-package.toml"))
                .expect("restored receipt should be readable"),
            "format_version = 1\n"
        );
        assert!(
            !dest_root.join("cargo-ai-package.toml").exists(),
            "root-level package receipt should be moved into origin metadata"
        );
        assert_eq!(
            fs::read_to_string(dest_root.join("assets/demo.txt"))
                .expect("restored asset should be readable"),
            "hello"
        );

        let _ = fs::remove_dir_all(source_root);
        let _ = fs::remove_dir_all(dest_root);
    }

    fn hosted_pull_response(
        archive_bytes: &[u8],
        project_name: &str,
        project_version: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "project": project_name,
            "project_version": project_version,
            "hosted_source_id": "source-id",
            "hosted_version_id": "version-id",
            "package_sha256": sha256_hex(archive_bytes),
            "package_size_bytes": archive_bytes.len(),
            "package_manifest": {"format_version": 1, "project_name": project_name, "project_version": project_version},
            "package_archive_base64": base64::engine::general_purpose::STANDARD.encode(archive_bytes)
        })
    }

    fn write_pull_source(root: &std::path::Path, project_name: &str) {
        fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata root should exist");
        fs::create_dir_all(root.join("assets")).expect("asset root should exist");
        fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("project metadata should be writable");
        fs::write(
            root.join("cargo-ai-package.toml"),
            format!(
                "format_version = 1\nproject_name = \"{project_name}\"\nproject_version = \"1.0.0\"\n"
            ),
        )
        .expect("package manifest should be writable");
        fs::write(root.join("assets/new.txt"), "new").expect("package asset should be writable");
    }

    #[test]
    fn forced_pull_rejects_matching_identity_with_changed_embedded_declarations() {
        let source = temp_dir("manifest-binding-source");
        let output = temp_dir("manifest-binding-output");
        write_pull_source(&source, "demo");
        let archive = create_package_archive_bytes(&source).unwrap();
        let mut response = hosted_pull_response(&archive, "demo", "1.0.0");
        response["package_manifest"]["permissions"] = serde_json::json!({"subprocess":"allowed"});
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("sentinel"), "preserve").unwrap();
        let error = restore_pulled_project(&response, &output, true).unwrap_err();
        assert!(error.contains("archive manifest differs"), "{error}");
        assert_eq!(
            fs::read_to_string(output.join("sentinel")).unwrap(),
            "preserve"
        );
        assert!(!output.join("assets/new.txt").exists());
        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(output).unwrap();
    }

    #[test]
    fn forced_pull_validates_staging_before_replacing_existing_output() {
        let source_root = temp_dir("transaction-source-invalid");
        let output_root = temp_dir("transaction-output-invalid");
        write_pull_source(&source_root, "actual");
        let archive =
            create_package_archive_bytes(&source_root).expect("fixture archive should serialize");
        fs::create_dir_all(&output_root).expect("existing output should exist");
        fs::write(output_root.join("sentinel.txt"), "old")
            .expect("existing sentinel should be writable");
        let response = hosted_pull_response(&archive, "different", "1.0.0");

        let error = restore_pulled_project(&response, &output_root, true)
            .expect_err("manifest mismatch should reject staged pull");
        assert!(error.contains("did not match hosted response identity"));
        assert_eq!(
            fs::read_to_string(output_root.join("sentinel.txt"))
                .expect("existing output should be preserved"),
            "old"
        );
        assert!(!output_root.join("assets/new.txt").exists());

        let _ = fs::remove_dir_all(source_root);
        let _ = fs::remove_dir_all(output_root);
    }

    #[test]
    fn forced_pull_activates_only_the_fully_validated_staged_output() {
        let source_root = temp_dir("transaction-source-valid");
        let output_root = temp_dir("transaction-output-valid");
        write_pull_source(&source_root, "demo");
        let archive =
            create_package_archive_bytes(&source_root).expect("fixture archive should serialize");
        fs::create_dir_all(&output_root).expect("existing output should exist");
        fs::write(output_root.join("sentinel.txt"), "old")
            .expect("existing sentinel should be writable");
        let response = hosted_pull_response(&archive, "demo", "1.0.0");

        restore_pulled_project(&response, &output_root, true)
            .expect("validated staged pull should replace existing output");
        assert!(!output_root.join("sentinel.txt").exists());
        assert_eq!(
            fs::read_to_string(output_root.join("assets/new.txt"))
                .expect("restored asset should be readable"),
            "new"
        );
        assert!(output_root
            .join(".cargo-ai/origin/cargo-ai-package.toml")
            .exists());
        assert!(output_root
            .join(".cargo-ai/origin/cargo-ai-package-receipt.toml")
            .exists());

        let _ = fs::remove_dir_all(source_root);
        let _ = fs::remove_dir_all(output_root);
    }

    #[test]
    fn forced_pull_restores_existing_output_after_late_activation_failure() {
        let transaction_root = temp_dir("transaction-late-failure");
        let source_root = transaction_root.join("source");
        let output_root = transaction_root.join("output");
        write_pull_source(&source_root, "demo");
        let archive =
            create_package_archive_bytes(&source_root).expect("fixture archive should serialize");
        fs::create_dir_all(&output_root).expect("existing output should exist");
        fs::write(output_root.join("sentinel.txt"), "old")
            .expect("existing sentinel should be writable");
        let response = hosted_pull_response(&archive, "demo", "1.0.0");
        super::fail_next_pull_activation();

        let error = restore_pulled_project(&response, &output_root, true)
            .expect_err("injected late activation failure should reject pull");
        assert!(error.contains("Previous output was restored"));
        assert_eq!(
            fs::read_to_string(output_root.join("sentinel.txt"))
                .expect("previous output should be restored"),
            "old"
        );
        assert!(!output_root.join("assets/new.txt").exists());
        let retained_transaction_paths = fs::read_dir(&transaction_root)
            .expect("transaction parent should remain readable")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cargo-ai-pull-")
            })
            .count();
        assert_eq!(
            retained_transaction_paths, 0,
            "successful recovery should clean staging and backup paths"
        );

        let _ = fs::remove_dir_all(transaction_root);
    }

    #[test]
    fn sha256_hex_renders_expected_length() {
        let rendered = sha256_hex(b"hello");
        assert_eq!(rendered.len(), 64);
        assert_eq!(
            rendered,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn legacy_json_archive_still_restores_successfully() {
        let source_root = temp_dir("legacy-archive-source");
        let dest_root = temp_dir("legacy-archive-dest");
        fs::create_dir_all(source_root.join(".cargo-ai"))
            .expect("source metadata dir should be created");
        fs::write(
            source_root.join(".cargo-ai/project.toml"),
            "format_version = 1\n",
        )
        .expect("project metadata should be written");
        fs::write(
            source_root.join("cargo-ai-package.toml"),
            "format_version = 1\n",
        )
        .expect("package manifest should be written");
        let legacy_archive = PackageArchiveDocument {
            format_version: 1,
            entries: vec![
                super::PackageArchiveEntry {
                    path: ".cargo-ai".to_string(),
                    kind: "dir".to_string(),
                    contents_base64: None,
                },
                super::PackageArchiveEntry {
                    path: ".cargo-ai/project.toml".to_string(),
                    kind: "file".to_string(),
                    contents_base64: Some(
                        base64::engine::general_purpose::STANDARD
                            .encode("format_version = 1\n".as_bytes()),
                    ),
                },
                super::PackageArchiveEntry {
                    path: "cargo-ai-package.toml".to_string(),
                    kind: "file".to_string(),
                    contents_base64: Some(
                        base64::engine::general_purpose::STANDARD
                            .encode("format_version = 1\n".as_bytes()),
                    ),
                },
            ],
        };
        let archive_bytes =
            serde_json::to_vec(&legacy_archive).expect("legacy archive should serialize");

        fs::create_dir_all(&dest_root).expect("dest root should be created");
        extract_package_archive_bytes(archive_bytes.as_slice(), dest_root.as_path())
            .expect("legacy archive should restore");
        relocate_pulled_package_receipt(dest_root.as_path())
            .expect("receipt should move into pulled-project origin metadata");

        assert!(dest_root.join(".cargo-ai/project.toml").exists());
        assert!(dest_root
            .join(".cargo-ai/origin/cargo-ai-package.toml")
            .exists());

        let _ = fs::remove_dir_all(source_root);
        let _ = fs::remove_dir_all(dest_root);
    }

    #[test]
    fn compressed_archive_limits_entry_count_and_expanded_bytes() {
        let source_root = temp_dir("compressed-limits-source");
        let dest_root = temp_dir("compressed-limits-dest");
        fs::create_dir_all(&source_root).expect("source root should be created");
        fs::create_dir_all(&dest_root).expect("dest root should be created");
        fs::write(source_root.join("one.txt"), "1234").expect("first file should be written");
        fs::write(source_root.join("two.txt"), "5678").expect("second file should be written");
        let archive =
            create_package_archive_bytes(source_root.as_path()).expect("archive should serialize");

        let entry_error = extract_compressed_package_archive_bytes(
            archive.as_slice(),
            dest_root.as_path(),
            ArchiveLimits {
                compressed_bytes: archive.len(),
                expanded_bytes: 100,
                entries: 1,
                path_bytes: 1_024,
            },
        )
        .expect_err("compressed entry count should be limited");
        assert!(entry_error.contains("more than 1 entries"));

        let expanded_dest = temp_dir("compressed-expanded-dest");
        fs::create_dir_all(&expanded_dest).expect("expanded dest should be created");
        let expanded_error = extract_compressed_package_archive_bytes(
            archive.as_slice(),
            expanded_dest.as_path(),
            ArchiveLimits {
                compressed_bytes: archive.len(),
                expanded_bytes: 3,
                entries: 10,
                path_bytes: 1_024,
            },
        )
        .expect_err("compressed expanded bytes should be limited");
        assert!(expanded_error.contains("Expanded package archive exceeds"));

        let _ = fs::remove_dir_all(source_root);
        let _ = fs::remove_dir_all(dest_root);
        let _ = fs::remove_dir_all(expanded_dest);
    }

    #[test]
    fn legacy_archive_limits_entry_count_and_expanded_bytes() {
        let archive = PackageArchiveDocument {
            format_version: 1,
            entries: vec![
                PackageArchiveEntry {
                    path: "one.txt".to_string(),
                    kind: "file".to_string(),
                    contents_base64: Some(
                        base64::engine::general_purpose::STANDARD.encode(b"1234"),
                    ),
                },
                PackageArchiveEntry {
                    path: "two.txt".to_string(),
                    kind: "file".to_string(),
                    contents_base64: Some(
                        base64::engine::general_purpose::STANDARD.encode(b"5678"),
                    ),
                },
            ],
        };
        let archive = serde_json::to_vec(&archive).expect("legacy archive should serialize");
        let entry_dest = temp_dir("legacy-entry-limit");
        fs::create_dir_all(&entry_dest).expect("entry dest should be created");
        let entry_error = extract_legacy_package_archive_bytes(
            archive.as_slice(),
            entry_dest.as_path(),
            ArchiveLimits {
                compressed_bytes: archive.len(),
                expanded_bytes: 100,
                entries: 1,
                path_bytes: 1_024,
            },
        )
        .expect_err("legacy entry count should be limited");
        assert!(entry_error.contains("more than 1 entries"));

        let expanded_dest = temp_dir("legacy-expanded-limit");
        fs::create_dir_all(&expanded_dest).expect("expanded dest should be created");
        let expanded_error = extract_legacy_package_archive_bytes(
            archive.as_slice(),
            expanded_dest.as_path(),
            ArchiveLimits {
                compressed_bytes: archive.len(),
                expanded_bytes: 3,
                entries: 10,
                path_bytes: 1_024,
            },
        )
        .expect_err("legacy expanded bytes should be limited");
        assert!(expanded_error.contains("Expanded package archive exceeds"));

        let _ = fs::remove_dir_all(entry_dest);
        let _ = fs::remove_dir_all(expanded_dest);
    }

    #[test]
    fn archive_paths_reject_windows_prefixes_on_every_platform() {
        for candidate in [
            "C:relative.txt",
            "C:\\absolute.txt",
            "\\\\server\\share\\file.txt",
            "\\\\?\\C:\\file.txt",
        ] {
            let error = validate_relative_archive_path(candidate)
                .expect_err("Windows-prefixed path should be rejected");
            assert!(
                error.contains("drive-relative")
                    || error.contains("device-root")
                    || error.contains("prefix"),
                "unexpected error for {candidate}: {error}"
            );
        }
    }

    #[test]
    fn archive_limits_decoded_size_and_normalized_path_length() {
        let error = extract_package_archive_bytes_with_limits(
            b"12345",
            PathBuf::from("unused").as_path(),
            ArchiveLimits {
                compressed_bytes: 4,
                expanded_bytes: 100,
                entries: 10,
                path_bytes: 1_024,
            },
        )
        .expect_err("decoded archive size should be checked before extraction");
        assert!(error.contains("after decoding"));

        let long_path = format!("{}.txt", "a".repeat(1_025));
        let error = validate_relative_archive_path(long_path.as_str())
            .expect_err("overlong normalized path should be rejected");
        assert!(error.contains("1024-byte path limit"));
    }

    #[test]
    fn archive_base64_size_is_bounded_before_decode() {
        let limits = ArchiveLimits {
            compressed_bytes: 4,
            expanded_bytes: 100,
            entries: 10,
            path_bytes: 1_024,
        };
        validate_archive_base64_size(8, limits).expect("base64 for four decoded bytes should fit");
        let error = validate_archive_base64_size(9, limits)
            .expect_err("oversized base64 should be rejected before decode");
        assert!(error.contains("base64 payload exceeds"));
    }

    #[test]
    fn hosted_pull_response_must_match_requested_identity_and_version() {
        let response = serde_json::json!({
            "project": "demo",
            "project_version": "1.2.3",
            "owner_handle": "alice"
        });
        validate_project_pull_response_matches_request(
            &response,
            "demo",
            Some("Alice"),
            Some("1.2.3"),
        )
        .expect("matching hosted response should pass");

        let package_error =
            validate_project_pull_response_matches_request(&response, "other", None, None)
                .expect_err("wrong package identity should fail");
        assert!(package_error.contains("requested package `other`"));
        let version_error =
            validate_project_pull_response_matches_request(&response, "demo", None, Some("1.2.2"))
                .expect_err("wrong exact version should fail");
        assert!(version_error.contains("exact requested version 1.2.2"));

        let owner_error =
            validate_project_pull_response_matches_request(&response, "demo", Some("bob"), None)
                .expect_err("wrong explicit owner should fail");
        assert!(owner_error.contains("requested owner `bob`"));

        let missing_owner = serde_json::json!({
            "project": "demo",
            "project_version": "1.2.3"
        });
        let missing_owner_error = validate_project_pull_response_matches_request(
            &missing_owner,
            "demo",
            Some("alice"),
            None,
        )
        .expect_err("missing explicit owner provenance should fail");
        assert!(missing_owner_error.contains("did not include `owner_handle`"));
    }

    #[test]
    fn restored_manifest_must_match_hosted_response_identity() {
        let root = temp_dir("restored-manifest-identity");
        fs::create_dir_all(&root).expect("restored root should be writable");
        fs::write(
            root.join("cargo-ai-package.toml"),
            r#"format_version = 1
project_name = "demo"
project_version = "1.2.3"
"#,
        )
        .expect("restored manifest should be writable");
        let matching = serde_json::json!({
            "project": "demo",
            "project_version": "1.2.3",
            "package_manifest": {"format_version": 1, "project_name": "demo", "project_version": "1.2.3"}
        });
        validate_restored_package_manifest(&matching, &root)
            .expect("matching restored manifest should pass");

        let mismatched = serde_json::json!({
            "project": "other",
            "project_version": "1.2.3"
        });
        let error = validate_restored_package_manifest(&mismatched, &root)
            .expect_err("mismatched restored manifest should fail");
        assert!(error.contains("did not match hosted response identity"));

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn pull_output_rejects_symlink_root_without_touching_target() {
        use super::resolve_pull_output_path;
        use std::os::unix::fs::symlink;

        let root = temp_dir("pull-output-symlink");
        let external = root.join("external");
        let output = root.join("output");
        fs::create_dir_all(&external).expect("external output should be writable");
        fs::write(external.join("sentinel.txt"), "outside")
            .expect("external sentinel should be writable");
        symlink(&external, &output).expect("output symlink should be created");

        let error = resolve_pull_output_path(&output, true)
            .expect_err("symlinked output root should be rejected");
        assert!(error.contains("symbolic link"));
        assert_eq!(
            fs::read_to_string(external.join("sentinel.txt"))
                .expect("external sentinel should remain"),
            "outside"
        );

        let _ = fs::remove_file(output);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn archive_and_pull_output_reject_linked_ancestors() {
        use super::resolve_pull_output_path;
        use std::os::unix::fs::symlink;

        let root = temp_dir("linked-boundaries");
        let package_root = root.join("package");
        let external = root.join("external");
        fs::create_dir_all(&package_root).expect("package root should exist");
        fs::create_dir_all(&external).expect("external root should exist");
        fs::write(external.join("secret.txt"), "outside")
            .expect("external fixture should be writable");
        symlink(external.join("secret.txt"), package_root.join("linked.txt"))
            .expect("linked package file should be created");
        let archive_error = create_package_archive_bytes(&package_root)
            .expect_err("archive creation should reject linked entries");
        assert!(archive_error.contains("symbolic link"));

        let linked_parent = root.join("linked-parent");
        symlink(&external, &linked_parent).expect("linked output parent should be created");
        let output_error = resolve_pull_output_path(&linked_parent.join("project"), true)
            .expect_err("pull output should reject a linked ancestor");
        assert!(output_error.contains("symbolic link"));
        assert!(!external.join("project").exists());
        assert_eq!(
            fs::read_to_string(external.join("secret.txt"))
                .expect("external fixture should remain readable"),
            "outside"
        );

        let _ = fs::remove_file(package_root.join("linked.txt"));
        let _ = fs::remove_file(linked_parent);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hosted_pull_receipt_reads_legacy_owner_id_but_omits_new_emission() {
        let legacy = r#"
format_version = 1
source_kind = "hosted"
hosted_source_id = "source-id"
hosted_version_id = "version-id"
owner_account_id = "private-account-id"
owner_handle = "alice"
package_name = "demo"
resolved_version = "1.0.0"
package_sha256 = "abc"
"#;
        let receipt: PulledPackageHostedReceiptDocument =
            toml::from_str(legacy).expect("legacy hosted receipt should remain readable");
        assert_eq!(receipt.hosted_source_id, "source-id");

        let rendered = toml::to_string_pretty(&receipt).expect("receipt should serialize");
        assert!(!rendered.contains("owner_account_id"));
    }

    #[cfg(windows)]
    #[test]
    fn archive_output_rejects_windows_reparse_attributes() {
        assert!(super::archive_windows_attributes_are_link_like(
            super::FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert!(!super::archive_windows_attributes_are_link_like(0));
    }
}
