//! Target resolution helpers for hatch builds and exports.

use std::env;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CargoCompileProfile {
    Dev,
    Release,
}

impl CargoCompileProfile {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Release => "release",
        }
    }

    pub(crate) fn output_directory(self) -> &'static str {
        match self {
            Self::Dev => "debug",
            Self::Release => "release",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BuildTarget {
    cargo_target: Option<String>,
    cache_key_target: String,
}

impl BuildTarget {
    pub(crate) fn from_cli(raw_target: Option<&str>) -> Result<Self, String> {
        let cargo_target = match raw_target {
            Some(value) if value.trim().is_empty() => {
                return Err("Target triple cannot be empty. Provide --target <TRIPLE>.".to_string());
            }
            Some(value) => Some(value.trim().to_string()),
            None => configured_target_triple(),
        };

        let cache_key_target = cargo_target
            .clone()
            .unwrap_or_else(crate::cargo_ai_metadata::current_build_target);

        Ok(Self {
            cargo_target,
            cache_key_target,
        })
    }

    pub(crate) fn cargo_target(&self) -> Option<&str> {
        self.cargo_target.as_deref()
    }

    pub(crate) fn cache_key_target(&self) -> &str {
        &self.cache_key_target
    }

    pub(crate) fn cargo_args(&self, command: &str, profile: CargoCompileProfile) -> Vec<String> {
        let mut args = vec![command.to_string()];
        if profile == CargoCompileProfile::Release {
            args.push("--release".to_string());
        }
        if let Some(target_triple) = self.cargo_target() {
            args.push("--target".to_string());
            args.push(target_triple.to_string());
        }
        args
    }

    pub(crate) fn compiled_binary_path(
        &self,
        project_path: &Path,
        agent_name: &str,
        profile: CargoCompileProfile,
    ) -> PathBuf {
        let mut source_path = project_path.join("target");
        if let Some(target_triple) = self.cargo_target() {
            source_path = source_path.join(target_triple);
        }
        source_path = source_path
            .join(profile.output_directory())
            .join(agent_name);
        if self.uses_windows_exe() {
            source_path.set_extension("exe");
        }
        source_path
    }

    pub(crate) fn exported_binary_path(&self, dest_dir: &Path, agent_name: &str) -> PathBuf {
        let mut dest_path = dest_dir.join(agent_name);
        if self.uses_windows_exe() {
            dest_path.set_extension("exe");
        }
        dest_path
    }

    fn uses_windows_exe(&self) -> bool {
        self.cargo_target
            .as_ref()
            .map(|target| target.contains("windows"))
            .unwrap_or(cfg!(windows))
    }
}

fn configured_target_triple() -> Option<String> {
    env::var("CARGO_BUILD_TARGET")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{BuildTarget, CargoCompileProfile};
    use std::path::{Path, PathBuf};

    #[test]
    fn explicit_target_builds_cargo_args() {
        let target = BuildTarget {
            cargo_target: Some("x86_64-pc-windows-msvc".to_string()),
            cache_key_target: "x86_64-pc-windows-msvc".to_string(),
        };

        assert_eq!(
            target.cargo_args("check", CargoCompileProfile::Dev),
            vec![
                "check".to_string(),
                "--target".to_string(),
                "x86_64-pc-windows-msvc".to_string()
            ]
        );
    }

    #[test]
    fn explicit_target_owns_export_paths_and_cache_key() {
        let target = BuildTarget {
            cargo_target: Some("x86_64-pc-windows-msvc".to_string()),
            cache_key_target: "x86_64-pc-windows-msvc".to_string(),
        };
        let project_path = PathBuf::from("/tmp/demo-agent");

        assert_eq!(target.cache_key_target(), "x86_64-pc-windows-msvc");
        assert_eq!(
            target.compiled_binary_path(
                &project_path,
                "weather_test",
                CargoCompileProfile::Release
            ),
            project_path
                .join("target")
                .join("x86_64-pc-windows-msvc")
                .join("release")
                .join("weather_test.exe")
        );
        assert_eq!(
            target.exported_binary_path(Path::new("/tmp/out"), "weather_test"),
            Path::new("/tmp/out").join("weather_test.exe")
        );
    }

    #[test]
    fn empty_target_triple_is_rejected() {
        let err = BuildTarget::from_cli(Some("   ")).expect_err("empty target triple should fail");
        assert!(err.contains("--target"));
    }

    #[test]
    fn compile_profile_separates_arguments_and_artifacts_on_every_target() {
        for triple in [
            None,
            Some("aarch64-apple-darwin"),
            Some("x86_64-pc-windows-msvc"),
        ] {
            let target = BuildTarget::from_cli(triple).unwrap();
            let dev = target.compiled_binary_path(
                Path::new("project"),
                "probe",
                CargoCompileProfile::Dev,
            );
            let release = target.compiled_binary_path(
                Path::new("project"),
                "probe",
                CargoCompileProfile::Release,
            );
            assert_ne!(dev, release);
            assert_eq!(dev.parent().unwrap().file_name().unwrap(), "debug");
            assert_eq!(release.parent().unwrap().file_name().unwrap(), "release");
            assert_eq!(dev.file_name(), release.file_name());
            assert!(!target
                .cargo_args("check", CargoCompileProfile::Dev)
                .contains(&"--release".to_string()));
            assert!(target
                .cargo_args("build", CargoCompileProfile::Release)
                .contains(&"--release".to_string()));
        }
    }
}
