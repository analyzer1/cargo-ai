//! CLI parser definition for `cargo ai build`.
use clap::{Arg, ArgAction, Command};

/// Builds the `build` command schema.
pub fn command() -> Command {
    Command::new("build")
        .about("Assemble a target-specific local build root from project-attached inputs")
        .arg(
            Arg::new("profile")
                .help("Input-selection profile from .cargo-ai/project.toml, not a Cargo compile profile (defaults to default)")
                .required(false)
                .value_name("PROFILE"),
        )
        .arg(
            Arg::new("target")
                .long("target")
                .help("Rust target triple to pass through to tool and agent builds")
                .value_name("TRIPLE")
                .num_args(1),
        )
        .arg(
            Arg::new("output_dir")
                .long("output-dir")
                .help(
                    "Destination directory for the assembled build root (defaults to target/cargo-ai/build/<profile>/<target>/release)",
                )
                .value_name("DIR")
                .num_args(1),
        )
        .arg(
            Arg::new("force")
                .long("force")
                .short('f')
                .help("Replace an existing explicit output directory before assembling the build")
                .required(false)
                .action(ArgAction::SetTrue),
        )
        .after_help(
            "The build profile is read from `.cargo-ai/project.toml` and must explicitly list:\n  - `agent_definitions`\n  - `hatched_agents`\n  - `tools`\n  - `assets`\n\nFinal agents and tools use the Cargo release profile. Optimization settings remain in Cargo manifests. Builds use project-attached tools only. Machine-only tools must be attached to the project before build succeeds.",
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn build_supports_profile_target_output_dir_and_force() {
        let matches = super::command()
            .try_get_matches_from([
                "build",
                "default",
                "--target",
                "aarch64-apple-darwin",
                "--output-dir",
                "./dist",
                "--force",
            ])
            .expect("build command should parse");

        assert_eq!(
            matches.get_one::<String>("profile").map(String::as_str),
            Some("default")
        );
        assert_eq!(
            matches.get_one::<String>("target").map(String::as_str),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            matches.get_one::<String>("output_dir").map(String::as_str),
            Some("./dist")
        );
        assert!(matches.get_flag("force"));
    }

    #[test]
    fn build_defaults_profile_when_omitted() {
        let matches = super::command()
            .try_get_matches_from(["build"])
            .expect("build command should parse without a profile");

        assert!(matches.get_one::<String>("profile").is_none());
        assert!(!matches.get_flag("force"));
    }
}
