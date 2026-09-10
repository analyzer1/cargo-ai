//! Credential-free request receipts written durably before publication is transmitted.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    format_version: u32,
    request_id: String,
    intent_sha256: String,
    project_name: String,
    project_version: String,
    package_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hosted_source_id: Option<String>,
}

pub(crate) fn request_id(
    root: &Path,
    name: &str,
    version: &str,
    digest: &str,
    source: Option<&str>,
) -> Result<String, String> {
    let intent = serde_json::to_vec(&serde_json::json!([name, version, digest, source]))
        .map_err(|e| e.to_string())?;
    let intent = format!("{:x}", Sha256::digest(intent));
    let mut directory = root.to_path_buf();
    for component in [".cargo-ai", "publish-requests"] {
        directory.push(component);
        match fs::create_dir(&directory) {
            Ok(()) => {
                sync_directory(
                    directory
                        .parent()
                        .ok_or("Receipt directory has no parent")?,
                )?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "Cannot prepare publication request receipt: {error}"
                ))
            }
        }
        let metadata = fs::symlink_metadata(&directory).map_err(|e| e.to_string())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || link_like(&metadata) {
            return Err("Publication receipt directory must be a real project directory.".into());
        }
    }
    for attempt in 0..1024 {
        let path = directory.join(if attempt == 0 {
            format!("{intent}.json")
        } else {
            format!("{intent}-{attempt}.json")
        });
        let proposed = Receipt {
            format_version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            intent_sha256: intent.clone(),
            project_name: name.into(),
            project_version: version.into(),
            package_sha256: digest.into(),
            hosted_source_id: source.map(str::to_string),
        };
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let bytes = serde_json::to_vec_pretty(&proposed).map_err(|e| e.to_string())?;
                file.write_all(&bytes)
                    .and_then(|_| file.sync_all())
                    .map_err(|e| {
                        format!("Cannot persist publication identity; nothing transmitted: {e}")
                    })?;
                sync_directory(&directory)?;
                return Ok(proposed.request_id);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || link_like(&metadata)
                    || metadata.len() > 16_384
                {
                    return Err("Invalid publication request receipt; nothing transmitted.".into());
                }
                let stored: Receipt = serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
                .map_err(|_| "Incomplete publication request receipt; nothing transmitted. Inspect the receipt before recovery.".to_string())?;
                let mut expected = proposed;
                expected.request_id = stored.request_id.clone();
                if stored != expected || uuid::Uuid::parse_str(&stored.request_id).is_err() {
                    return Err(
                        "Publication request receipt does not match the immutable intent.".into(),
                    );
                }
                let terminal = directory.join(format!("{}.completed.json", stored.request_id));
                if terminal.try_exists().map_err(|e| e.to_string())? {
                    let completed = read_terminal(&terminal)?;
                    if completed.request_id != stored.request_id
                        || completed.package_sha256 != digest
                        || completed.project_version != version
                    {
                        return Err(
                            "Publication terminal receipt does not match its request.".into()
                        );
                    }
                    continue;
                }
                return Ok(stored.request_id);
            }
            Err(error) => {
                return Err(format!(
                    "Cannot persist publication request receipt: {error}"
                ))
            }
        }
    }
    Err("Publication receipt history limit reached; review local receipts before recovery.".into())
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Terminal {
    request_id: String,
    hosted_source_id: String,
    hosted_version_id: String,
    project_version: String,
    package_sha256: String,
}

fn read_terminal(path: &Path) -> Result<Terminal, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || link_like(&metadata)
        || metadata.len() > 16_384
    {
        return Err("Invalid publication terminal receipt.".into());
    }
    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|_| "Incomplete publication terminal receipt; review before recovery.".into())
}

pub(crate) fn record_terminal(
    root: &Path,
    request_id: &str,
    version: &str,
    digest: &str,
    response: &serde_json::Value,
) -> Result<(), String> {
    let opaque = |key: &str| -> Result<String, String> {
        let id = response[key].as_str().unwrap_or("");
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("Publication response has an invalid immutable identity; preserve the request for retry.".into());
        }
        Ok(id.into())
    };
    if uuid::Uuid::parse_str(request_id).is_err()
        || response["package_sha256"] != digest
        || response["project_version"] != version
    {
        return Err(
            "Publication response differs from its request; preserve the request for retry.".into(),
        );
    }
    let terminal = Terminal {
        request_id: request_id.into(),
        hosted_source_id: opaque("hosted_source_id")?,
        hosted_version_id: opaque("hosted_version_id")?,
        project_version: version.into(),
        package_sha256: digest.into(),
    };
    let mut directory = root.to_path_buf();
    for part in [".cargo-ai", "publish-requests"] {
        directory.push(part);
        let metadata = fs::symlink_metadata(&directory).map_err(|e| e.to_string())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || link_like(&metadata) {
            return Err(
                "Publication receipt directory changed; preserve request for retry.".into(),
            );
        }
    }
    let path = directory.join(format!("{request_id}.completed.json"));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(&serde_json::to_vec_pretty(&terminal).map_err(|e| e.to_string())?)
                .and_then(|_| file.sync_all())
                .map_err(|e| {
                    format!("Publication succeeded but its terminal receipt was not durable: {e}")
                })?;
            sync_directory(&directory)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if read_terminal(&path)? == terminal {
                Ok(())
            } else {
                Err("Conflicting publication terminal receipt.".into())
            }
        }
        Err(e) => Err(format!(
            "Publication succeeded but its terminal receipt could not be written: {e}"
        )),
    }
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("Cannot persist publication receipt directory: {e}"))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn link_like(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_durable_matching_terminal_result_starts_a_new_request() {
        let root = std::env::temp_dir().join(format!("publish-terminal-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let digest = "a".repeat(64);
        let first = request_id(&root, "original", "1.0.0", &digest, None).unwrap();
        let mut response = serde_json::json!({"hosted_source_id":"source", "hosted_version_id":"version", "project_version":"1.0.0", "package_sha256":"b".repeat(64)});
        assert!(record_terminal(&root, &first, "1.0.0", &digest, &response).is_err());
        assert_eq!(
            request_id(&root, "original", "1.0.0", &digest, None).unwrap(),
            first
        );
        response["package_sha256"] = digest.clone().into();
        record_terminal(&root, &first, "1.0.0", &digest, &response).unwrap();
        record_terminal(&root, &first, "1.0.0", &digest, &response).unwrap();
        let second = request_id(&root, "original", "1.0.0", &digest, None).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            second,
            request_id(&root, "original", "1.0.0", &digest, None).unwrap()
        );
        let receipt = root
            .join(".cargo-ai/publish-requests")
            .join(format!("{first}.completed.json"));
        fs::write(receipt, "{").unwrap();
        assert!(request_id(&root, "original", "1.0.0", &digest, None).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retries_after_restart_reuse_the_persisted_intent_without_credentials() {
        let root = std::env::temp_dir().join(format!("publish-receipt-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let first = request_id(&root, "original", "1.0.0", &"a".repeat(64), None).unwrap();
        assert_eq!(
            first,
            request_id(&root, "original", "1.0.0", &"a".repeat(64), None).unwrap()
        );
        assert_ne!(
            first,
            request_id(&root, "original", "1.0.1", &"b".repeat(64), None).unwrap()
        );
        for entry in fs::read_dir(root.join(".cargo-ai/publish-requests")).unwrap() {
            let raw = fs::read_to_string(entry.unwrap().path()).unwrap();
            assert!(!raw.contains("token") && !raw.contains("credentials"));
        }
        fs::remove_dir_all(root).unwrap();
    }
}
