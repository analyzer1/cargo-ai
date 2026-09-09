//! CLI parser definition for `cargo ai hatch`.
use clap::{Arg, ArgAction, ArgGroup, Command};

/// Builds the `hatch` command schema.
pub fn command() -> Command {
    Command::new("hatch")
        .about("Hatch a native agent from JSON using the Cargo release profile")
        .group(
            ArgGroup::new("explicit_definition_source")
                .args(["config", "json", "stdin"]),
        )
        .arg(
            Arg::new("name")
                .help(
                    "Agent name (or local .json config path shorthand when no explicit definition source flag is used)",
                )
                .required(true),
        )
        .arg(
            Arg::new("check")
                .long("check")
                .help("Validate scaffold and compile path with Cargo dev-profile `check` (no binary export)")
                .required(false)
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("from_account")
                .long("from-account")
                .help("Hatch NAME from your authenticated account instead of local resolution")
                .conflicts_with("owner_handle")
                .conflicts_with_all(["config", "json", "stdin"])
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("owner_handle")
                .long("owner-handle")
                .help("Hatch NAME from this public owner handle instead of local resolution")
                .value_name("HANDLE")
                .conflicts_with("from_account")
                .conflicts_with_all(["config", "json", "stdin"])
                .num_args(1),
        )
        .arg(
            Arg::new("definition_path")
                .long("definition-path")
                .help("Account-side definition namespace path to read from (defaults to '/'; not a local filesystem path)")
                .value_name("PATH")
                .conflicts_with_all(["config", "json", "stdin"])
                .num_args(1),
        )
        .arg(
            Arg::new("agent")
                .long("agent")
                .help("Remote/account agent name override (defaults to positional NAME)")
                .value_name("AGENT")
                .conflicts_with_all(["config", "json", "stdin"])
                .num_args(1),
        )
        .arg(
            Arg::new("config")
                .long("config")
                .short('c')
                .help("Path to agent definition JSON file")
                .value_name("FILE")
                .num_args(1),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .help("Agent definition JSON (raw JSON string)")
                .value_name("JSON")
                .num_args(1),
        )
        .arg(
            Arg::new("stdin")
                .long("stdin")
                .help("Read agent definition JSON from stdin")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("target")
                .long("target")
                .help("Rust target triple to pass through to cargo build/check")
                .value_name("TRIPLE")
                .num_args(1),
        )
        .arg(
            Arg::new("output_dir")
                .long("output-dir")
                .help(
                    "Destination directory for the exported binary (defaults to current directory)",
                )
                .value_name("DIR")
                .num_args(1),
        )
        .arg(
            Arg::new("force")
                .long("force")
                .short('f')
                .help("Overwrite existing output binary and replace any kept internal workspace")
                .required(false)
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("keep_project")
                .long("keep-project")
                .help("Preserve the internal hatched project workspace for inspection")
                .required(false)
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("ignore_tools")
                .long("ignore-tools")
                .help("Skip upfront tool contract checks while hatching or checking")
                .required(false)
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("allow_hosted_code")
                .long("allow-hosted-code")
                .help("Acknowledge that exported hosted package code will run outside package confinement")
                .required(false)
                .conflicts_with_all(["from_account", "owner_handle"])
                .action(ArgAction::SetTrue),
        )
        .after_help(
            "Definition sources:\n  - NAME by itself: agent name, registry name, or local .json shorthand\n  - --from-account: account agent NAME from your authenticated account\n  - --owner-handle <HANDLE>: public account agent NAME from another owner\n  - --agent <AGENT>: remote account source name when the local output name differs\n  - --config <FILE>: agent definition JSON file\n  - --json <JSON>: raw agent definition JSON string\n  - --stdin: read agent definition JSON from stdin\n\nWhen --config, --json, or --stdin is used, positional NAME is always the local output/project name.",
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn help_describes_definition_source_flags() {
        let mut command = super::command();
        let mut help = Vec::new();
        command
            .write_long_help(&mut help)
            .expect("hatch help should render");
        let help = String::from_utf8(help).expect("help should be utf8");

        assert!(help.contains("--config <FILE>"));
        assert!(help.contains("--json <JSON>"));
        assert!(help.contains("--stdin"));
        assert!(help.contains("--allow-hosted-code"));
        assert!(help.contains("Definition sources:"));
        assert!(help.contains("When --config, --json, or --stdin is used"));
    }
}
