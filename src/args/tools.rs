//! CLI parser definition for `cargo ai tools`.
use clap::{Arg, ArgAction, ArgGroup, Command};

/// Builds the `tools` command schema and nested subcommands.
pub fn command() -> Command {
    Command::new("tools")
        .about("Manage Cargo AI tool artifacts and contracts")
        .subcommand(
            Command::new("build")
                .about("Build a source-backed tool with the Cargo dev profile and materialize its managed artifact")
                .arg(
                    Arg::new("name")
                        .help("Tool name")
                        .required(true)
                        .value_name("NAME"),
                )
                .arg(
                    Arg::new("target")
                        .long("target")
                        .help("Rust target triple to pass through to cargo build")
                        .value_name("TRIPLE")
                        .num_args(1),
                ),
        )
        .subcommand(
            Command::new("describe")
                .about("Print a tool's machine-readable describe contract")
                .arg(
                    Arg::new("name")
                        .help("Tool name")
                        .required(true)
                        .value_name("NAME"),
                )
                .arg(
                    Arg::new("target")
                        .long("target")
                        .help("Target triple to use when resolving a materialized artifact")
                        .value_name("TRIPLE")
                        .num_args(1),
                ),
        )
        .subcommand(
            Command::new("lint")
                .about("Statically lint a project-local source-backed tool scaffold")
                .arg(
                    Arg::new("name")
                        .help("Tool name")
                        .required(true)
                        .value_name("NAME"),
                ),
        )
        .subcommand(
            Command::new("check")
                .about("Validate tool contract readiness")
                .group(
                    ArgGroup::new("check_target")
                        .args(["name", "config"])
                        .required(true),
                )
                .arg(Arg::new("name").help("Tool name").value_name("NAME"))
                .arg(
                    Arg::new("config")
                        .long("config")
                        .short('c')
                        .help(
                            "Agent definition JSON file whose referenced tools should be validated",
                        )
                        .value_name("FILE")
                        .num_args(1),
                )
                .arg(
                    Arg::new("target")
                        .long("target")
                        .help("Target triple to use when resolving a materialized artifact")
                        .value_name("TRIPLE")
                        .num_args(1),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .help("Reserved for future structured check output")
                        .hide(true)
                        .action(ArgAction::SetTrue),
                ),
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn build_supports_target() {
        let matches = super::command()
            .try_get_matches_from([
                "tools",
                "build",
                "render_cover_image",
                "--target",
                "aarch64-apple-darwin",
            ])
            .expect("tools build should parse");

        let build = matches
            .subcommand_matches("build")
            .expect("build subcommand should be available");
        assert_eq!(
            build.get_one::<String>("name").map(String::as_str),
            Some("render_cover_image")
        );
        assert_eq!(
            build.get_one::<String>("target").map(String::as_str),
            Some("aarch64-apple-darwin")
        );
    }

    #[test]
    fn check_accepts_config_without_tool_name() {
        let matches = super::command()
            .try_get_matches_from(["tools", "check", "--config", "./agent.json"])
            .expect("tools check --config should parse");

        let check = matches
            .subcommand_matches("check")
            .expect("check subcommand should be available");
        assert_eq!(
            check.get_one::<String>("config").map(String::as_str),
            Some("./agent.json")
        );
        assert!(check.get_one::<String>("name").is_none());
    }

    #[test]
    fn lint_requires_tool_name() {
        let matches = super::command()
            .try_get_matches_from(["tools", "lint", "hello_tool"])
            .expect("tools lint should parse");

        let lint = matches
            .subcommand_matches("lint")
            .expect("lint subcommand should be available");
        assert_eq!(
            lint.get_one::<String>("name").map(String::as_str),
            Some("hello_tool")
        );
    }
}
