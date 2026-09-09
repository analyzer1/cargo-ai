//! Handles compiling an agent workspace into an executable.

use super::build_target::{BuildTarget, CargoCompileProfile};
use std::path::Path;
use std::process::{Command, Stdio};

/// Builds the agent project at the given path (e.g. `.cargo-ai/agents/my_agent`)
pub fn build_agent_project(
    agent_name: &str,
    build_target: &BuildTarget,
    shared_target_dir: Option<&Path>,
) -> Result<(), std::io::Error> {
    let project_path = super::agent_workspace_path(agent_name);
    run_cargo_compile_in_path(
        &project_path,
        "build",
        build_target,
        CargoCompileProfile::Release,
        shared_target_dir,
    )
}

/// Runs `cargo check` for the agent project at the given path.
pub fn check_agent_project(
    agent_name: &str,
    build_target: &BuildTarget,
    shared_target_dir: Option<&Path>,
) -> Result<(), std::io::Error> {
    let project_path = super::agent_workspace_path(agent_name);
    run_cargo_compile_in_path(
        &project_path,
        "check",
        build_target,
        CargoCompileProfile::Dev,
        shared_target_dir,
    )
}

/// Builds an arbitrary workspace path with `cargo build`.
pub(crate) fn build_workspace(
    project_path: &Path,
    build_target: &BuildTarget,
    profile: CargoCompileProfile,
) -> Result<(), std::io::Error> {
    run_cargo_compile_in_path(project_path, "build", build_target, profile, None)
}

fn run_cargo_compile_in_path(
    project_path: &Path,
    command: &str,
    build_target: &BuildTarget,
    profile: CargoCompileProfile,
    shared_target_dir: Option<&Path>,
) -> Result<(), std::io::Error> {
    if !project_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Agent path does not exist",
        ));
    }

    let mut cargo_command = prepare_cargo_compile_command(
        project_path,
        command,
        build_target,
        profile,
        shared_target_dir,
    );

    let status = cargo_command.status()?;

    if !status.success() {
        let target_detail = build_target
            .cargo_target()
            .map(|target| {
                format!(
                    " for target '{target}'. See compiler output above for missing rustup target, linker, or SDK details."
                )
            })
            .unwrap_or_default();
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Cargo {command} failed{target_detail}"),
        ));
    }

    Ok(())
}

fn prepare_cargo_compile_command(
    project_path: &Path,
    command: &str,
    build_target: &BuildTarget,
    profile: CargoCompileProfile,
    shared_target_dir: Option<&Path>,
) -> Command {
    let mut cargo_command = Command::new("cargo");
    cargo_command
        .args(build_target.cargo_args(command, profile))
        .current_dir(project_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if let Some(target_dir) = shared_target_dir {
        cargo_command.env("CARGO_TARGET_DIR", target_dir);
    }

    cargo_command
}

#[cfg(test)]
mod tests {
    use super::prepare_cargo_compile_command;
    use crate::agent_builder::build_target::{BuildTarget, CargoCompileProfile};
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn shared_target_dir_is_applied_to_check_commands() {
        let build_target = BuildTarget::from_cli(Some("x86_64-pc-windows-msvc"))
            .expect("explicit target should resolve");
        let command = prepare_cargo_compile_command(
            Path::new("/tmp/demo-agent"),
            "check",
            &build_target,
            CargoCompileProfile::Dev,
            Some(Path::new("/tmp/shared-target")),
        );

        let args = command
            .get_args()
            .map(|value| value.to_os_string())
            .collect::<Vec<_>>();
        let current_dir = command
            .get_current_dir()
            .expect("current dir should be set")
            .to_path_buf();
        let envs = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(|entry| entry.to_os_string())))
            .collect::<Vec<_>>();

        assert_eq!(current_dir, Path::new("/tmp/demo-agent"));
        assert_eq!(
            args,
            vec![
                OsString::from("check"),
                OsString::from("--target"),
                OsString::from("x86_64-pc-windows-msvc"),
            ]
        );
        assert!(envs.iter().any(|(key, value)| {
            key == "CARGO_TARGET_DIR"
                && value.as_ref() == Some(&OsString::from("/tmp/shared-target"))
        }));
    }

    #[test]
    fn shared_target_dir_is_applied_to_build_commands() {
        let build_target = BuildTarget::from_cli(Some("x86_64-pc-windows-msvc"))
            .expect("explicit target should resolve");
        let command = prepare_cargo_compile_command(
            Path::new("/tmp/demo-agent"),
            "build",
            &build_target,
            CargoCompileProfile::Release,
            Some(Path::new("/tmp/shared-target")),
        );

        let args = command
            .get_args()
            .map(|value| value.to_os_string())
            .collect::<Vec<_>>();
        let current_dir = command
            .get_current_dir()
            .expect("current dir should be set")
            .to_path_buf();
        let envs = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(|entry| entry.to_os_string())))
            .collect::<Vec<_>>();

        assert_eq!(current_dir, Path::new("/tmp/demo-agent"));
        assert_eq!(
            args,
            vec![
                OsString::from("build"),
                OsString::from("--release"),
                OsString::from("--target"),
                OsString::from("x86_64-pc-windows-msvc"),
            ]
        );
        assert!(envs.iter().any(|(key, value)| {
            key == "CARGO_TARGET_DIR"
                && value.as_ref() == Some(&OsString::from("/tmp/shared-target"))
        }));
    }

    #[test]
    fn build_commands_do_not_force_shared_target_dir_by_default() {
        let build_target = BuildTarget::from_cli(None).expect("default target should resolve");
        let command = prepare_cargo_compile_command(
            Path::new("/tmp/demo-agent"),
            "build",
            &build_target,
            CargoCompileProfile::Release,
            None,
        );

        assert!(!command.get_envs().any(|(key, _)| key == "CARGO_TARGET_DIR"));
    }
}
