//! Bounded optional inspection metadata for format-1 packages.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectionMetadata {
    pub schema_version: u32,
    pub builder_version: String,
    pub definition_schema: String,
    pub publisher: PublisherStatements,
    pub files: Vec<InventoryFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_lock_digest: Option<String>,
    pub required_provider_capabilities: Vec<String>,
    pub runtime_profile: String,
    pub effects: DeclaredEffects,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherStatements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredEffects {
    pub filesystem: String,
    pub network: String,
    pub subprocess: String,
    pub secrets: String,
    pub transmitted_data: String,
    pub destructive_actions: String,
}

pub fn validate_manifest_metadata(manifest: &Value) -> Result<(), String> {
    if let Some(raw) = manifest.get("inspection") {
        if serde_json::to_vec(raw).map_err(|e| e.to_string())?.len() > 512 * 1024 {
            return Err("Package inspection metadata exceeds 512 KiB.".into());
        }
        let metadata: InspectionMetadata = serde_json::from_value(raw.clone())
            .map_err(|_| "Invalid or unsupported package inspection metadata.".to_string())?;
        metadata.validate()?;
    }
    Ok(())
}

pub fn portable_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !path.starts_with(".cargo-ai/publish-requests")
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl InspectionMetadata {
    pub fn validate(&self) -> Result<(), String> {
        let strings = [
            &self.builder_version,
            &self.definition_schema,
            &self.runtime_profile,
            &self.effects.filesystem,
            &self.effects.network,
            &self.effects.subprocess,
            &self.effects.secrets,
            &self.effects.transmitted_data,
            &self.effects.destructive_actions,
        ];
        if self.schema_version != 1
            || self.files.len() > 10_000
            || strings.iter().any(|s| {
                s.is_empty()
                    || s.len() > 4096
                    || s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
            })
            || self.required_provider_capabilities.len() > 16
            || self.required_provider_capabilities.iter().any(|s| {
                ![
                    "structured_output",
                    "image",
                    "image_generation",
                    "text",
                    "unknown",
                ]
                .contains(&s.as_str())
            })
            || self
                .dependency_lock_digest
                .as_deref()
                .is_some_and(|s| !digest(s))
        {
            return Err("Unsupported or unbounded package inspection metadata.".into());
        }
        for text in [
            &self.publisher.description,
            &self.publisher.license,
            &self.publisher.source_revision,
        ]
        .into_iter()
        .flatten()
        {
            if text.len() > 4096
                || text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return Err("Publisher statements exceed inspection text limits.".into());
            }
        }
        if self.publisher.source_revision.as_deref().is_some_and(|s| {
            !(40..=64).contains(&s.len()) || !s.bytes().all(|b| b.is_ascii_hexdigit())
        }) {
            return Err("Source revision must be a full hexadecimal revision, not a path or mutable reference.".into());
        }
        let mut paths = BTreeSet::new();
        let mut total = 0u64;
        for file in &self.files {
            total = total
                .checked_add(file.bytes)
                .ok_or("Package inventory size overflow")?;
            if !portable_path(&file.path)
                || file.path == "cargo-ai-package.toml"
                || !digest(&file.sha256)
                || !paths.insert(&file.path)
                || total > 100 * 1024 * 1024
            {
                return Err("Invalid, duplicate or unbounded package inventory entry.".into());
            }
        }
        Ok(())
    }
}
