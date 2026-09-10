#[cfg(test)]
use crate::config::schema::CargoAiMetadata;
use serde_json::{json, Value};
use std::time::Duration;

const PROJECTS_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const PROJECTS_RESPONSE_MAX_BYTES: usize = 16 * 1024 * 1024;

async fn post_projects_request(url: String, body: &Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(PROJECTS_REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("Failed to configure projects request: {error}"))?;
    let mut response = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|error| format!("Projects request failed: {error}"))?;

    if response
        .content_length()
        .map(|length| length > PROJECTS_RESPONSE_MAX_BYTES as u64)
        .unwrap_or(false)
    {
        return Err(format!(
            "Projects response exceeded the {}-byte client limit.",
            PROJECTS_RESPONSE_MAX_BYTES
        ));
    }

    let mut response_bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("Failed to read projects response: {error}"))?
    {
        let next_size = checked_response_size(response_bytes.len(), chunk.len())?;
        response_bytes.reserve(next_size - response_bytes.len());
        response_bytes.extend_from_slice(&chunk);
    }

    serde_json::from_slice(&response_bytes)
        .map_err(|error| format!("Failed to parse projects response JSON: {error}"))
}

fn checked_response_size(current_size: usize, chunk_size: usize) -> Result<usize, String> {
    let next_size = current_size
        .checked_add(chunk_size)
        .ok_or_else(|| "Projects response size exceeded supported bounds.".to_string())?;
    if next_size > PROJECTS_RESPONSE_MAX_BYTES {
        return Err(format!(
            "Projects response exceeded the {}-byte client limit.",
            PROJECTS_RESPONSE_MAX_BYTES
        ));
    }
    Ok(next_size)
}

/// List projects for the authenticated account or for a specific owner handle.
pub async fn list_projects(
    base_url: &str,
    access_token: &str,
    owner_handle: Option<&str>,
    include_archived: bool,
) -> Result<Value, String> {
    let url = format!("{}/account", base_url.trim_end_matches('/'));
    let body = build_list_projects_body(access_token, owner_handle, include_archived);

    post_projects_request(url, &body).await
}

/// Publish a packaged project archive.
pub async fn publish_project(
    base_url: &str,
    access_token: &str,
    project_name: &str,
    project_version: &str,
    package_manifest: Value,
    package_sha256: &str,
    package_size_bytes: i64,
    package_archive_base64: &str,
    request_id: &str,
    hosted_source_id: Option<&str>,
) -> Result<Value, String> {
    let url = format!("{}/account", base_url.trim_end_matches('/'));
    let mut body = build_publish_project_body(
        access_token,
        project_name,
        project_version,
        package_manifest,
        package_sha256,
        package_size_bytes,
        package_archive_base64,
    );

    body["projects"]["publish"]["publish_request_id"] = json!(request_id);
    if let Some(source) = hosted_source_id {
        body["projects"]["publish"]["hosted_source_id"] = json!(source);
    }
    post_projects_request(url, &body).await
}

pub async fn read_project(
    base_url: &str,
    access_token: &str,
    name: &str,
    owner_handle: Option<&str>,
    hosted_source_id: Option<&str>,
    version: Option<&str>,
    hosted_version_id: Option<&str>,
    inspect: bool,
) -> Result<Value, String> {
    let url = format!("{}/account", base_url.trim_end_matches('/'));
    let mut body =
        build_pull_project_body(access_token, name, owner_handle, hosted_source_id, version);
    if let Some(id) = hosted_version_id {
        body["projects"]["pull"]["hosted_version_id"] = json!(id);
    }
    if inspect {
        let payload = body["projects"]
            .as_object_mut()
            .unwrap()
            .remove("pull")
            .unwrap();
        body["projects"]["inspect"] = payload;
    }

    post_projects_request(url, &body).await
}

pub async fn rename_project(
    base_url: &str,
    access_token: &str,
    source_id: &str,
    name: &str,
) -> Result<Value, String> {
    let body = super::with_cargo_ai_metadata(json!({"action":"projects",
        "credentials":{"access_token":access_token},
        "projects":{"rename":{"hosted_source_id":source_id,"name":name}}}));
    post_projects_request(format!("{}/account", base_url.trim_end_matches('/')), &body).await
}

pub async fn set_project_visibility(
    base_url: &str,
    access_token: &str,
    name: &str,
    is_public: bool,
) -> Result<Value, String> {
    let url = format!("{}/account", base_url.trim_end_matches('/'));
    let body = build_set_project_visibility_body(access_token, name, is_public);

    post_projects_request(url, &body).await
}

pub async fn set_project_archive(
    base_url: &str,
    access_token: &str,
    name: &str,
    is_archived: bool,
) -> Result<Value, String> {
    let url = format!("{}/account", base_url.trim_end_matches('/'));
    let body = build_set_project_archive_body(access_token, name, is_archived);

    post_projects_request(url, &body).await
}

fn build_list_projects_body(
    access_token: &str,
    owner_handle: Option<&str>,
    include_archived: bool,
) -> Value {
    let mut list_payload = json!({
        "include_archived": include_archived,
    });

    if let Some(handle) = owner_handle {
        list_payload["owner_handle"] = json!(handle);
    }

    super::with_cargo_ai_metadata(json!({
        "action": "projects",
        "credentials": {
            "access_token": access_token
        },
        "projects": {
            "list": list_payload
        }
    }))
}

fn build_publish_project_body(
    access_token: &str,
    project_name: &str,
    project_version: &str,
    package_manifest: Value,
    package_sha256: &str,
    package_size_bytes: i64,
    package_archive_base64: &str,
) -> Value {
    super::with_cargo_ai_metadata(json!({
        "action": "projects",
        "credentials": {
            "access_token": access_token
        },
        "projects": {
            "publish": {
                "project_name": project_name,
                "project_version": project_version,
                "package_manifest": package_manifest,
                "package_sha256": package_sha256,
                "package_size_bytes": package_size_bytes,
                "package_archive_base64": package_archive_base64
            }
        }
    }))
}

pub(crate) fn estimate_publish_project_request_size(
    access_token: &str,
    project_name: &str,
    project_version: &str,
    package_manifest: Value,
    package_sha256: &str,
    package_size_bytes: i64,
    package_archive_base64: &str,
) -> Result<u64, String> {
    let body = build_publish_project_body(
        access_token,
        project_name,
        project_version,
        package_manifest,
        package_sha256,
        package_size_bytes,
        package_archive_base64,
    );
    let serialized = serde_json::to_vec(&body)
        .map_err(|error| format!("Failed to estimate project publish request size: {error}"))?;
    u64::try_from(serialized.len()).map_err(|_| {
        "Estimated project publish request size exceeded supported limits.".to_string()
    })
}

fn build_pull_project_body(
    access_token: &str,
    name: &str,
    owner_handle: Option<&str>,
    hosted_source_id: Option<&str>,
    version: Option<&str>,
) -> Value {
    let mut pull_payload = json!({
        "name": name
    });

    if let Some(handle) = owner_handle {
        pull_payload["owner_handle"] = json!(handle);
    }
    if let Some(source_id) = hosted_source_id {
        pull_payload["hosted_source_id"] = json!(source_id);
    }
    if let Some(version) = version {
        pull_payload["version"] = json!(version);
    }

    super::with_cargo_ai_metadata(json!({
        "action": "projects",
        "credentials": {
            "access_token": access_token
        },
        "projects": {
            "pull": pull_payload
        }
    }))
}

fn build_set_project_visibility_body(access_token: &str, name: &str, is_public: bool) -> Value {
    super::with_cargo_ai_metadata(json!({
        "action": "projects",
        "credentials": {
            "access_token": access_token
        },
        "projects": {
            "visibility": {
                "name": name,
                "is_public": is_public
            }
        }
    }))
}

fn build_set_project_archive_body(access_token: &str, name: &str, is_archived: bool) -> Value {
    super::with_cargo_ai_metadata(json!({
        "action": "projects",
        "credentials": {
            "access_token": access_token
        },
        "projects": {
            "archive": {
                "name": name,
                "is_archived": is_archived
            }
        }
    }))
}

#[cfg(test)]
fn build_publish_project_body_with_metadata(
    access_token: &str,
    project_name: &str,
    project_version: &str,
    package_manifest: Value,
    package_sha256: &str,
    package_size_bytes: i64,
    package_archive_base64: &str,
    metadata: Option<CargoAiMetadata>,
) -> Value {
    super::with_cargo_ai_metadata_override(
        json!({
            "action": "projects",
            "credentials": {
                "access_token": access_token
            },
            "projects": {
                "publish": {
                    "project_name": project_name,
                    "project_version": project_version,
                    "package_manifest": package_manifest,
                    "package_sha256": package_sha256,
                    "package_size_bytes": package_size_bytes,
                    "package_archive_base64": package_archive_base64
                }
            }
        }),
        metadata,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        build_publish_project_body_with_metadata, build_pull_project_body, checked_response_size,
        PROJECTS_RESPONSE_MAX_BYTES,
    };
    use crate::config::schema::CargoAiMetadata;
    use serde_json::json;

    fn sample_metadata() -> CargoAiMetadata {
        CargoAiMetadata {
            cargo_ai_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            template_schema_version: Some("2026-03-03.r1".to_string()),
            cargo_ai_build_target: Some("aarch64-apple-darwin".to_string()),
            cargo_ai_install_id: Some("install-123".to_string()),
            cargo_ai_binary_sha256: Some("hash-456".to_string()),
        }
    }

    #[test]
    fn build_publish_project_body_includes_top_level_metadata() {
        let body = build_publish_project_body_with_metadata(
            "access-token-123",
            "demo_project",
            "0.1.0",
            json!({
                "format_version": 1,
                "profile": "default"
            }),
            "abc123",
            1024,
            "cGFja2FnZQ==",
            Some(sample_metadata()),
        );

        assert_eq!(body["action"], "projects");
        assert_eq!(body["projects"]["publish"]["project_name"], "demo_project");
        assert_eq!(body["projects"]["publish"]["project_version"], "0.1.0");
        assert_eq!(body["projects"]["publish"]["package_sha256"], "abc123");
        assert_eq!(body["projects"]["publish"]["package_size_bytes"], 1024);
        assert_eq!(
            body["cargo_ai_metadata"]["cargo_ai_binary_sha256"],
            "hash-456"
        );
    }

    #[test]
    fn projects_response_size_is_bounded_before_json_parsing() {
        assert_eq!(
            checked_response_size(PROJECTS_RESPONSE_MAX_BYTES - 1, 1)
                .expect("response at the limit should be accepted"),
            PROJECTS_RESPONSE_MAX_BYTES
        );
        let error = checked_response_size(PROJECTS_RESPONSE_MAX_BYTES, 1)
            .expect_err("response beyond the limit should be rejected");
        assert!(error.contains("client limit"));
    }

    #[test]
    fn pull_body_supports_stable_hosted_source_identity() {
        let body = build_pull_project_body(
            "access-token",
            "demo",
            None,
            Some("source-id"),
            Some("1.2.3"),
        );
        assert_eq!(body["projects"]["pull"]["hosted_source_id"], "source-id");
        assert!(body["projects"]["pull"].get("owner_handle").is_none());
    }
}
