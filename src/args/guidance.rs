//! Local, installed guidance lifecycle commands.
use clap::Command;

pub fn command() -> Command {
    Command::new("guidance")
        .about("Inspect or explicitly update installed project guidance offline")
        .subcommand(
            Command::new("status")
                .about("Read guidance ownership and update status without changing files"),
        )
        .subcommand(
            Command::new("update")
                .about("Update unchanged managed guidance from this installed Cargo AI binary"),
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn guidance_lifecycle_parses_only_status_and_flagless_update() {
        for subcommand in ["status", "update"] {
            let parsed = super::command()
                .try_get_matches_from(["guidance", subcommand])
                .unwrap();
            assert_eq!(parsed.subcommand_name(), Some(subcommand));
        }
        for args in [
            vec!["guidance", "repair"],
            vec!["guidance", "update", "--style", "codex"],
            vec!["guidance", "update", "--force"],
            vec!["guidance", "status", "--write"],
        ] {
            assert!(super::command().try_get_matches_from(args).is_err());
        }
    }
}
