use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "installctl",
    version,
    about = "Safely run software installers in an unprivileged sandbox"
)]
pub struct Args {
    #[arg(long, help = "Program name")]
    pub name: Option<String>,

    #[arg(long, hide = true, conflicts_with = "command")]
    pub apply_helper: Option<PathBuf>,

    #[arg(trailing_var_arg = true, required_unless_present = "apply_helper")]
    pub command: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_with_name() {
        let args = Args::try_parse_from(["installctl", "--name", "myapp", "sh", "install.sh"]).unwrap();
        assert_eq!(args.name.as_deref(), Some("myapp"));
        assert_eq!(args.command, vec!["sh", "install.sh"]);
        assert!(args.apply_helper.is_none());
    }

    #[test]
    fn parse_apply_helper() {
        let args = Args::try_parse_from(["installctl", "--apply-helper", "/tmp/plan.json"]).unwrap();
        assert_eq!(args.apply_helper, Some(PathBuf::from("/tmp/plan.json")));
        assert!(args.command.is_empty());
    }

    #[test]
    fn parse_command_with_flags() {
        let args = Args::try_parse_from(["installctl", "sh", "-c", "echo test"]).unwrap();
        assert_eq!(args.command, vec!["sh", "-c", "echo test"]);
    }
}
