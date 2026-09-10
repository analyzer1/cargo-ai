//! Resolve only explicitly enrolled package identities before source checkout.

use super::qualification_policy::{hexadecimal, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Catalog {
    pub schema_version: u32,
    pub official_package_count: usize,
    #[serde(default)]
    pub official_packages: Vec<Package>,
    #[serde(default)]
    pub qualification_canaries: Vec<Package>,
}

#[derive(Debug, Deserialize)]
pub struct Package {
    pub repository: String,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub declaration_path: String,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub release_required: bool,
}

pub fn repository(value: &str) -> bool {
    let parts: Vec<_> = value.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && *part != "."
                && *part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        })
}

fn declaration(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['/', '\\'])
        && !value
            .chars()
            .any(|c| c.is_control() || c == ':' || c == '\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

impl Catalog {
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.len() > 1024 * 1024 {
            return Err("oversized qualification catalog");
        }
        let catalog: Self = toml::from_str(raw).map_err(|_| "invalid qualification catalog")?;
        if catalog.schema_version != 1
            || catalog.official_package_count != catalog.official_packages.len()
        {
            return Err("invalid catalog version or package count");
        }
        Ok(catalog)
    }

    pub fn canary(&self) -> Result<&Package> {
        let mut found = self
            .qualification_canaries
            .iter()
            .filter(|p| p.enabled && p.release_required);
        let row = found.next().ok_or("missing qualification canary")?;
        if found.next().is_some()
            || !repository(&row.repository)
            || !hexadecimal(&row.revision, 40)
            || row.platforms.is_empty()
            || row.platforms.len() > 3
            || row
                .platforms
                .iter()
                .any(|p| !["ubuntu-latest", "macos-latest", "windows-latest"].contains(&p.as_str()))
            || row
                .platforms
                .iter()
                .enumerate()
                .any(|(i, p)| row.platforms[..i].contains(p))
        {
            return Err("invalid or ambiguous qualification canary");
        }
        Ok(row)
    }

    pub fn resolve(
        &self,
        requested_repo: &str,
        requested_sha: &str,
        requested_path: &str,
    ) -> Result<String> {
        let requested_repo = requested_repo.trim();
        let (row, revision) = if requested_repo.is_empty() {
            let row = self.canary()?;
            (row, row.revision.as_str())
        } else {
            let mut found = self
                .qualification_canaries
                .iter()
                .chain(&self.official_packages)
                .filter(|p| p.repository == requested_repo);
            let row = found.next().ok_or("package is not allowlisted")?;
            if found.next().is_some() {
                return Err("package is not uniquely allowlisted");
            }
            (row, requested_sha.trim())
        };
        let path = if requested_path.trim().is_empty() {
            row.declaration_path.as_str()
        } else {
            requested_path.trim()
        };
        if !repository(&row.repository) || !hexadecimal(revision, 40) || !declaration(path) {
            return Err("package identity must be exact and declaration portable/package-relative");
        }
        // Only validated single-line values may cross the Actions output boundary.
        Ok(format!(
            "repository={}\nsha={revision}\ndeclaration={path}\n",
            row.repository
        ))
    }
}
