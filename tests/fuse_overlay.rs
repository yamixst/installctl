mod common;

use std::fs;
use std::io::{IoSlice, IoSliceMut, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::sync::{Arc, Mutex};
use nix::sched::{CloneFlags, unshare};
use nix::sys::socket::{
    AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, recvmsg,
    sendmsg, socketpair,
};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, fork, getegid, geteuid};

use installctl::overlay::Overlay;
use installctl::overlay::fuse::{mount_fuse_fd, new_fs, serve};

pub struct MountContext {
    pub overlay: Arc<Mutex<Overlay>>,
    _upper: tempfile::TempDir,
}

impl std::ops::Deref for MountContext {
    type Target = Arc<Mutex<Overlay>>;
    fn deref(&self) -> &Self::Target {
        &self.overlay
    }
}

fn in_userns_with_mount<F: FnOnce(&Path)>(lower: &Path, f: F) -> MountContext {
    let upper = tempfile::tempdir().expect("create upper tempdir");
    let overlay = Overlay::new(lower, upper.path()).expect("create overlay");
    let (fs, overlay_arc) = new_fs(overlay);

    let (sock_parent, sock_child) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::SOCK_CLOEXEC,
    )
    .expect("socketpair failed");

    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();

    match unsafe { fork() }.expect("fork failed") {
        ForkResult::Child => {
            drop(sock_parent);

            unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS)
                .expect("unshare failed");

            fs::write("/proc/self/setgroups", "deny").expect("write setgroups");
            fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).expect("write uid_map");
            fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).expect("write gid_map");

            let _ = nix::mount::mount(
                None::<&str>,
                "/",
                None::<&str>,
                nix::mount::MsFlags::MS_REC | nix::mount::MsFlags::MS_PRIVATE,
                None::<&str>,
            );

            let mount_dir = tempfile::tempdir().expect("create mount dir");
            let mountpoint = mount_dir.path();

            let fd = mount_fuse_fd(mountpoint).expect("mount_fuse_fd failed");

            let iov = [IoSlice::new(b"F")];
            let fds = [fd.as_raw_fd()];
            let cmsg = ControlMessage::ScmRights(&fds);
            sendmsg::<()>(
                sock_child.as_raw_fd(),
                &iov,
                &[cmsg],
                MsgFlags::empty(),
                None,
            )
            .expect("sendmsg failed");

            let mut ack = [0u8; 1];
            let mut ack_iov = [IoSliceMut::new(&mut ack)];
            recvmsg::<()>(
                sock_child.as_raw_fd(),
                &mut ack_iov,
                None,
                MsgFlags::empty(),
            )
            .expect("recv ack failed");

            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                f(mountpoint);
            }));
            if res.is_err() {
                exit(1);
            } else {
                exit(0);
            }
        }
        ForkResult::Parent { child } => {
            drop(sock_child);

            let mut buf = [0u8; 1];
            let mut iov = [IoSliceMut::new(&mut buf)];
            let mut cmsg_buf = nix::cmsg_space!(std::os::fd::RawFd);
            let msg = recvmsg::<()>(
                sock_parent.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg_buf),
                MsgFlags::empty(),
            )
            .expect("recvmsg failed");

            let mut fuse_fd = None;
            for c in msg.cmsgs().expect("cmsgs failed") {
                if let ControlMessageOwned::ScmRights(fds) = c
                    && let Some(&raw_fd) = fds.first()
                {
                    fuse_fd = Some(unsafe { OwnedFd::from_raw_fd(raw_fd) });
                }
            }
            let fuse_fd = fuse_fd.expect("no fd received");

            let session = serve(fs, fuse_fd).expect("serve failed");

            let ack_iov = [IoSlice::new(b"A")];
            sendmsg::<()>(
                sock_parent.as_raw_fd(),
                &ack_iov,
                &[],
                MsgFlags::empty(),
                None,
            )
            .expect("send ack failed");

            let status = waitpid(child, None).expect("waitpid failed");
            assert_eq!(
                status,
                WaitStatus::Exited(child, 0),
                "child did not exit cleanly"
            );

            let _ = session.join();

            MountContext {
                overlay: overlay_arc,
                _upper: upper,
            }
        }
    }
}

#[test]
fn fuse_basic_ops() {
    if !common::priv_env() {
        return;
    }

    let lower_dir = tempfile::tempdir().expect("lower tempdir");
    let lower = lower_dir.path();

    fs::write(lower.join("to_read.txt"), "hello from lower\n").unwrap();
    fs::write(lower.join("to_append.txt"), "initial\n").unwrap();
    fs::write(lower.join("to_delete.txt"), "delete me\n").unwrap();

    let ctx = in_userns_with_mount(lower, |mount| {
        // 1. Read lower file
        let content = fs::read_to_string(mount.join("to_read.txt")).expect("read lower file");
        assert_eq!(content, "hello from lower\n");

        // 2. Write new file
        fs::write(mount.join("new_file.txt"), "brand new content\n").expect("write new file");

        // 3. Append to lower file
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(mount.join("to_append.txt"))
            .expect("open to append");
        f.write_all(b"second line\n").expect("append to file");
        drop(f);

        let appended = fs::read_to_string(mount.join("to_append.txt")).expect("read appended");
        assert_eq!(appended, "initial\nsecond line\n");

        // 4. remove_file lower file
        fs::remove_file(mount.join("to_delete.txt")).expect("remove lower file");
        assert!(!mount.join("to_delete.txt").exists());

        // 5. create_dir_all
        fs::create_dir_all(mount.join("dir1/dir2")).expect("create_dir_all");
        assert!(mount.join("dir1/dir2").is_dir());

        // 6. std::os::unix::fs::symlink
        std::os::unix::fs::symlink(Path::new("target_link"), mount.join("symlink_test"))
            .expect("symlink");
        assert_eq!(
            fs::read_link(mount.join("symlink_test")).expect("read_link"),
            PathBuf::from("target_link")
        );

        // 7. fs::rename of a file
        fs::rename(mount.join("new_file.txt"), mount.join("renamed_file.txt"))
            .expect("rename file");
        assert!(!mount.join("new_file.txt").exists());
        assert_eq!(
            fs::read_to_string(mount.join("renamed_file.txt")).expect("read renamed"),
            "brand new content\n"
        );
    });

    // Afterwards: host lower tree unchanged
    assert_eq!(
        fs::read_to_string(lower.join("to_read.txt")).unwrap(),
        "hello from lower\n"
    );
    assert_eq!(
        fs::read_to_string(lower.join("to_append.txt")).unwrap(),
        "initial\n"
    );
    assert!(lower.join("to_delete.txt").is_file());
    assert!(!lower.join("new_file.txt").exists());
    assert!(!lower.join("renamed_file.txt").exists());
    assert!(!lower.join("dir1").exists());
    assert!(!lower.join("symlink_test").exists());

    // changes::collect reports the expected 5+ changes
    let overlay = ctx.overlay.lock().unwrap();
    let changes = installctl::changes::collect(&overlay).expect("changes::collect");
    assert!(
        changes.len() >= 5,
        "expected at least 5 changes, got {}: {:#?}",
        changes.len(),
        changes
    );

    let paths: std::collections::HashSet<_> = changes.iter().map(|c| c.path.clone()).collect();
    assert!(paths.contains(Path::new("/to_append.txt")));
    assert!(paths.contains(Path::new("/to_delete.txt")));
    assert!(paths.contains(Path::new("/renamed_file.txt")));
    assert!(paths.contains(Path::new("/dir1")));
    assert!(paths.contains(Path::new("/dir1/dir2")));
    assert!(paths.contains(Path::new("/symlink_test")));
}

#[test]
fn fuse_mv_lower_dir_falls_back() {
    if !common::priv_env() {
        return;
    }

    let lower_dir = tempfile::tempdir().expect("lower tempdir");
    let lower = lower_dir.path();

    let sub = lower.join("orig_dir");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("file1.txt"), "file1 content\n").unwrap();
    fs::write(sub.join("file2.txt"), "file2 content\n").unwrap();

    let _ctx = in_userns_with_mount(lower, |mount| {
        let src = mount.join("orig_dir");
        let dst = mount.join("moved_dir");

        let status = Command::new("mv")
            .arg(&src)
            .arg(&dst)
            .status()
            .expect("mv execution failed");
        assert!(status.success(), "mv exited with error: {:?}", status);

        assert!(!src.exists(), "src should not exist after mv");
        assert!(dst.is_dir(), "dst should be a directory");
        assert_eq!(
            fs::read_to_string(dst.join("file1.txt")).unwrap(),
            "file1 content\n"
        );
        assert_eq!(
            fs::read_to_string(dst.join("file2.txt")).unwrap(),
            "file2 content\n"
        );
    });

    assert!(lower.join("orig_dir").is_dir());
    assert!(lower.join("orig_dir/file1.txt").is_file());
    assert!(lower.join("orig_dir/file2.txt").is_file());
    assert!(!lower.join("moved_dir").exists());
}
