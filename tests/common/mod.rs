#![allow(dead_code)]

use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn is_executable(p: &Path) -> bool {
    if let Ok(meta) = p.metadata() {
        meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
    } else {
        false
    }
}

fn find_in_path(cmd: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(cmd);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn check_fuse() -> Result<(), String> {
    match OpenOptions::new().read(true).write(true).open("/dev/fuse") {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("cannot open /dev/fuse: {e}")),
    }
}

fn check_newuidmap() -> Result<(), String> {
    if find_in_path("newuidmap").is_some() {
        Ok(())
    } else {
        Err("newuidmap not found on PATH".to_string())
    }
}

fn check_userns_allowed() -> Result<(), String> {
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
    if restricted {
        Err("unprivileged user namespaces are restricted by AppArmor".to_string())
    } else {
        Ok(())
    }
}

pub fn priv_env() -> bool {
    let errors: Vec<String> = [check_fuse(), check_newuidmap(), check_userns_allowed()]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if errors.is_empty() {
        return true;
    }
    let reason = errors.join("; ");

    if std::env::var("INSTALLCTL_REQUIRE_PRIV_TESTS").as_deref() == Ok("1") {
        panic!("{reason}");
    } else {
        eprintln!("skipping: {reason}");
        false
    }
}

pub fn sudo_env() -> bool {
    let reason = match Command::new("sudo").args(["-n", "true"]).status() {
        Ok(status) if status.success() => return true,
        Ok(status) => format!("sudo -n true exited with {status}"),
        Err(e) => format!("failed to execute sudo: {e}"),
    };

    if std::env::var("INSTALLCTL_REQUIRE_PRIV_TESTS").as_deref() == Ok("1") {
        panic!("{reason}");
    } else {
        eprintln!("skipping: {reason}");
        false
    }
}
