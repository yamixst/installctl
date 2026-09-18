use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{chdir, fork, pivot_root, setgroups, setresgid, setresuid, ForkResult, Gid, Pid, Uid};

use crate::ipc::{self, ExitStatus, InitEvent, Leftover};
use crate::overlay::fuse::mount_fuse_fd;
use crate::sandbox::SandboxConfig;
use crate::shim::SudoRequest;

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

    let sudo_sock_path = "/run/installctl/sudo.sock";
    let _ = std::fs::remove_file(sudo_sock_path);
    let sudo_listener = std::os::unix::net::UnixListener::bind(sudo_sock_path)?;
    sudo_listener.set_nonblocking(true)?;

    let _ = std::fs::set_permissions(sudo_sock_path, std::fs::Permissions::from_mode(0o666));
    let _ = std::fs::set_permissions("/run/installctl", std::fs::Permissions::from_mode(0o777));

    // Step 5:
    ipc::send_msg(ctl_i, &InitEvent::Ready, &[])?;

    let main_pid = match unsafe { fork()? } {
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

            unsafe {
                let _ = nix::sys::signal::signal(
                    nix::sys::signal::Signal::SIGINT,
                    nix::sys::signal::SigHandler::SigDfl,
                );
            }

            let _ = nix::unistd::execv(&sh, &[sh.as_c_str(), c_flag.as_c_str(), cmd.as_c_str()]);
            unsafe { libc::_exit(127) };
        }
        ForkResult::Parent { child } => child,
    };

    // Step 6:
    extern "C" fn sigchld_handler(_: libc::c_int) {}
    unsafe {
        let sa = nix::sys::signal::SigAction::new(
            nix::sys::signal::SigHandler::Handler(sigchld_handler),
            nix::sys::signal::SaFlags::SA_NOCLDSTOP,
            nix::sys::signal::SigSet::empty(),
        );
        let _ = nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGCHLD, &sa);
    }

    struct InFlightSudo {
        conn: UnixStream,
    }
    let mut in_flight: HashMap<Pid, InFlightSudo> = HashMap::new();
    let mut main_exit_code: Option<i32> = None;

    let exit_code = loop {
        loop {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => {
                    if pid == main_pid {
                        main_exit_code = Some(code);
                    } else if let Some(inflight) = in_flight.remove(&pid) {
                        let _ = ipc::send_msg(&inflight.conn, &ExitStatus { code }, &[]);
                    }
                }
                Ok(WaitStatus::Signaled(pid, sig, _)) => {
                    let code = 128 + sig as i32;
                    if pid == main_pid {
                        main_exit_code = Some(code);
                    } else if let Some(inflight) = in_flight.remove(&pid) {
                        let _ = ipc::send_msg(&inflight.conn, &ExitStatus { code }, &[]);
                    }
                }
                Ok(WaitStatus::StillAlive) => {
                    break;
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

        if let Some(code) = main_exit_code
            && in_flight.is_empty()
        {
            break code;
        }

        let mut poll_fds = [PollFd::new(
            sudo_listener.as_fd(),
            PollFlags::POLLIN,
        )];
        let _ = poll(&mut poll_fds, PollTimeout::from(50u16));

        if let Some(revents) = poll_fds[0].revents()
            && revents.contains(PollFlags::POLLIN)
        {
            while let Ok((conn, _)) = sudo_listener.accept() {
                let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let _ = conn.set_write_timeout(Some(std::time::Duration::from_secs(5)));

                match ipc::recv_msg::<SudoRequest>(&conn) {
                    Ok((req, fds)) => {
                        if fds.len() < 3 {
                            eprintln!("sudo request received with fewer than 3 fds: {}", fds.len());
                            let _ = ipc::send_msg(&conn, &ExitStatus { code: 1 }, &[]);
                            continue;
                        }

                        match unsafe { fork() } {
                            Ok(ForkResult::Child) => {
                                if nix::unistd::chdir(&req.cwd).is_err() {
                                    let _ = nix::unistd::chdir("/");
                                }

                                if req.preserve_env {
                                    for (k, _) in std::env::vars() {
                                        unsafe { std::env::remove_var(k); }
                                    }
                                    for (k, v) in &req.env {
                                        unsafe { std::env::set_var(k, v); }
                                    }
                                } else {
                                    let term = req.env.iter().find(|(k, _)| k == "TERM").map(|(_, v)| v.clone());
                                    for (k, _) in std::env::vars() {
                                        unsafe { std::env::remove_var(k); }
                                    }
                                    unsafe {
                                        std::env::set_var("PATH", "/run/installctl/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
                                        std::env::set_var("HOME", "/root");
                                        std::env::set_var("USER", "root");
                                        std::env::set_var("LOGNAME", "root");
                                        if let Some(term_val) = term {
                                            std::env::set_var("TERM", term_val);
                                        }
                                    }
                                }

                                unsafe {
                                    libc::dup2(fds[0].as_raw_fd(), 0);
                                    libc::dup2(fds[1].as_raw_fd(), 1);
                                    libc::dup2(fds[2].as_raw_fd(), 2);
                                }

                                unsafe {
                                    let _ = nix::sys::signal::signal(
                                        nix::sys::signal::Signal::SIGINT,
                                        nix::sys::signal::SigHandler::SigDfl,
                                    );
                                }

                                drop(fds);
                                drop(conn);
                                drop(sudo_listener);

                                if req.argv.is_empty() {
                                    unsafe { libc::_exit(1) };
                                }

                                let c_cmd = CString::new(req.argv[0].as_str()).unwrap_or_default();
                                let c_args: Vec<CString> = req
                                    .argv
                                    .iter()
                                    .map(|s| CString::new(s.as_str()).unwrap_or_default())
                                    .collect();

                                let _ = nix::unistd::execvp(&c_cmd, &c_args);
                                unsafe { libc::_exit(127) };
                            }
                            Ok(ForkResult::Parent { child }) => {
                                drop(fds);
                                in_flight.insert(child, InFlightSudo { conn });
                            }
                            Err(e) => {
                                eprintln!("fork sudo child failed: {e}");
                                let _ = ipc::send_msg(&conn, &ExitStatus { code: 1 }, &[]);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("failed to receive sudo request: {e}");
                    }
                }
            }
        }
    };

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

pub fn run_init(cfg: &SandboxConfig, ctl_i: UnixStream) -> ! {
    if let Err(e) = run_init_inner(cfg, &ctl_i) {
        eprintln!("init failed: {e:?}");
    }
    unsafe { libc::_exit(1) };
}
