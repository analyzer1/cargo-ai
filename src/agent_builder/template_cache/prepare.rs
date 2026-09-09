use crate::agent_builder::build;
use crate::agent_builder::build_target::{BuildTarget, CargoCompileProfile};
use crate::agent_builder::project;
use std::fs;
use std::path::Path;

const TEMPLATE_SEED_AGENT_NAME: &str = "template_seed_agent";
const TEMPLATE_SEED_AGENTCFG: &str = include_str!("../../../templates/.agentcfg");

pub(super) fn prepare_warmed_template_workspace(
    path: &Path,
    build_target: &BuildTarget,
    profile: CargoCompileProfile,
) -> Result<(), String> {
    if path.exists() {
        fs::remove_dir_all(path).map_err(|error| {
            format!(
                "Failed to reset incomplete template workspace '{}': {error}",
                path.display()
            )
        })?;
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create template cache directory '{}': {error}",
                parent.display()
            )
        })?;
    }

    let creation_result = (|| -> Result<(), String> {
        project::create_template_project(path, TEMPLATE_SEED_AGENT_NAME, TEMPLATE_SEED_AGENTCFG)
            .map_err(|error| {
                format!(
                    "Failed to create warmed template workspace '{}': {error}",
                    path.display()
                )
            })?;
        build::build_workspace(path, build_target, profile).map_err(|error| {
            format!(
                "Failed to warm template workspace '{}': {error}",
                path.display()
            )
        })?;
        Ok(())
    })();

    if let Err(error) = creation_result {
        let _ = fs::remove_dir_all(path);
        return Err(error);
    }

    if !template_workspace_ready(path, build_target, profile) {
        let _ = fs::remove_dir_all(path);
        return Err(format!(
            "Warmed template workspace '{}' is incomplete after build.",
            path.display()
        ));
    }

    Ok(())
}

pub(super) fn template_workspace_ready(
    path: &Path,
    build_target: &BuildTarget,
    profile: CargoCompileProfile,
) -> bool {
    path.join("Cargo.toml").is_file()
        && path.join("build.rs").is_file()
        && path.join(".agentcfg").is_file()
        && path.join("src").join("main.rs").is_file()
        && build_target
            .compiled_binary_path(path, TEMPLATE_SEED_AGENT_NAME, profile)
            .is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_requires_the_selected_target_and_profile_seed() {
        let root = std::env::temp_dir().join(format!("cache-ready-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("src")).unwrap();
        for file in ["Cargo.toml", "build.rs", ".agentcfg", "src/main.rs"] {
            fs::write(root.join(file), "fixture").unwrap();
        }
        let target = BuildTarget::from_cli(Some("x86_64-pc-windows-msvc")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        assert!(!template_workspace_ready(
            &root,
            &target,
            CargoCompileProfile::Dev
        ));
        let dev =
            target.compiled_binary_path(&root, TEMPLATE_SEED_AGENT_NAME, CargoCompileProfile::Dev);
        fs::create_dir_all(dev.parent().unwrap()).unwrap();
        fs::write(&dev, "dev seed").unwrap();
        assert!(template_workspace_ready(
            &root,
            &target,
            CargoCompileProfile::Dev
        ));
        assert!(!template_workspace_ready(
            &root,
            &target,
            CargoCompileProfile::Release
        ));
        let release = target.compiled_binary_path(
            &root,
            TEMPLATE_SEED_AGENT_NAME,
            CargoCompileProfile::Release,
        );
        assert_eq!(release.extension().unwrap(), "exe");
        fs::create_dir_all(release.parent().unwrap()).unwrap();
        fs::write(&release, "release seed").unwrap();
        assert!(template_workspace_ready(
            &root,
            &target,
            CargoCompileProfile::Release
        ));
        fs::remove_file(&release).unwrap();
        assert!(!template_workspace_ready(
            &root,
            &target,
            CargoCompileProfile::Release
        ));
        fs::remove_file(root.join(".agentcfg")).unwrap();
        assert!(!template_workspace_ready(
            &root,
            &target,
            CargoCompileProfile::Dev
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
