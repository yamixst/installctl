use crate::prereq::UserInfo;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct StagingDir {
    pub root: PathBuf,
}

impl StagingDir {
    pub fn create(state_home: &Path) -> io::Result<StagingDir> {
        let runs_dir = state_home.join("runs");
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let pid = std::process::id();

        let mut root = runs_dir.join(format!("{now_secs}-{pid}"));
        let mut counter = 1;
        while root.exists() {
            root = runs_dir.join(format!("{now_secs}-{pid}-{counter}"));
            counter += 1;
        }

        std::fs::create_dir_all(root.join("upper"))?;
        std::fs::create_dir_all(root.join("run/bin"))?;
        std::fs::create_dir_all(root.join("mnt"))?;
        std::fs::write(root.join("owner.pid"), pid.to_string())?;

        Ok(StagingDir { root })
    }

    pub fn upper(&self) -> PathBuf {
        self.root.join("upper")
    }

    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn mountpoint(&self) -> PathBuf {
        self.root.join("mnt")
    }

    pub fn remove(self) -> io::Result<()> {
        std::fs::remove_dir_all(&self.root)
    }

    pub fn cleanup_stale(state_home: &Path) -> io::Result<Vec<PathBuf>> {
        let runs_dir = state_home.join("runs");
        if !runs_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut removed = Vec::new();
        for entry in std::fs::read_dir(&runs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let pid_file = path.join("owner.pid");
            if let Ok(content) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = content.trim().parse::<i32>()
                && pid > 0
            {
                let is_alive = match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
                    Ok(()) => true,
                    Err(nix::errno::Errno::ESRCH) => false,
                    Err(_) => true,
                };
                if !is_alive && std::fs::remove_dir_all(&path).is_ok() {
                    removed.push(path);
                }
            }
        }
        removed.sort();
        Ok(removed)
    }
}

pub fn state_home(user: &UserInfo) -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(xdg).join("installctl")
    } else {
        user.home.join(".local/state/installctl")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_runs_are_removed_live_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let state_home = tmp.path();
        let runs_dir = state_home.join("runs");
        std::fs::create_dir_all(&runs_dir).unwrap();

        let live_dir = runs_dir.join("live-run");
        std::fs::create_dir_all(live_dir.join("upper")).unwrap();
        std::fs::write(live_dir.join("owner.pid"), std::process::id().to_string()).unwrap();

        let dead_child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = dead_child.id();
        let _ = dead_child.wait_with_output().unwrap();

        let dead_dir = runs_dir.join("dead-run");
        std::fs::create_dir_all(dead_dir.join("upper")).unwrap();
        std::fs::write(dead_dir.join("owner.pid"), dead_pid.to_string()).unwrap();

        let removed = StagingDir::cleanup_stale(state_home).unwrap();
        assert_eq!(removed, vec![dead_dir.clone()]);
        assert!(live_dir.exists());
        assert!(!dead_dir.exists());
    }
}
