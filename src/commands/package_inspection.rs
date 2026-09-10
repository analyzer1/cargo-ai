//! Hosted inspection and exact-snapshot checks before package materialization.
use clap::ArgMatches;
use serde_json::Value;

pub(crate) async fn run(matches: &ArgMatches) -> bool {
    let account = matches
        .get_one::<String>("account")
        .map(String::as_str)
        .filter(|s| !s.is_empty());
    let name = matches
        .get_one::<String>("alias")
        .map(String::as_str)
        .unwrap_or("");
    let source = matches.get_one::<String>("source_id").map(String::as_str);
    if source.is_none() && name.is_empty() {
        eprintln!("x Hosted inspection requires a package name or --source-id.");
        return false;
    }
    if source.is_some() && account.is_some() {
        eprintln!("x --source-id identifies its owner; omit the account handle.");
        return false;
    }
    match super::local_packages::read_hosted_package(
        name,
        account,
        source,
        matches.get_one::<String>("version").map(String::as_str),
        matches.get_one::<String>("version_id").map(String::as_str),
        true,
    )
    .await
    {
        Ok(snapshot) => {
            let result = if matches.get_flag("json") {
                validate_snapshot(&snapshot)
                    .map(|()| println!("{}", serde_json::to_string_pretty(&snapshot).unwrap()))
            } else {
                render_snapshot(&snapshot)
            };
            if let Err(error) = result {
                eprintln!("x {error}");
                return false;
            }
            true
        }
        Err(error) => {
            eprintln!("x {error}");
            false
        }
    }
}

fn validate_snapshot(value: &Value) -> Result<(), String> {
    for field in ["hosted_source_id", "hosted_version_id"] {
        let id = value[field].as_str().unwrap_or("");
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!("Inspection returned invalid {field}."));
        }
    }
    let digest = value["package_sha256"].as_str().unwrap_or("");
    if digest.len() != 64
        || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        || value["package_size_bytes"]
            .as_u64()
            .is_none_or(|n| n == 0 || n > 10 * 1024 * 1024)
        || !value["package_manifest"].is_object()
    {
        return Err("Inspection returned invalid package integrity facts.".into());
    }
    super::package_metadata::validate_manifest_metadata(&value["package_manifest"])?;
    Ok(())
}

pub(crate) fn render_snapshot(value: &Value) -> Result<(), String> {
    validate_snapshot(value)?;
    println!("Hosted package inspection (publisher declarations are untrusted):");
    // JSON escaping keeps terminal control characters in publisher text inert.
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    if value["package_manifest"].get("inspection").is_none() {
        println!("Inspection metadata: unavailable in this legacy package.");
    }
    println!("Review source and declared permissions before execution. Dynamic effects may be unknown; Cargo AI permissions are not an operating-system sandbox.");
    Ok(())
}

pub(crate) fn validate_same_snapshot(inspected: &Value, downloaded: &Value) -> Result<(), String> {
    validate_snapshot(inspected)?;
    for field in [
        "hosted_source_id",
        "hosted_version_id",
        "project_version",
        "package_sha256",
        "package_size_bytes",
        "package_manifest",
    ] {
        if inspected.get(field).is_none() || inspected[field] != downloaded[field] {
            return Err(format!("Downloaded package differs from the inspected {field}; inspect again before installation."));
        }
    }
    Ok(())
}

/// Match every declaration to the archive bytes, including legacy format-1 fields.
pub(crate) fn validate_archive_manifest(
    response: &Value,
    package_root: &std::path::Path,
) -> Result<(), String> {
    let raw = std::fs::read_to_string(package_root.join("cargo-ai-package.toml"))
        .map_err(|e| format!("Cannot read downloaded package manifest: {e}"))?;
    let parsed: toml::Value = toml::from_str(&raw)
        .map_err(|e| format!("Cannot parse downloaded package manifest: {e}"))?;
    let actual = serde_json::to_value(parsed).map_err(|e| e.to_string())?;
    if !response["package_manifest"].is_object() || actual != response["package_manifest"] {
        return Err("Downloaded archive manifest differs from the inspected package manifest; nothing installed or replaced.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn inspection_binds_identity_permissions_and_bytes_before_installation() {
        let snapshot = json!({"hosted_source_id":"source", "hosted_version_id":"version",
            "project_version":"1.0.0", "package_sha256":"a".repeat(64), "package_size_bytes":100,
            "package_manifest":{"permissions":{"subprocess":"blocked_without_explicit_grant"}}});
        assert!(validate_same_snapshot(&snapshot, &snapshot).is_ok());
        for field in [
            "hosted_source_id",
            "hosted_version_id",
            "project_version",
            "package_sha256",
            "package_size_bytes",
            "package_manifest",
        ] {
            let mut changed = snapshot.clone();
            changed[field] = Value::Null;
            assert!(
                validate_same_snapshot(&snapshot, &changed).is_err(),
                "{field}"
            );
        }
    }
}
