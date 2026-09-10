use crate::agent_builder::build_target::{BuildTarget, CargoCompileProfile};
use crate::cargo_ai_metadata;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TemplateCacheKey {
    pub binary_sha256: String,
    pub rustc_version: String,
    pub target_triple: String,
    pub compile_profile: CargoCompileProfile,
}

pub(super) fn resolve_template_cache_key(
    build_target: &BuildTarget,
    compile_profile: CargoCompileProfile,
) -> Result<TemplateCacheKey, String> {
    Ok(TemplateCacheKey {
        binary_sha256: cargo_ai_metadata::current_binary_sha256()?,
        rustc_version: normalized_rustc_version()?,
        target_triple: build_target.cache_key_target().to_string(),
        compile_profile,
    })
}

fn normalized_rustc_version() -> Result<String, String> {
    let output = Command::new("rustc")
        .arg("--version")
        .output()
        .map_err(|error| format!("Failed to execute `rustc --version`: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "`rustc --version` exited with status {}",
            output.status
        ));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("`rustc --version` returned non-UTF-8 output: {error}"))?;

    normalized_rustc_version_from_output(&stdout).ok_or_else(|| {
        format!(
            "Unable to normalize rustc version from output: {}",
            stdout.trim()
        )
    })
}

fn normalized_rustc_version_from_output(output: &str) -> Option<String> {
    let mut parts = output.split_whitespace();
    let first = parts.next()?;
    let second = parts.next()?;
    if first != "rustc" {
        return None;
    }
    Some(format!("rustc-{second}"))
}

#[cfg(test)]
mod tests {
    use super::normalized_rustc_version_from_output;

    #[test]
    fn normalizes_rustc_semver_output() {
        let output = "rustc 1.90.0 (1159e78c4 2025-09-14)\n";
        assert_eq!(
            normalized_rustc_version_from_output(output).as_deref(),
            Some("rustc-1.90.0")
        );
    }

    #[test]
    fn rejects_unexpected_rustc_output_shape() {
        assert!(normalized_rustc_version_from_output("cargo 1.90.0").is_none());
        assert!(normalized_rustc_version_from_output("").is_none());
    }
}
