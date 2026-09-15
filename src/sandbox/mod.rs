use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use nix::fcntl::OFlag;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sched::CloneFlags;
use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, ForkResult, Gid, Pid, Uid};

use crate::ipc::{self, HostCommand, InitEvent, Leftover};
use crate::overlay::fuse::{new_fs, serve};
use crate::overlay::Overlay;
use crate::prereq::UserInfo;

pub mod idmap;
pub mod init;

pub const SANDBOX_RUN_DIR: &str = "/run/installctl";

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub command: String,
    pub user: UserInfo,
    pub staging_upper: PathBuf,
    pub run_dir: PathBuf,
    pub mountpoint: PathBuf,
    pub exe: PathBuf, /* copy of installctl in run_dir */
    pub shim_targets: Vec<PathBuf>, /* real paths to bind the exe over; empty in this task */
    pub cwd: PathBuf,
}

#[derive(Debug)]
pub struct SandboxOutcome {
    pub exit_code: i32,
    pub leftovers: Vec<Leftover>,
    pub overlay: Overlay,
    pub aborted: bool,
}

pub trait SandboxEvents {
    /// Called from the host loop for each accepted connection on run_dir/action.sock; returns Some(Kill) to abort.
    fn on_action_conn(&mut self, conn: UnixStream) -> Option<HostCommand>;
}

struct SigintGuard {
    old_handler: nix::sys::signal::SigHandler,
}

impl Drop for SigintGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = nix::sys::signal::signal(
                nix::sys::signal::Signal::SIGINT,
                self.old_handler,
            );
        }
    }
}

struct ChildGuard {
    pid: Pid,
    reaped: bool,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = nix::sys::signal::kill(self.pid, nix::sys::signal::Signal::SIGKILL);
            let _ = waitpid(self.pid, None);
        }
    }
}

pub fn run_sandboxed(
    cfg: &SandboxConfig,
    events: &mut dyn SandboxEvents,
) -> anyhow::Result<SandboxOutcome> {
    // 1. Ignore SIGINT in host P while the sandbox runs
    let old_sigint = unsafe {
        nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGINT,
            nix::sys::signal::SigHandler::SigIgn,
        )?
    };
    let _sigint_guard = SigintGuard {
        old_handler: old_sigint,
    };

    // 2. Parse subuids and subgids
    let subuid_content = std::fs::read_to_string("/etc/subuid")
        .map_err(|e| anyhow::anyhow!("cannot read /etc/subuid: {e}"))?;
    let (sub_uid_start, sub_uid_count) =
        idmap::parse_subid(&subuid_content, &cfg.user.name, cfg.user.uid)
            .ok_or_else(|| anyhow::anyhow!("no subordinate UID range for user {}", cfg.user.name))?;
    let uid_ranges = idmap::compute_map(cfg.user.uid, sub_uid_start, sub_uid_count)
        .map_err(|e| anyhow::anyhow!("failed to compute uid map: {e}"))?;

    let subgid_content = std::fs::read_to_string("/etc/subgid")
        .map_err(|e| anyhow::anyhow!("cannot read /etc/subgid: {e}"))?;
    let (sub_gid_start, sub_gid_count) =
        idmap::parse_subid(&subgid_content, &cfg.user.name, cfg.user.gid)
            .ok_or_else(|| anyhow::anyhow!("no subordinate GID range for user {}", cfg.user.name))?;
    let gid_ranges = idmap::compute_map(cfg.user.gid, sub_gid_start, sub_gid_count)
        .map_err(|e| anyhow::anyhow!("failed to compute gid map: {e}"))?;

    // 3. Prepare run_dir and action.sock listener
    std::fs::create_dir_all(&cfg.run_dir)?;
    let action_sock_path = cfg.run_dir.join("action.sock");
    let _ = std::fs::remove_file(&action_sock_path);
    let action_listener = std::os::unix::net::UnixListener::bind(&action_sock_path)
        .map_err(|e| anyhow::anyhow!("failed to bind action.sock at {:?}: {e}", action_sock_path))?;
    action_listener.set_nonblocking(true)?;

    // 4. Communication channels
    let (ctl_p, ctl_i) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::SOCK_CLOEXEC,
    )?;
    let ctl_p = std::os::unix::net::UnixStream::from(ctl_p);
    let ctl_i = std::os::unix::net::UnixStream::from(ctl_i);

    let (c_to_p_r, c_to_p_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
    let (p_to_c_r, p_to_c_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;

    // 5. Fork C
    let c_pid = match unsafe { fork()? } {
        ForkResult::Child => {
            drop(ctl_p);
            drop(c_to_p_r);
            drop(p_to_c_w);

            // Step 1: unshare(CLONE_NEWUSER)
            if let Err(e) = nix::sched::unshare(CloneFlags::CLONE_NEWUSER) {
                eprintln!("C unshare(CLONE_NEWUSER) failed: {e}");
                unsafe { libc::_exit(1) };
            }

            use std::io::{Read, Write};
            let mut writer = std::fs::File::from(c_to_p_w);
            if writer.write_all(&[1u8]).is_err() {
                unsafe { libc::_exit(1) };
            }

            let mut reader = std::fs::File::from(p_to_c_r);
            let mut ack = [0u8; 1];
            if reader.read_exact(&mut ack).is_err() {
                unsafe { libc::_exit(1) };
            }
            drop(reader);

            // Step 2: C: setresgid(0,0,0), setresuid(0,0,0), unshare(CLONE_NEWNS|CLONE_NEWPID)
            let gid0 = Gid::from_raw(0);
            let uid0 = Uid::from_raw(0);
            if nix::unistd::setresgid(gid0, gid0, gid0).is_err() {
                unsafe { libc::_exit(1) };
            }
            if nix::unistd::setresuid(uid0, uid0, uid0).is_err() {
                unsafe { libc::_exit(1) };
            }
            if nix::sched::unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID).is_err() {
                unsafe { libc::_exit(1) };
            }

            match unsafe { fork() } {
                Ok(ForkResult::Parent { child: i_pid }) => {
                    drop(ctl_i);
                    let raw_pid = i_pid.as_raw();
                    let _ = writer.write_all(&raw_pid.to_le_bytes());
                    drop(writer);

                    let code = match waitpid(i_pid, None) {
                        Ok(WaitStatus::Exited(_, c)) => c,
                        Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
                        _ => 1,
                    };
                    unsafe { libc::_exit(code) };
                }
                Ok(ForkResult::Child) => {
                    drop(writer);
                    init::run_init(cfg, ctl_i);
                }
                Err(e) => {
                    eprintln!("C fork I failed: {e}");
                    unsafe { libc::_exit(1) };
                }
            }
        }
        ForkResult::Parent { child } => child,
    };

    drop(ctl_i);
    drop(c_to_p_w);
    drop(p_to_c_r);

    let mut child_guard = ChildGuard {
        pid: c_pid,
        reaped: false,
    };

    // Host P: Step 1
    use std::io::{Read, Write};
    let mut c_to_p_file = std::fs::File::from(c_to_p_r);
    let mut sync_buf = [0u8; 1];
    c_to_p_file
        .read_exact(&mut sync_buf)
        .map_err(|e| anyhow::anyhow!("failed to read sync byte from C: {e}"))?;

    idmap::apply_map(c_pid.as_raw(), "newuidmap", &uid_ranges)?;
    idmap::apply_map(c_pid.as_raw(), "newgidmap", &gid_ranges)?;

    let mut p_to_c_file = std::fs::File::from(p_to_c_w);
    p_to_c_file
        .write_all(&[1u8])
        .map_err(|e| anyhow::anyhow!("failed to signal C: {e}"))?;
    drop(p_to_c_file);

    // Host P: Step 2: Read I's pid
    let mut i_pid_buf = [0u8; 4];
    c_to_p_file
        .read_exact(&mut i_pid_buf)
        .map_err(|e| anyhow::anyhow!("failed to read I's PID from C: {e}"))?;
    let i_pid = i32::from_le_bytes(i_pid_buf);
    drop(c_to_p_file);

    // Host P: Event loop
    let mut session = None;
    let mut overlay_arc = None;
    let mut exit_code = 1;
    let mut leftovers = Vec::new();
    let mut aborted = false;

    loop {
        let mut poll_fds = [
            PollFd::new(
                ctl_p.as_fd(),
                PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
            ),
            PollFd::new(action_listener.as_fd(), PollFlags::POLLIN),
        ];

        let n = poll(&mut poll_fds, PollTimeout::NONE)?;
        if n <= 0 {
            continue;
        }

        // Check action_listener
        if let Some(revents) = poll_fds[1].revents()
            && revents.contains(PollFlags::POLLIN)
        {
            while let Ok((conn, _)) = action_listener.accept() {
                if let Some(HostCommand::Kill) = events.on_action_conn(conn) {
                    let _ = nix::sys::signal::kill(
                        Pid::from_raw(i_pid),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                    aborted = true;
                    break;
                }
            }
            if aborted {
                break;
            }
        }

        // Check ctl_p
        if let Some(revents) = poll_fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                match ipc::recv_msg::<InitEvent>(&ctl_p) {
                    Ok((event, received_fds)) => match event {
                        InitEvent::FuseFd => {
                            let fuse_fd = received_fds.into_iter().next().ok_or_else(|| {
                                anyhow::anyhow!("no fuse fd received in InitEvent::FuseFd")
                            })?;
                            let mut overlay = Overlay::new(Path::new("/"), &cfg.staging_upper)?;
                            overlay.add_synthetic_dir(Path::new("/run/installctl"))?;
                            let (fs, arc) = new_fs(overlay);
                            let s = serve(fs, fuse_fd)?;
                            session = Some(s);
                            overlay_arc = Some(arc);
                        }
                        InitEvent::Ready => {}
                        InitEvent::Finished {
                            exit_code: c,
                            leftovers: l,
                        } => {
                            exit_code = c;
                            leftovers = l;
                            break;
                        }
                    },
                    Err(_) => {
                        break;
                    }
                }
            } else if revents.contains(PollFlags::POLLHUP) || revents.contains(PollFlags::POLLERR) {
                break;
            }
        }
    }

    if aborted {
        let _ = nix::sys::signal::kill(
            Pid::from_raw(i_pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        exit_code = 137;
    }

    // Wait for C
    let _ = waitpid(c_pid, None);
    child_guard.reaped = true;

    // Unmount FUSE by dropping session
    drop(session);

    let overlay = if let Some(arc) = overlay_arc {
        Arc::try_unwrap(arc)
            .map(|m| m.into_inner().unwrap())
            .unwrap_or_else(|arc| arc.lock().unwrap().clone())
    } else {
        Overlay::new(Path::new("/"), &cfg.staging_upper)?
    };

    let _ = std::fs::remove_file(&action_sock_path);

    Ok(SandboxOutcome {
        exit_code,
        leftovers,
        overlay,
        aborted,
    })
}
