//! CLI parser definitions for `cargo ai packages`.
use clap::{Arg, ArgAction, ArgGroup, Command};

#[cfg(feature = "developer-tools")]
fn publish_command() -> Command {
    Command::new("publish")
        .about("Package the current project and publish it as an account package")
        .arg(
            Arg::new("profile")
                .help("Build/package profile to publish (defaults to 'default')")
                .required(false)
                .value_name("PROFILE")
                .num_args(1)
                .index(1),
        )
        .after_help(
            "Notes:\n  - `publish` packages the current Cargo AI project first, then uploads the resulting package archive.\n  - Project identity is taken from `.cargo-ai/project.toml` `[project].name` and `[project].version`.\n  - If PROFILE is omitted, `default` is used.",
        )
}

fn opaque_id(raw: &str) -> Result<String, String> {
    if raw.is_empty()
        || raw.len() > 128
        || !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("Expected 1 to 128 letters, digits, '-' or '_' characters".into());
    }
    Ok(raw.to_string())
}

fn hosted_selectors(command: Command, require_account: bool) -> Command {
    let mut source = Arg::new("source_id")
        .long("source-id")
        .num_args(1)
        .value_name("ID")
        .value_parser(opaque_id)
        .help("Stable hosted source identity, independent of display name");
    if require_account {
        source = source.requires("account");
    }
    command.arg(source).arg(
        Arg::new("version_id")
            .long("version-id")
            .num_args(1)
            .value_name("ID")
            .requires("source_id")
            .conflicts_with("version")
            .value_parser(opaque_id)
            .help("Immutable hosted version identity"),
    )
}

pub fn command() -> Command {
    let command = Command::new("packages")
        .about("Manage local and hosted packages")
        .subcommand(
            Command::new("list")
                .about("List installed local packages, or hosted packages with --account")
                .arg(
                    Arg::new("account")
                        .long("account")
                        .help("List hosted packages from your account, or from HANDLE when provided")
                        .required(false)
                        .value_name("HANDLE")
                        .num_args(0..=1)
                        .default_missing_value(""),
                )
                .arg(
                    Arg::new("include_archived")
                        .long("include-archived")
                        .help("Include archived hosted packages")
                        .requires("account")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .help("Maximum number of packages to display")
                        .required(false)
                        .value_name("N")
                        .num_args(1)
                        .value_parser(clap::value_parser!(u32).range(1..))
                        .conflicts_with("all"),
                )
                .arg(
                    Arg::new("all")
                        .long("all")
                        .help("Display all returned packages")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("limit"),
                ),
        )
        .subcommand(
            Command::new("install")
                .about("Install a local or hosted package into Cargo AI Home")
                .arg(
                    Arg::new("source")
                        .help("Local package root/archive/manifest path, or hosted package name when --account is present")
                        .required(false)
                        .value_name("SOURCE")
                        .num_args(1)
                        .index(1),
                )
                .arg(
                    Arg::new("account")
                        .long("account")
                        .help("Install a hosted package from your account, or from HANDLE when provided")
                        .required(false)
                        .value_name("HANDLE")
                        .num_args(0..=1)
                        .default_missing_value(""),
                )
                .arg(
                    Arg::new("version")
                        .long("version")
                        .help("Exact hosted package version to install (defaults to latest)")
                        .required(false)
                        .value_name("SEMVER")
                        .num_args(1),
                )
                .arg(
                    Arg::new("alias")
                        .long("as")
                        .help("Local package alias (defaults to the package name)")
                        .required(false)
                        .value_name("ALIAS")
                        .num_args(1),
                )
                .arg(
                    Arg::new("profile")
                        .long("profile")
                        .help("Current-project package profile to install when SOURCE is omitted")
                        .required(false)
                        .value_name("PROFILE")
                        .num_args(1),
                )
                .arg(
                    Arg::new("replace")
                        .long("replace")
                        .help("Replace same-version content or a different package identity at the alias")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("downgrade")
                        .long("downgrade")
                        .help("Allow installing an older version over the same package identity")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("accept_permissions")
                        .long("accept-permissions")
                        .help("Accept reviewed hosted package permissions for this install")
                        .requires("account")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("keep_data")
                        .long("keep-data")
                        .help("Transfer existing alias data during an explicit cross-source replacement")
                        .requires("replace")
                        .conflicts_with("delete_data")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("delete_data")
                        .long("delete-data")
                        .help("Delete existing alias data during an explicit replacement")
                        .requires("replace")
                        .conflicts_with("keep_data")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("update")
                .about("Update an installed hosted package alias to the latest eligible version")
                .arg(
                    Arg::new("alias")
                        .help("Installed hosted package alias")
                        .required(true)
                        .value_name("ALIAS")
                        .num_args(1)
                        .index(1),
                )
                .arg(
                    Arg::new("accept_permissions")
                        .long("accept-permissions")
                        .help("Accept reviewed permission expansion for this hosted update")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("rollback")
                .about("Switch an installed hosted package alias to an exact earlier version")
                .arg(
                    Arg::new("alias")
                        .help("Installed hosted package alias")
                        .required(true)
                        .value_name("ALIAS")
                        .num_args(1)
                        .index(1),
                )
                .arg(
                    Arg::new("to")
                        .long("to")
                        .help("Exact hosted package version to switch to")
                        .required(true)
                        .value_name("SEMVER")
                        .num_args(1),
                )
                .arg(
                    Arg::new("accept_permissions")
                        .long("accept-permissions")
                        .help("Accept reviewed permission expansion for this hosted rollback")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("inspect")
                .about("Inspect an installed local package")
                .arg(
                    Arg::new("alias")
                        .help("Installed package alias")
                        .required(true)
                        .value_name("ALIAS")
                        .num_args(1)
                        .index(1),
                ),
        )
        .subcommand(
            Command::new("uninstall")
                .about("Uninstall a local package alias")
                .arg(
                    Arg::new("alias")
                        .help("Installed package alias")
                        .required(true)
                        .value_name("ALIAS")
                        .num_args(1)
                        .index(1),
                )
                .arg(
                    Arg::new("delete_data")
                        .long("delete-data")
                        .help("Confirm deletion when the package alias has persistent data")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("pull")
                .about("Fetch a published package")
                .group(
                    ArgGroup::new("pull_name")
                        .args(["name", "name_positional"])
                        .required(true),
                )
                .arg(
                    Arg::new("name")
                        .long("name")
                        .help("Package name (explicit alias for positional NAME)")
                        .required(false)
                        .value_name("NAME")
                        .num_args(1),
                )
                .arg(
                    Arg::new("name_positional")
                        .help("Package name")
                        .required(false)
                        .value_name("NAME")
                        .num_args(1)
                        .index(1)
                        .conflicts_with("name"),
                )
                .arg(
                    Arg::new("owner_handle")
                        .long("owner-handle")
                        .help("Owner handle to pull from (omit to pull your own)")
                        .required(false)
                        .value_name("HANDLE")
                        .num_args(1),
                )
                .arg(
                    Arg::new("version")
                        .long("version")
                        .help("Exact published version to pull (defaults to latest)")
                        .required(false)
                        .value_name("SEMVER")
                        .num_args(1),
                )
                .arg(
                    Arg::new("output_dir")
                        .long("output-dir")
                        .help("Destination directory for the restored project (defaults to ./<name>)")
                        .required(false)
                        .value_name("DIR")
                        .num_args(1),
                )
                .arg(
                    Arg::new("force")
                        .long("force")
                        .help("Overwrite the destination directory if it already exists")
                        .required(false)
                        .action(clap::ArgAction::SetTrue),
                )
                .after_help(
                    "Notes:\n  - Name can be provided as positional NAME or via --name.\n  - If --version is omitted, the latest published package is restored.\n  - Default output: ./<name> (when --output-dir is omitted).\n  - --force applies only when writing to an existing destination directory.",
                ),
        )
        .subcommand(
            Command::new("visibility")
                .about("Set public visibility for a package")
                .group(
                    ArgGroup::new("visibility_state")
                        .args(["public", "private"])
                        .required(true),
                )
                .arg(
                    Arg::new("name")
                        .long("name")
                        .help("Package name")
                        .required(true)
                        .value_name("NAME")
                        .num_args(1),
                )
                .arg(
                    Arg::new("public")
                        .long("public")
                        .help("Set package visibility to public")
                        .required(false)
                        .conflicts_with("private")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("private")
                        .long("private")
                        .help("Set package visibility to private")
                        .required(false)
                        .conflicts_with("public")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("archive")
                .about("Archive or unarchive a package")
                .group(
                    ArgGroup::new("archive_state")
                        .args(["archive", "unarchive"])
                        .required(true),
                )
                .arg(
                    Arg::new("name")
                        .long("name")
                        .help("Package name")
                        .required(true)
                        .value_name("NAME")
                        .num_args(1),
                )
                .arg(
                    Arg::new("archive")
                        .long("archive")
                        .help("Archive the package")
                        .required(false)
                        .conflicts_with("unarchive")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("unarchive")
                        .long("unarchive")
                        .help("Unarchive the package")
                        .required(false)
                        .conflicts_with("archive")
                        .action(ArgAction::SetTrue),
                ),
        );

    let command = command
        .mut_subcommand("install", |c| hosted_selectors(c, true))
        .mut_subcommand("inspect", |c| {
            hosted_selectors(c, true)
                .about("Inspect an installed alias or a hosted package before installation")
                .mut_arg("alias", |a| {
                    a.required(false).required_unless_present("account")
                })
                .arg(
                    Arg::new("account")
                        .long("account")
                        .num_args(0..=1)
                        .default_missing_value("")
                        .value_name("HANDLE"),
                )
                .arg(
                    Arg::new("version")
                        .long("version")
                        .num_args(1)
                        .requires("account")
                        .value_name("SEMVER"),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .requires("account")
                        .action(ArgAction::SetTrue),
                )
        })
        .mut_subcommand("pull", |c| {
            hosted_selectors(c, false)
                .mut_group("pull_name", |g| g.required(false))
                .arg(
                    Arg::new("account")
                        .long("account")
                        .num_args(0..=1)
                        .default_missing_value("")
                        .conflicts_with("owner_handle"),
                )
        })
        .subcommand(
            Command::new("rename")
                .about("Rename a hosted source without changing its stable identity")
                .arg(
                    Arg::new("source_id")
                        .long("source-id")
                        .num_args(1)
                        .required(true)
                        .value_parser(opaque_id),
                )
                .arg(Arg::new("name").long("name").num_args(1).required(true)),
        );

    #[cfg(feature = "developer-tools")]
    let command = command.subcommand(
        publish_command().arg(
            Arg::new("source_id")
                .long("source-id")
                .num_args(1)
                .value_parser(opaque_id)
                .help("Stable source to publish after a display rename"),
        ),
    );

    command
}

#[cfg(test)]
mod tests {
    #[test]
    fn inspection_and_immutable_selectors_preserve_local_commands() {
        super::command().debug_assert();
        for args in [
            vec!["packages", "inspect", "local_alias"],
            vec![
                "packages",
                "inspect",
                "example",
                "--account",
                "alice",
                "--json",
            ],
            vec![
                "packages",
                "inspect",
                "--account",
                "--source-id",
                "source",
                "--version-id",
                "version",
            ],
            vec![
                "packages",
                "install",
                "--account",
                "--source-id",
                "source",
                "--version-id",
                "version",
                "--as",
                "local_alias",
            ],
            vec![
                "packages",
                "pull",
                "--source-id",
                "source",
                "--version-id",
                "version",
            ],
            vec![
                "packages",
                "rename",
                "--source-id",
                "source",
                "--name",
                "new_name",
            ],
        ] {
            super::command()
                .try_get_matches_from(args.clone())
                .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        for args in [
            vec!["packages", "inspect"],
            vec!["packages", "inspect", "local_alias", "--json"],
            vec!["packages", "inspect", "--source-id", "source"],
            vec![
                "packages",
                "inspect",
                "--account",
                "--version-id",
                "version",
            ],
            vec![
                "packages",
                "inspect",
                "--account",
                "--source-id",
                "source",
                "--version-id",
                "version",
                "--version",
                "1.0.0",
            ],
            vec![
                "packages",
                "inspect",
                "--account",
                "--source-id",
                "../escape",
            ],
        ] {
            assert!(
                super::command().try_get_matches_from(args.clone()).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn list_defaults_to_local_without_account_selector() {
        let matches = super::command()
            .try_get_matches_from(["packages", "list"])
            .expect("packages list should parse");
        let list = matches
            .subcommand_matches("list")
            .expect("list should be available");

        assert!(list.get_one::<String>("account").is_none());
    }

    #[test]
    fn list_account_selector_accepts_optional_handle() {
        let own_matches = super::command()
            .try_get_matches_from(["packages", "list", "--account", "--limit", "20"])
            .expect("packages list --account should parse");
        let own_list = own_matches
            .subcommand_matches("list")
            .expect("list should be available");

        assert_eq!(
            own_list.get_one::<String>("account").map(String::as_str),
            Some("")
        );
        assert_eq!(own_list.get_one::<u32>("limit").copied(), Some(20));

        let handle_matches = super::command()
            .try_get_matches_from(["packages", "list", "--account", "alice"])
            .expect("packages list --account handle should parse");
        let handle_list = handle_matches
            .subcommand_matches("list")
            .expect("list should be available");

        assert_eq!(
            handle_list.get_one::<String>("account").map(String::as_str),
            Some("alice")
        );
    }

    #[test]
    fn include_archived_requires_hosted_account_listing() {
        let error = super::command()
            .try_get_matches_from(["packages", "list", "--include-archived"])
            .expect_err("local listing must not silently ignore --include-archived");
        assert!(error.to_string().contains("--account"));

        super::command()
            .try_get_matches_from(["packages", "list", "--account", "--include-archived"])
            .expect("hosted listing should accept --include-archived");
    }

    #[test]
    fn install_supports_local_source_profile_alias_and_safety_flags() {
        let matches = super::command()
            .try_get_matches_from([
                "packages",
                "install",
                "./pkg",
                "--profile",
                "release",
                "--as",
                "sales_stable",
                "--replace",
                "--downgrade",
            ])
            .expect("packages install should parse");
        let install = matches
            .subcommand_matches("install")
            .expect("install should be available");

        assert_eq!(
            install.get_one::<String>("source").map(String::as_str),
            Some("./pkg")
        );
        assert_eq!(
            install.get_one::<String>("profile").map(String::as_str),
            Some("release")
        );
        assert_eq!(
            install.get_one::<String>("alias").map(String::as_str),
            Some("sales_stable")
        );
        assert!(install.get_flag("replace"));
        assert!(install.get_flag("downgrade"));
    }

    #[test]
    fn install_supports_hosted_account_version_and_alias() {
        let own_matches = super::command()
            .try_get_matches_from([
                "packages",
                "install",
                "data_integration",
                "--account",
                "--as",
                "data",
            ])
            .expect("hosted self install should parse");
        let own_install = own_matches
            .subcommand_matches("install")
            .expect("install should be available");
        assert_eq!(
            own_install.get_one::<String>("account").map(String::as_str),
            Some("")
        );
        assert_eq!(
            own_install.get_one::<String>("alias").map(String::as_str),
            Some("data")
        );

        let version_matches = super::command()
            .try_get_matches_from([
                "packages",
                "install",
                "data_integration",
                "--account",
                "alice",
                "--version",
                "1.2.3",
                "--as",
                "data",
            ])
            .expect("hosted handle install should parse");
        let version_install = version_matches
            .subcommand_matches("install")
            .expect("install should be available");
        assert_eq!(
            version_install
                .get_one::<String>("account")
                .map(String::as_str),
            Some("alice")
        );
        assert_eq!(
            version_install
                .get_one::<String>("version")
                .map(String::as_str),
            Some("1.2.3")
        );
    }

    #[test]
    fn update_and_rollback_parse_hosted_alias_commands() {
        let update_matches = super::command()
            .try_get_matches_from(["packages", "update", "data", "--accept-permissions"])
            .expect("packages update should parse");
        let update = update_matches
            .subcommand_matches("update")
            .expect("update should be available");
        assert_eq!(
            update.get_one::<String>("alias").map(String::as_str),
            Some("data")
        );
        assert!(update.get_flag("accept_permissions"));

        let rollback_matches = super::command()
            .try_get_matches_from([
                "packages",
                "rollback",
                "data",
                "--to",
                "1.0.0",
                "--accept-permissions",
            ])
            .expect("packages rollback should parse");
        let rollback = rollback_matches
            .subcommand_matches("rollback")
            .expect("rollback should be available");
        assert_eq!(
            rollback.get_one::<String>("alias").map(String::as_str),
            Some("data")
        );
        assert_eq!(
            rollback.get_one::<String>("to").map(String::as_str),
            Some("1.0.0")
        );
        assert!(rollback.get_flag("accept_permissions"));
    }

    #[test]
    fn permission_acceptance_is_hosted_only_for_install() {
        let error = super::command()
            .try_get_matches_from(["packages", "install", "./pkg", "--accept-permissions"])
            .expect_err("local install must not accept hosted permissions");

        assert!(error.to_string().contains("--account"));
    }

    #[test]
    fn replacement_and_uninstall_data_flags_are_deliberate() {
        let replacement = super::command()
            .try_get_matches_from([
                "packages",
                "install",
                "demo",
                "--account",
                "--as",
                "demo",
                "--replace",
                "--keep-data",
            ])
            .expect("explicit replacement data transfer should parse");
        assert!(replacement
            .subcommand_matches("install")
            .expect("install should be available")
            .get_flag("keep_data"));

        let missing_replace = super::command()
            .try_get_matches_from(["packages", "install", "./pkg", "--keep-data"])
            .expect_err("data transfer must require explicit replacement");
        assert!(missing_replace.to_string().contains("--replace"));
        let conflicting = super::command()
            .try_get_matches_from([
                "packages",
                "install",
                "./pkg",
                "--replace",
                "--keep-data",
                "--delete-data",
            ])
            .expect_err("replacement data dispositions must conflict");
        assert!(conflicting.to_string().contains("cannot be used"));

        let uninstall = super::command()
            .try_get_matches_from(["packages", "uninstall", "demo", "--delete-data"])
            .expect("explicit uninstall data deletion should parse");
        assert!(uninstall
            .subcommand_matches("uninstall")
            .expect("uninstall should be available")
            .get_flag("delete_data"));
    }
}
