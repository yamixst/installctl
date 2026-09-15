use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

pub fn current_user() -> anyhow::Result<UserInfo> {
    let uid = nix::unistd::getuid();
    let user = nix::unistd::User::from_uid(uid)?
        .ok_or_else(|| anyhow::anyhow!("current user with UID {uid} not found in passwd database"))?;
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or(user.dir);
    Ok(UserInfo {
        name: user.name,
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
        home,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    DevFuse,
    NewUidMap,
    NewGidMap,
    SubUid,
    SubGid,
    UserNamespaces(String),
    AppArmorRestriction,
}

pub fn explain(m: &Missing) -> String {
    match m {
        Missing::DevFuse => {
            "/dev/fuse is missing or inaccessible. Ensure the fuse module is loaded and /dev/fuse has read/write permissions.".to_string()
        }
        Missing::NewUidMap => {
            "newuidmap executable not found in PATH. Install the uidmap package (e.g. 'apt-get install uidmap').".to_string()
        }
        Missing::NewGidMap => {
            "newgidmap executable not found in PATH. Install the uidmap package (e.g. 'apt-get install uidmap').".to_string()
        }
        Missing::SubUid => {
            "No subordinate UID range configured in /etc/subuid for the current user. Configure at least 65535 subuids (e.g. '<user>:100000:65536').".to_string()
        }
        Missing::SubGid => {
            "No subordinate GID range configured in /etc/subgid for the current user. Configure at least 65535 subgids (e.g. '<user>:100000:65536').".to_string()
        }
        Missing::UserNamespaces(err) => {
            format!("Unprivileged user namespaces cannot be created: {err}")
        }
        Missing::AppArmorRestriction => {
            "Unprivileged user namespaces are restricted by AppArmor (kernel.apparmor_restrict_unprivileged_userns = 1). Disable the restriction via sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 or provide an AppArmor profile.".to_string()
        }
    }
}

fn find_in_path(cmd: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(cmd);
            if let Ok(meta) = candidate.metadata() {
                use std::os::unix::fs::PermissionsExt;
                if meta.is_file() && (meta.permissions().mode() & 0o111 != 0) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn check_apparmor_restricted() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

fn probe_userns(uid: u32) -> Result<(), String> {
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{fork, pipe, ForkResult};
    use std::io::{Read, Write};

    let (pipe_read_fd, pipe_write_fd) = pipe().map_err(|e| format!("pipe failed: {e}"))?;

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            drop(pipe_write_fd);
            let mut err_msg = String::new();
            let mut reader = std::fs::File::from(pipe_read_fd);
            let _ = reader.read_to_string(&mut err_msg);

            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, 0)) => Ok(()),
                Ok(WaitStatus::Exited(_, code)) => {
                    if err_msg.trim().is_empty() {
                        Err(format!("probe process exited with code {code}"))
                    } else {
                        Err(err_msg.trim().to_string())
                    }
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    Err(format!("probe process killed by signal {sig:?}"))
                }
                Ok(status) => Err(format!("probe process unexpected status: {status:?}")),
                Err(e) => Err(format!("waitpid failed: {e}")),
            }
        }
        Ok(ForkResult::Child) => {
            drop(pipe_read_fd);
            let mut writer = std::fs::File::from(pipe_write_fd);

            let run_probe = || -> Result<(), String> {
                nix::sched::unshare(
                    nix::sched::CloneFlags::CLONE_NEWUSER | nix::sched::CloneFlags::CLONE_NEWNS,
                )
                .map_err(|e| format!("unshare failed: {e}"))?;

                let mut uid_map = std::fs::OpenOptions::new()
                    .write(true)
                    .open("/proc/self/uid_map")
                    .map_err(|e| format!("open /proc/self/uid_map failed: {e}"))?;
                uid_map
                    .write_all(format!("0 {uid} 1\n").as_bytes())
                    .map_err(|e| format!("write /proc/self/uid_map failed: {e}"))?;
                drop(uid_map);

                nix::mount::mount(
                    Some("tmpfs"),
                    "/tmp",
                    Some("tmpfs"),
                    nix::mount::MsFlags::empty(),
                    None::<&str>,
                )
                .map_err(|e| format!("mount tmpfs on /tmp failed: {e}"))?;

                Ok(())
            };

            match run_probe() {
                Ok(()) => {
                    drop(writer);
                    unsafe { libc::_exit(0) };
                }
                Err(err) => {
                    let _ = writer.write_all(err.as_bytes());
                    drop(writer);
                    unsafe { libc::_exit(1) };
                }
            }
        }
        Err(e) => Err(format!("fork failed: {e}")),
    }
}

pub fn check(user: &UserInfo) -> Vec<Missing> {
    let mut missing = Vec::new();

    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_err()
    {
        missing.push(Missing::DevFuse);
    }

    if find_in_path("newuidmap").is_none() {
        missing.push(Missing::NewUidMap);
    }

    if find_in_path("newgidmap").is_none() {
        missing.push(Missing::NewGidMap);
    }

    let subuid_ok = match std::fs::read_to_string("/etc/subuid") {
        Ok(content) => match crate::sandbox::idmap::parse_subid(&content, &user.name, user.uid) {
            Some((start, count)) => {
                crate::sandbox::idmap::compute_map(user.uid, start, count).is_ok()
            }
            None => false,
        },
        Err(_) => false,
    };
    if !subuid_ok {
        missing.push(Missing::SubUid);
    }

    let subgid_ok = match std::fs::read_to_string("/etc/subgid") {
        Ok(content) => match crate::sandbox::idmap::parse_subid(&content, &user.name, user.gid) {
            Some((start, count)) => {
                crate::sandbox::idmap::compute_map(user.gid, start, count).is_ok()
            }
            None => false,
        },
        Err(_) => false,
    };
    if !subgid_ok {
        missing.push(Missing::SubGid);
    }

    if let Err(err) = probe_userns(user.uid) {
        if check_apparmor_restricted() {
            missing.push(Missing::AppArmorRestriction);
        } else {
            missing.push(Missing::UserNamespaces(err));
        }
    }

    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_explain_apparmor() {
        let msg = explain(&Missing::AppArmorRestriction);
        assert!(
            msg.contains("kernel.apparmor_restrict_unprivileged_userns"),
            "explain missing sysctl: {msg}"
        );
    }

    #[test]
    fn test_explain_newuidmap() {
        let msg = explain(&Missing::NewUidMap);
        assert!(
            msg.contains("uidmap"),
            "explain missing uidmap: {msg}"
        );
    }

    #[test]
    fn test_current_user() {
        let user = current_user().expect("current_user should succeed");
        assert!(!user.name.is_empty());
        assert!(user.uid > 0 || user.name == "root");
    }

    #[test]
    fn test_probe_userns() {
        let user = current_user().expect("current_user should succeed");
        let res = probe_userns(user.uid);
        // On this host unshare works, so res should be Ok
        assert!(res.is_ok(), "probe_userns failed: {res:?}");
    }
}
