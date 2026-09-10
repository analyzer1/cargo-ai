//! Generate inventory from assembled bytes without executing publisher code.
#[path = "../../templates/package_metadata.rs"]
mod contract;
pub(crate) use contract::{validate_manifest_metadata, InspectionMetadata, PublisherStatements};
use contract::{DeclaredEffects, InventoryFile};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

pub(crate) fn generate(
    root: &Path,
    publisher: PublisherStatements,
    subprocess: &str,
    definitions: impl Iterator<Item = String>,
) -> Result<InspectionMetadata, String> {
    let mut files = Vec::new();
    inventory(root, root, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let locks: Vec<_> = files
        .iter()
        .filter(|f| f.path.ends_with("Cargo.lock") || f.path.ends_with("packages.lock"))
        .collect();
    let dependency_lock_digest = if locks.is_empty() {
        None
    } else {
        Some(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&locks).map_err(|e| e.to_string())?)
        ))
    };
    let mut capabilities = std::collections::BTreeSet::new();
    for path in definitions.collect::<std::collections::BTreeSet<_>>() {
        if !files.iter().any(|file| file.path == path) {
            return Err("Declared definition is absent from the package inventory.".into());
        }
        let bytes = fs::read(root.join(&path)).map_err(|e| e.to_string())?;
        // Package assembly does not certify executable definitions; invalid or dynamic
        // inputs remain unknown until the normal validation/runtime boundary.
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if value
                .pointer("/agent_schema/properties")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|properties| !properties.is_empty())
            {
                capabilities.insert("structured_output".to_string());
                capabilities.insert("text".to_string());
                if value
                    .get("inputs")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|inputs| inputs.iter().any(|input| input["type"] == "image"))
                {
                    capabilities.insert("image".to_string());
                }
            }
            if value
                .get("actions")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|actions| {
                    actions.iter().any(|action| {
                        action
                            .get("run")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|steps| {
                                steps.iter().any(|step| step["kind"] == "generate_image")
                            })
                    })
                })
            {
                capabilities.insert("image_generation".to_string());
            }
        }
    }
    let metadata = InspectionMetadata {
        schema_version:1, builder_version:env!("CARGO_PKG_VERSION").into(),
        definition_schema:crate::schema_version::current_schema_version(), publisher, files,
        dependency_lock_digest, required_provider_capabilities:capabilities.into_iter().collect(),
        runtime_profile:"Static requirements from declared definitions only. Dynamic child agents, unparsed definitions and runtime inputs may add unknown requirements. Recipient-selected provider/model compatibility is checked at execution.".into(),
        effects:DeclaredEffects {
            filesystem:"Declared package payload, package data and explicit workspace permissions apply to Cargo AI-controlled operations. Tool effects outside those boundaries may be unknown.".into(),
            network:"Runtime provider destination depends on the selected profile. Dynamic tool destinations are unknown; inspect source.".into(),
            subprocess:format!("Cargo AI subprocess control: {subprocess}. Source tools can have open-world effects when execution is granted."),
            secrets:"Cargo AI does not collect runtime credentials into this metadata. Arbitrary asset contents, credential names and tool requirements still need source/profile review.".into(),
            transmitted_data:"Declared text/image inputs and runtime context may be sent to the selected provider. Additional dynamic tool data flows are unknown.".into(),
            destructive_actions:"Unknown for arbitrary tools; review editable source. Metadata is not a safety certification or operating-system sandbox.".into(),
        },
    };
    metadata.validate()?;
    Ok(metadata)
}

fn inventory(root: &Path, dir: &Path, files: &mut Vec<InventoryFile>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        if relative == "cargo-ai-package.toml" {
            continue;
        }
        if !contract::portable_path(&relative) {
            return Err("Invalid inventory path.".into());
        }
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if fs::symlink_metadata(&path)
                .map_err(|e| e.to_string())?
                .file_attributes()
                & 0x400
                != 0
            {
                return Err("Package inventory cannot traverse reparse points.".into());
            }
        }
        if kind.is_dir() {
            inventory(root, &path, files)?;
        } else if kind.is_file() {
            if files.len() >= 10_000 {
                return Err("Package inventory entry limit exceeded.".into());
            }
            let contents = fs::read(&path).map_err(|e| e.to_string())?;
            files.push(InventoryFile {
                path: relative,
                sha256: format!("{:x}", Sha256::digest(&contents)),
                bytes: contents.len() as u64,
            });
        } else {
            return Err("Package inventory cannot contain links or special files.".into());
        }
    }
    Ok(())
}

pub(crate) fn verify_inventory(root: &Path, manifest: &serde_json::Value) -> Result<(), String> {
    validate_manifest_metadata(manifest)?;
    if let Some(raw) = manifest.get("inspection") {
        let declared: InspectionMetadata =
            serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
        let mut actual = Vec::new();
        inventory(root, root, &mut actual)?;
        actual.sort_by(|a, b| a.path.cmp(&b.path));
        let mut expected = declared.files;
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        if actual != expected {
            return Err("Package contents differ from the inspection inventory.".into());
        }
    }
    Ok(())
}
