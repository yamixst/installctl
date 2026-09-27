use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::apply;
use crate::changes::{self, Change};
use crate::cli;
use crate::control::{self, ActionController, Decision};
use crate::naming;
use crate::prereq::{self, UserInfo};
use crate::prompt;
use crate::report::{self, Review};
use crate::sandbox::{self, SandboxConfig};
use crate::staging::{self, StagingDir};
use crate::uninstall;

pub(crate) fn format_command(argv: &[String]) -> String {
    if argv.len() == 1 {
        argv[0].clone()
    } else {
        crate::actions::shell_quote(argv)
    }
}

fn do_run(args: cli::Args, user: &UserInfo, staging: &StagingDir) -> anyhow::Result<i32> {
    // 2. Copy exe and prepare sandbox configuration
    let cur_exe = if let Ok(cargo_bin) = std::env::var("CARGO_BIN_EXE_installctl") {
        PathBuf::from(cargo_bin)
    } else {
        std::fs::read_link("/proc/self/exe").or_else(|_| std::env::current_exe())?
    };

    let staged_exe = staging.run_dir().join("installctl");
    std::fs::copy(&cur_exe, &staged_exe)?;
    let mut perms = std::fs::metadata(&staged_exe)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&staged_exe, perms)?;

    let command = format_command(&args.command);

    let cwd = std::env::current_dir()?;
    let cfg = SandboxConfig {
        command,
        user: user.clone(),
        staging_upper: staging.upper(),
        run_dir: staging.run_dir(),
        mountpoint: staging.mountpoint(),
        exe: staged_exe.clone(),
        shim_targets: Vec::new(),
        cwd,
    };

    println!(
        "installctl: running the installer in a sandbox; nothing is changed on your system yet."
    );

    // 3. Set up prompter and controller, then run sandboxed
    let mut prompter = prompt::from_env()?;
    let (outcome, records) = {
        let mut controller = ActionController::new(&mut *prompter, Box::new(control::run_on_host));
        let outcome = sandbox::run_sandboxed(&cfg, &mut controller)?;
        (outcome, controller.records)
    };

    if outcome.aborted {
        for r in records
            .iter()
            .filter(|r| r.decision == Decision::RunForReal)
        {
            let code_str = match r.exit_code {
                Some(c) => format!("(exit {c})"),
                None => "(unknown exit)".to_string(),
            };
            prompter.say(&format!(
                "! ran {code_str}: {}",
                crate::actions::display(&r.action)
            ));
        }
        return Ok(1);
    }

    // 4. Collect changes; check if there is anything to install
    let changes = changes::collect(&outcome.overlay)?;
    let no_run_for_real = !records.iter().any(|r| r.decision == Decision::RunForReal);
    if changes.is_empty() && no_run_for_real {
        prompter.say("Nothing to install.");
        return Ok(0);
    }

    // 5. Render report and review changes
    let rendered_report = report::render(&changes, &records, &outcome.leftovers, outcome.exit_code);
    prompter.say(&rendered_report);

    let review = report::review(&changes, Path::new("/"), &mut *prompter);
    let applied: Vec<Change> = match review {
        Review::ApplyAll => changes,
        Review::ApplySubset(indices) => indices.into_iter().map(|i| changes[i].clone()).collect(),
        Review::Reject => {
            prompter.say("Nothing was changed.");
            return Ok(1);
        }
    };

    // 6. Check conflicts
    let conflicts = apply::check_conflicts(&applied, Path::new("/"));
    if !conflicts.is_empty() {
        for c in &conflicts {
            prompter.say(&format!("Conflict: {} ({})", c.path.display(), c.reason));
        }
        let ch = prompter.choose(
            "Conflicts detected. Proceed anyway?",
            &[('c', "continue"), ('a', "abort")],
        );
        if ch != 'c' {
            return Ok(1);
        }
    }

    // 7. Apply changes (partition by needs_root)
    let (root_part, user_part): (Vec<_>, Vec<_>) = applied
        .into_iter()
        .partition(|c| apply::needs_root(c, Path::new("/")));

    let mut root_originals = Vec::new();
    if !root_part.is_empty() {
        match apply::apply_root_part(&root_part, &staging.run_dir(), &staged_exe) {
            Ok(origs) => root_originals = origs,
            Err(_) => {
                prompter.say("Nothing was changed: could not get root rights.");
                return Ok(1);
            }
        }
    }

    let user_originals = apply::capture_originals(&user_part, Path::new("/"))?;
    apply::apply(&user_part, Path::new("/"), false)?;

    let mut all_applied = root_part;
    all_applied.extend(user_part);
    let mut all_originals = root_originals;
    all_originals.extend(user_originals);

    // 8. Program name and uninstall script
    let create_script =
        prompter.choose("Create an uninstall script?", &[('y', "yes"), ('n', "no")]);
    if create_script == 'y' {
        let path_dirs = naming::path_dirs(user);
        let candidates = naming::candidates(&all_applied, &path_dirs);
        let program_name = naming::choose_name(args.name.as_deref(), &candidates, &mut *prompter);
        let script_dir = uninstall::ask_location(&mut *prompter, &user.home);
        let uninstall_data =
            uninstall::build(&program_name, &all_applied, &all_originals, &records)?;
        let script_path = uninstall::write_script(&script_dir, uninstall_data, &mut *prompter)?;
        prompter.say(&format!(
            "Uninstall script written to {}",
            script_path.display()
        ));
    }

    // 9. Success
    Ok(0)
}

pub fn run(args: cli::Args) -> i32 {
    // 1. Prerequisites check and stale staging cleanup
    let user = match prereq::current_user() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("installctl: error: {e:#}");
            return 2;
        }
    };

    let state_home = staging::state_home(&user);
    let _ = staging::StagingDir::cleanup_stale(&state_home);

    let missing = prereq::check(&user);
    if !missing.is_empty() {
        for m in &missing {
            eprintln!("{}", prereq::explain(m));
        }
        return 3;
    }

    let staging = match staging::StagingDir::create(&state_home) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("installctl: error: {e:#}");
            return 2;
        }
    };

    let code = match do_run(args, &user, &staging) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("installctl: error: {e:#}");
            2
        }
    };

    let _ = staging.remove();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_command() {
        assert_eq!(
            format_command(&["echo 'hello world'".to_string()]),
            "echo 'hello world'"
        );
        assert_eq!(
            format_command(&["echo".to_string(), "arg with spaces".to_string()]),
            "echo 'arg with spaces'"
        );
        assert_eq!(
            format_command(&[
                "install.sh".to_string(),
                "prefix with space/dir".to_string(),
                "--flag".to_string(),
            ]),
            "install.sh 'prefix with space/dir' --flag"
        );
    }
}
