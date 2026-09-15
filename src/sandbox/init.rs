use std::ffi::CString;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{chdir, fork, pivot_root, setgroups, setresgid, setresuid, ForkResult, Gid, Uid};

use crate::ipc::{self, InitEvent, Leftover};
use crate::overlay::fuse::mount_fuse_fd;
use crate::sandbox::SandboxConfig;

fn run_init_inner(cfg: &SandboxConfig, ctl_i: &UnixStream) -> anyhow::Result<()> {
    // Step 3:
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )?;

    let fuse_fd = mount_fuse_fd(&cfg.mountpoint)?;
    ipc::send_msg(ctl_i, &InitEvent::FuseFd, &[fuse_fd.as_fd()])?;
    drop(fuse_fd);

    // Step 4:
    let m = &cfg.mountpoint;
    mount(
        Some("proc"),
        &m.join("proc"),
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )?;

    mount(
        Some("/dev"),
        &m.join("dev"),
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )?;

    mount(
        Some("/sys"),
        &m.join("sys"),
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )?;

    mount(
        Some("tmpfs"),
        &m.join("tmp"),
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=1777"),
    )?;

    let _ = std::fs::create_dir_all(m.join("var/tmp"));
    mount(
        Some("tmpfs"),
        &m.join("var/tmp"),
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=1777"),
    )?;

    mount(
        Some(&cfg.run_dir),
        &m.join("run/installctl"),
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )?;

    for target in &cfg.shim_targets {
        let rel = target.strip_prefix("/").unwrap_or(target);
        let target_in_m = m.join(rel);
        if target_in_m.exists() {
            mount(
                Some(&cfg.exe),
                &target_in_m,
                None::<&str>,
                MsFlags::MS_BIND,
                None::<&str>,
            )?;
            mount(
                None::<&str>,
                &target_in_m,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
                None::<&str>,
            )?;
        }
    }

    chdir(m)?;
    pivot_root(".", ".")?;
    umount2(".", MntFlags::MNT_DETACH)?;

    if chdir(&cfg.cwd).is_err() {
        let _ = chdir("/");
    }

    // Step 5:
    ipc::send_msg(ctl_i, &InitEvent::Ready, &[])?;

    match unsafe { fork()? } {
        ForkResult::Child => {
            let _ = setgroups(&[]);
            let gid = Gid::from_raw(cfg.user.gid);
            let uid = Uid::from_raw(cfg.user.uid);
            let _ = setresgid(gid, gid, gid);
            let _ = setresuid(uid, uid, uid);

            let old_path = std::env::var("PATH").unwrap_or_default();
            let new_path = format!("/run/installctl/bin:{}", old_path);
            unsafe {
                std::env::set_var("PATH", new_path);
                std::env::set_var("HOME", &cfg.user.home);
                std::env::set_var("USER", &cfg.user.name);
                std::env::set_var("LOGNAME", &cfg.user.name);
            }

            let sh = CString::new("/bin/sh").unwrap();
            let c_flag = CString::new("-c").unwrap();
            let cmd = CString::new(cfg.command.as_str()).unwrap();

            let _ = nix::unistd::execv(&sh, &[sh.as_c_str(), c_flag.as_c_str(), cmd.as_c_str()]);
            unsafe { libc::_exit(127) };
        }
        ForkResult::Parent { child: main_pid } => {
            // Step 6:
            let mut exit_code = 1;
            loop {
                match waitpid(None, None) {
                    Ok(WaitStatus::Exited(pid, code)) => {
                        if pid == main_pid {
                            exit_code = code;
                            break;
                        }
                    }
                    Ok(WaitStatus::Signaled(pid, sig, _)) => {
                        if pid == main_pid {
                            exit_code = 128 + sig as i32;
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(nix::errno::Errno::ECHILD) => {
                        break;
                    }
                    Err(_) => {
                        break;
                    }
                }
            }

            let mut leftovers = Vec::new();
            if let Ok(entries) = std::fs::read_dir("/proc") {
                for entry in entries.flatten() {
                    if let Ok(name) = entry.file_name().into_string()
                        && let Ok(pid) = name.parse::<i32>()
                        && pid != 1
                        && pid != main_pid.as_raw()
                    {
                        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
                            .map(|data| {
                                data.split(|&b| b == 0)
                                    .filter(|slice| !slice.is_empty())
                                    .map(|s| String::from_utf8_lossy(s))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default();
                        let cmdline = if cmdline.trim().is_empty() {
                            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                                .unwrap_or_default()
                                .trim()
                                .to_string()
                        } else {
                            cmdline.trim().to_string()
                        };
                        leftovers.push(Leftover { pid, cmdline });
                    }
                }
            }
            leftovers.sort_by_key(|l| l.pid);

            let _ = ipc::send_msg(ctl_i, &InitEvent::Finished { exit_code, leftovers }, &[]);
            unsafe {
                libc::kill(-1, libc::SIGKILL);
                libc::_exit(0);
            }
        }
    }
}

pub fn run_init(cfg: &SandboxConfig, ctl_i: UnixStream) -> ! {
    if let Err(e) = run_init_inner(cfg, &ctl_i) {
        eprintln!("init failed: {e:?}");
    }
    unsafe { libc::_exit(1) };
}
