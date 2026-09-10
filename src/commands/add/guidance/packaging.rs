//! Excludes incidental assistant guidance while preserving declared assets.
use std::path::{Component, Path};

pub(crate) fn is_bundle_path(path: &Path) -> bool {
    let parts = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    parts.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case(".cargo-ai") && pair[1].eq_ignore_ascii_case("guidance")
    })
}

pub(crate) fn validate_declared(path: &Path) -> Result<(), String> {
    if super::is_reserved_guidance_path(path) {
        return Err(
            "Guidance locks and recovery state cannot be declared as package/build assets.".into(),
        );
    }
    Ok(())
}

/// `declared_bundle` applies only when the asset itself names a guidance path,
/// not when guidance happens to occur below a larger declared directory.
pub(crate) fn skip_entry(
    project_root: &Path,
    source: &Path,
    declared_bundle: bool,
) -> Result<bool, String> {
    let relative = source
        .strip_prefix(project_root)
        .map_err(|error| error.to_string())?;
    if super::is_reserved_guidance_path(relative) {
        return Ok(true);
    }
    if declared_bundle {
        return Ok(false);
    }
    if is_bundle_path(relative) {
        return Ok(true);
    }
    super::is_managed_entrypoint(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_guidance_is_recognized_without_matching_unrelated_names() {
        for path in [
            ".cargo-ai/guidance",
            "assets/vendor/.cargo-ai/guidance/start-here.md",
            "tools/helper/.CARGO-AI/GUIDANCE",
        ] {
            assert!(is_bundle_path(Path::new(path)), "{path}");
        }
        for path in [
            "guidance/manual.md",
            "assets/.cargo-ai/guidance-notes",
            "CLAUDE.md",
        ] {
            assert!(!is_bundle_path(Path::new(path)), "{path}");
        }
    }
}
