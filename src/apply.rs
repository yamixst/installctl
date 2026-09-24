use crate::changes::{Change, ChangeKind};
use crate::overlay::FileKind;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Original {
    pub path: PathBuf,
    pub kind: FileKind,
    pub mode: u32,
    pub content: Option<Vec<u8>>,
    pub link_target: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelperPlan {
    pub changes: Vec<Change>,
}

pub fn originals_path(plan: &Path) -> PathBuf {
    let s = plan.to_string_lossy();
    if let Some(stripped) = s.strip_suffix(".json") {
        PathBuf::from(format!("{stripped}.originals.json"))
    } else {
        PathBuf::from(format!("{s}.originals.json"))
    }
}

fn resolve_path(root: &Path, p: &Path) -> PathBuf {
    if root == Path::new("/") {
        p.to_path_buf()
    } else {
        root.join(p.strip_prefix("/").unwrap_or(p))
    }
}

fn is_writable(p: &Path) -> bool {
    if let Ok(c) = CString::new(p.as_os_str().as_bytes()) {
        unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
    } else {
        false
    }
}

fn chown_path(p: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c = CString::new(p.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let ret = unsafe { libc::lchown(c.as_ptr(), uid, gid) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn check_conflicts(changes: &[Change], root: &Path) -> Vec<Conflict> {
    let mut conflicts = Vec::new();
    for change in changes {
        let target = resolve_path(root, &change.path);
        match change.kind {
            ChangeKind::Created => {
                if target.symlink_metadata().is_ok() {
                    conflicts.push(Conflict {
                        path: change.path.clone(),
                        reason: "created by someone else since the run".to_string(),
                    });
                }
            }
            ChangeKind::Modified | ChangeKind::Deleted => match target.symlink_metadata() {
                Ok(meta) => {
                    let kind = if meta.file_type().is_file() {
                        FileKind::File
                    } else if meta.file_type().is_dir() {
                        FileKind::Dir
                    } else if meta.file_type().is_symlink() {
                        FileKind::Symlink
                    } else {
                        FileKind::Other
                    };
                    let size = meta.len();
                    let mtime_ns =
                        (meta.mtime() as i128) * 1_000_000_000 + (meta.mtime_nsec() as i128);
                    if let Some(ref origin) = change.origin {
                        if kind != origin.kind || size != origin.size || mtime_ns != origin.mtime_ns
                        {
                            conflicts.push(Conflict {
                                path: change.path.clone(),
                                reason: "changed since the run".to_string(),
                            });
                        }
                    } else {
                        conflicts.push(Conflict {
                            path: change.path.clone(),
                            reason: "changed since the run".to_string(),
                        });
                    }
                }
                Err(_) => {
                    conflicts.push(Conflict {
                        path: change.path.clone(),
                        reason: "changed since the run".to_string(),
                    });
                }
            },
        }
    }
    conflicts
}

pub fn needs_root(change: &Change, root: &Path) -> bool {
    let target = resolve_path(root, &change.path);
    if target.symlink_metadata().is_ok() && !is_writable(&target) {
        return true;
    }
    let start = if target.symlink_metadata().is_ok() {
        target.parent()
    } else {
        Some(target.as_path())
    };
    let mut cur = start;
    while let Some(p) = cur {
        if p.symlink_metadata().is_ok() {
            return !is_writable(p);
        }
        cur = p.parent();
    }
    true
}

pub fn capture_originals(changes: &[Change], root: &Path) -> io::Result<Vec<Original>> {
    let mut originals = Vec::new();
    for change in changes {
        match change.kind {
            ChangeKind::Modified | ChangeKind::Deleted => {
                let target = resolve_path(root, &change.path);
                let meta = std::fs::symlink_metadata(&target)?;
                let kind = if meta.file_type().is_file() {
                    FileKind::File
                } else if meta.file_type().is_dir() {
                    FileKind::Dir
                } else if meta.file_type().is_symlink() {
                    FileKind::Symlink
                } else {
                    FileKind::Other
                };
                let mode = meta.mode() & 0o7777;
                let (content, link_target) = match kind {
                    FileKind::File => (Some(std::fs::read(&target)?), None),
                    FileKind::Symlink => (None, Some(std::fs::read_link(&target)?)),
                    _ => (None, None),
                };
                originals.push(Original {
                    path: change.path.clone(),
                    kind,
                    mode,
                    content,
                    link_target,
                });
            }
            ChangeKind::Created => {}
        }
    }
    Ok(originals)
}

pub fn apply(changes: &[Change], root: &Path, as_root: bool) -> io::Result<()> {
    // 1. Deleted: deepest path first (files/symlinks remove_file, dirs remove_dir)
    let mut deleted: Vec<&Change> = changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Deleted)
        .collect();
    deleted.sort_by(|a, b| {
        b.path
            .components()
            .count()
            .cmp(&a.path.components().count())
            .then_with(|| b.path.cmp(&a.path))
    });

    for change in deleted {
        let target = resolve_path(root, &change.path);
        if change.entry == FileKind::Dir {
            std::fs::remove_dir(&target)?;
        } else {
            std::fs::remove_file(&target)?;
        }
    }

    // 2. Created/Modified dirs: shallowest first (create_dir + chmod; Modified dir -> chmod only)
    let mut dirs: Vec<&Change> = changes
        .iter()
        .filter(|c| {
            c.entry == FileKind::Dir
                && (c.kind == ChangeKind::Created || c.kind == ChangeKind::Modified)
        })
        .collect();
    dirs.sort_by(|a, b| {
        a.path
            .components()
            .count()
            .cmp(&b.path.components().count())
            .then_with(|| a.path.cmp(&b.path))
    });

    for change in dirs {
        let target = resolve_path(root, &change.path);
        if change.kind == ChangeKind::Created {
            if !target.is_dir() {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::create_dir(&target)?;
            }
            if as_root {
                chown_path(&target, 0, 0)?;
            }
            std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(change.mode & 0o7777),
            )?;
        } else if change.kind == ChangeKind::Modified {
            std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(change.mode & 0o7777),
            )?;
        }
    }

    // 3. Files and symlinks: files (write temp, fsync, chmod, rename) and symlinks (create temp, rename)
    let mut files_and_symlinks: Vec<&Change> = changes
        .iter()
        .filter(|c| {
            c.entry != FileKind::Dir
                && (c.kind == ChangeKind::Created || c.kind == ChangeKind::Modified)
        })
        .collect();
    files_and_symlinks.sort_by(|a, b| a.path.cmp(&b.path));

    for change in files_and_symlinks {
        let target = resolve_path(root, &change.path);
        let dir = target.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "target has no parent directory")
        })?;
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
        }
        let file_name = target
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF8 file name"))?;
        let tmp_path = dir.join(format!(".{file_name}.installctl-tmp"));

        if change.entry == FileKind::Symlink {
            let _ = std::fs::remove_file(&tmp_path);
            let link_target = change.link_target.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing link_target")
            })?;
            std::os::unix::fs::symlink(link_target, &tmp_path)?;

            if as_root {
                match change.kind {
                    ChangeKind::Created => {
                        if let Err(e) = chown_path(&tmp_path, 0, 0) {
                            let _ = std::fs::remove_file(&tmp_path);
                            return Err(e);
                        }
                    }
                    ChangeKind::Modified => {
                        let (uid, gid) = if let Ok(meta) = target.symlink_metadata() {
                            (meta.uid(), meta.gid())
                        } else {
                            change.owner
                        };
                        if let Err(e) = chown_path(&tmp_path, uid, gid) {
                            let _ = std::fs::remove_file(&tmp_path);
                            return Err(e);
                        }
                    }
                    _ => {}
                }
            }

            if let Err(e) = std::fs::rename(&tmp_path, &target) {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e);
            }
        } else {
            let write_file = || -> io::Result<()> {
                let mut tmp_file = std::fs::File::create(&tmp_path)?;
                if let Some(ref blob_path) = change.blob {
                    let mut blob_file = std::fs::File::open(blob_path)?;
                    std::io::copy(&mut blob_file, &mut tmp_file)?;
                }
                tmp_file.sync_all()?;

                if as_root {
                    use std::os::unix::io::AsRawFd;
                    match change.kind {
                        ChangeKind::Created => {
                            let ret = unsafe { libc::fchown(tmp_file.as_raw_fd(), 0, 0) };
                            if ret != 0 {
                                return Err(io::Error::last_os_error());
                            }
                        }
                        ChangeKind::Modified => {
                            let (uid, gid) = if let Ok(meta) = target.symlink_metadata() {
                                (meta.uid(), meta.gid())
                            } else {
                                change.owner
                            };
                            let ret = unsafe { libc::fchown(tmp_file.as_raw_fd(), uid, gid) };
                            if ret != 0 {
                                return Err(io::Error::last_os_error());
                            }
                        }
                        _ => {}
                    }
                }

                tmp_file.set_permissions(std::fs::Permissions::from_mode(change.mode & 0o7777))?;
                drop(tmp_file);
                std::fs::rename(&tmp_path, &target)?;
                Ok(())
            };

            if let Err(e) = write_file() {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e);
            }
        }
    }

    Ok(())
}

pub fn helper_main(plan: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(plan)
        .map_err(|e| anyhow::anyhow!("failed to read plan {}: {e}", plan.display()))?;
    let plan_data: HelperPlan = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse plan {}: {e}", plan.display()))?;

    let originals = capture_originals(&plan_data.changes, Path::new("/"))?;
    apply(&plan_data.changes, Path::new("/"), true)?;

    let orig_json = serde_json::to_string_pretty(&originals)?;
    let orig_path = originals_path(plan);
    std::fs::write(&orig_path, &orig_json)?;

    let uid = std::env::var("SUDO_UID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(nix::unistd::Uid::from_raw);
    let gid = std::env::var("SUDO_GID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(nix::unistd::Gid::from_raw);

    if uid.is_some() || gid.is_some() {
        let _ = nix::unistd::chown(&orig_path, uid, gid);
    }

    let alt_path = PathBuf::from(format!("{}.originals.json", plan.display()));
    if alt_path != orig_path {
        let _ = std::fs::copy(&orig_path, &alt_path);
        if uid.is_some() || gid.is_some() {
            let _ = nix::unistd::chown(&alt_path, uid, gid);
        }
    }

    Ok(())
}

pub fn apply_root_part(
    changes: &[Change],
    staging: &Path,
    exe: &Path,
) -> anyhow::Result<Vec<Original>> {
    if changes.is_empty() {
        return Ok(Vec::new());
    }

    let plan_path = staging.join("root-plan.json");
    let plan = HelperPlan {
        changes: changes.to_vec(),
    };
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan)?)?;

    let status = std::process::Command::new("sudo")
        .arg("--")
        .arg(exe)
        .arg("--apply-helper")
        .arg(&plan_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to execute sudo: {e}"))?;

    if !status.success() {
        anyhow::bail!("sudo helper exited with failure: {status}");
    }

    let orig_path = originals_path(&plan_path);
    let orig_json = std::fs::read_to_string(&orig_path)
        .map_err(|e| anyhow::anyhow!("failed to read originals file {}: {e}", orig_path.display()))?;
    let originals: Vec<Original> = serde_json::from_str(&orig_json)?;
    Ok(originals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{LowerStamp, Meta};
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn sudo_env() -> bool {
        let reason = match std::process::Command::new("sudo").args(["-n", "true"]).status() {
            Ok(s) if s.success() => return true,
            Ok(s) => format!("sudo -n true exited with {s}"),
            Err(e) => format!("failed to execute sudo: {e}"),
        };
        if std::env::var("INSTALLCTL_REQUIRE_PRIV_TESTS").as_deref() == Ok("1") {
            panic!("{reason}");
        } else {
            eprintln!("skipping: {reason}");
            false
        }
    }

    fn find_installctl() -> PathBuf {
        if let Some(p) = option_env!("CARGO_BIN_EXE_installctl") {
            let path = PathBuf::from(p);
            if path.is_file() {
                return path;
            }
        }
        if let Ok(p) = std::env::var("CARGO_BIN_EXE_installctl") {
            let path = PathBuf::from(p);
            if path.is_file() {
                return path;
            }
        }
        if let Ok(cur) = std::env::current_exe()
            && let Some(parent) = cur.parent()
        {
            let candidate = parent.parent().unwrap_or(parent).join("installctl");
            if candidate.is_file() {
                return candidate;
            }
            let candidate = parent.join("installctl");
            if candidate.is_file() {
                return candidate;
            }
        }
        for candidate in &[
            "target/debug/installctl",
            "target-docker/debug/installctl",
            "/work/target-docker/debug/installctl",
        ] {
            let p = PathBuf::from(candidate);
            if p.is_file() {
                return p;
            }
        }
        PathBuf::from("installctl")
    }

    fn has_installctl_tmp_files(dir: &Path) -> bool {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && name.contains(".installctl-tmp")
                {
                    return true;
                }
                if path.is_dir() && has_installctl_tmp_files(&path) {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn applies_create_modify_delete() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path();
        let blob_dir = tempfile::tempdir().unwrap();
        let blob = blob_dir.path();

        // Initial files
        fs::write(root.join("mod_file.txt"), b"orig mod content").unwrap();
        fs::set_permissions(root.join("mod_file.txt"), fs::Permissions::from_mode(0o644)).unwrap();

        std::os::unix::fs::symlink("del_file.txt", root.join("link_mod")).unwrap();

        fs::write(root.join("del_file.txt"), b"delete me").unwrap();
        fs::set_permissions(root.join("del_file.txt"), fs::Permissions::from_mode(0o644)).unwrap();

        fs::create_dir(root.join("del_dir")).unwrap();
        fs::set_permissions(root.join("del_dir"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("del_dir/child.txt"), b"child content").unwrap();
        fs::set_permissions(root.join("del_dir/child.txt"), fs::Permissions::from_mode(0o644)).unwrap();

        // Staged blobs
        let create_blob = blob.join("created.blob");
        fs::write(&create_blob, b"#!/bin/sh\necho hello\n").unwrap();
        let inner_blob = blob.join("inner.blob");
        fs::write(&inner_blob, b"inner content").unwrap();
        let mod_blob = blob.join("mod.blob");
        fs::write(&mod_blob, b"new mod content").unwrap();

        let changes = vec![
            Change {
                path: PathBuf::from("/created_file.sh"),
                kind: ChangeKind::Created,
                entry: FileKind::File,
                size: 21,
                mode: 0o755,
                owner: (1000, 1000),
                blob: Some(create_blob),
                link_target: None,
                origin: None,
            },
            Change {
                path: PathBuf::from("/created_link"),
                kind: ChangeKind::Created,
                entry: FileKind::Symlink,
                size: 15,
                mode: 0o777,
                owner: (1000, 1000),
                blob: None,
                link_target: Some(PathBuf::from("created_file.sh")),
                origin: None,
            },
            Change {
                path: PathBuf::from("/created_dir"),
                kind: ChangeKind::Created,
                entry: FileKind::Dir,
                size: 0,
                mode: 0o755,
                owner: (1000, 1000),
                blob: None,
                link_target: None,
                origin: None,
            },
            Change {
                path: PathBuf::from("/created_dir/inner.txt"),
                kind: ChangeKind::Created,
                entry: FileKind::File,
                size: 13,
                mode: 0o644,
                owner: (1000, 1000),
                blob: Some(inner_blob),
                link_target: None,
                origin: None,
            },
            Change {
                path: PathBuf::from("/mod_file.txt"),
                kind: ChangeKind::Modified,
                entry: FileKind::File,
                size: 15,
                mode: 0o700,
                owner: (1000, 1000),
                blob: Some(mod_blob),
                link_target: None,
                origin: None,
            },
            Change {
                path: PathBuf::from("/link_mod"),
                kind: ChangeKind::Modified,
                entry: FileKind::Symlink,
                size: 12,
                mode: 0o777,
                owner: (1000, 1000),
                blob: None,
                link_target: Some(PathBuf::from("mod_file.txt")),
                origin: None,
            },
            Change {
                path: PathBuf::from("/del_file.txt"),
                kind: ChangeKind::Deleted,
                entry: FileKind::File,
                size: 9,
                mode: 0o644,
                owner: (1000, 1000),
                blob: None,
                link_target: None,
                origin: None,
            },
            Change {
                path: PathBuf::from("/del_dir/child.txt"),
                kind: ChangeKind::Deleted,
                entry: FileKind::File,
                size: 13,
                mode: 0o644,
                owner: (1000, 1000),
                blob: None,
                link_target: None,
                origin: None,
            },
            Change {
                path: PathBuf::from("/del_dir"),
                kind: ChangeKind::Deleted,
                entry: FileKind::Dir,
                size: 0,
                mode: 0o755,
                owner: (1000, 1000),
                blob: None,
                link_target: None,
                origin: None,
            },
        ];

        apply(&changes, root, false).unwrap();

        // Verify created file
        let created_path = root.join("created_file.sh");
        assert!(created_path.is_file());
        assert_eq!(fs::read(&created_path).unwrap(), b"#!/bin/sh\necho hello\n");
        assert_eq!(fs::symlink_metadata(&created_path).unwrap().mode() & 0o777, 0o755);

        // Verify created symlink
        let created_link_path = root.join("created_link");
        assert_eq!(fs::read_link(&created_link_path).unwrap(), Path::new("created_file.sh"));

        // Verify created dir and inner file
        let created_dir_path = root.join("created_dir");
        assert!(created_dir_path.is_dir());
        assert_eq!(fs::symlink_metadata(&created_dir_path).unwrap().mode() & 0o777, 0o755);
        let inner_path = created_dir_path.join("inner.txt");
        assert!(inner_path.is_file());
        assert_eq!(fs::read(&inner_path).unwrap(), b"inner content");
        assert_eq!(fs::symlink_metadata(&inner_path).unwrap().mode() & 0o777, 0o644);

        // Verify modified file
        let mod_path = root.join("mod_file.txt");
        assert!(mod_path.is_file());
        assert_eq!(fs::read(&mod_path).unwrap(), b"new mod content");
        assert_eq!(fs::symlink_metadata(&mod_path).unwrap().mode() & 0o777, 0o700);

        // Verify modified symlink
        let mod_link_path = root.join("link_mod");
        assert_eq!(fs::read_link(&mod_link_path).unwrap(), Path::new("mod_file.txt"));

        // Verify deleted entries are gone
        assert!(!root.join("del_file.txt").exists());
        assert!(!root.join("del_dir/child.txt").exists());
        assert!(!root.join("del_dir").exists());

        // Verify no temp files left
        assert!(!has_installctl_tmp_files(root), "found dangling .installctl-tmp files");
    }

    #[test]
    fn conflicts_detected() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path();

        // 1. Target created after the run
        fs::write(root.join("created_later.txt"), b"someone made this").unwrap();
        let create_change = Change {
            path: PathBuf::from("/created_later.txt"),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 5,
            mode: 0o644,
            owner: (1000, 1000),
            blob: None,
            link_target: None,
            origin: None,
        };

        let conflicts = check_conflicts(&[create_change], root);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, Path::new("/created_later.txt"));
        assert_eq!(conflicts[0].reason, "created by someone else since the run");

        // 2. Modified target touched (mtime changed)
        let mod_target = root.join("touched.txt");
        fs::write(&mod_target, b"original").unwrap();
        let meta = fs::symlink_metadata(&mod_target).unwrap();
        let mtime_ns = (meta.mtime() as i128) * 1_000_000_000 + (meta.mtime_nsec() as i128);
        let stamp = LowerStamp {
            kind: FileKind::File,
            size: meta.len(),
            mtime_ns,
            dev: meta.dev(),
            ino: meta.ino(),
            meta: Meta {
                mode: meta.mode() & 0o7777,
                uid: meta.uid(),
                gid: meta.gid(),
                atime_ns: 0,
                mtime_ns,
            },
        };

        // Sleep briefly and touch/modify the file so mtime differs
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(&mod_target, b"touched content").unwrap();

        let mod_change = Change {
            path: PathBuf::from("/touched.txt"),
            kind: ChangeKind::Modified,
            entry: FileKind::File,
            size: 15,
            mode: 0o644,
            owner: (1000, 1000),
            blob: None,
            link_target: None,
            origin: Some(stamp),
        };

        let conflicts = check_conflicts(&[mod_change], root);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, Path::new("/touched.txt"));
        assert_eq!(conflicts[0].reason, "changed since the run");
    }

    #[test]
    fn originals_captured_before_apply() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path();

        fs::write(root.join("mod.txt"), b"before-mod").unwrap();
        fs::set_permissions(root.join("mod.txt"), fs::Permissions::from_mode(0o644)).unwrap();

        fs::write(root.join("del.txt"), b"before-del").unwrap();
        fs::set_permissions(root.join("del.txt"), fs::Permissions::from_mode(0o755)).unwrap();

        let changes = vec![
            Change {
                path: PathBuf::from("/mod.txt"),
                kind: ChangeKind::Modified,
                entry: FileKind::File,
                size: 10,
                mode: 0o600,
                owner: (1000, 1000),
                blob: None,
                link_target: None,
                origin: None,
            },
            Change {
                path: PathBuf::from("/del.txt"),
                kind: ChangeKind::Deleted,
                entry: FileKind::File,
                size: 10,
                mode: 0o755,
                owner: (1000, 1000),
                blob: None,
                link_target: None,
                origin: None,
            },
        ];

        let originals = capture_originals(&changes, root).unwrap();
        assert_eq!(originals.len(), 2);

        let mod_orig = originals.iter().find(|o| o.path == Path::new("/mod.txt")).unwrap();
        assert_eq!(mod_orig.kind, FileKind::File);
        assert_eq!(mod_orig.mode, 0o644);
        assert_eq!(mod_orig.content.as_deref(), Some(b"before-mod".as_slice()));
        assert_eq!(mod_orig.link_target, None);

        let del_orig = originals.iter().find(|o| o.path == Path::new("/del.txt")).unwrap();
        assert_eq!(del_orig.kind, FileKind::File);
        assert_eq!(del_orig.mode, 0o755);
        assert_eq!(del_orig.content.as_deref(), Some(b"before-del".as_slice()));
        assert_eq!(del_orig.link_target, None);
    }

    #[test]
    fn helper_applies_as_root() {
        if !sudo_env() {
            return;
        }

        let staging = tempfile::tempdir().unwrap();
        let blob_path = staging.path().join("t.blob");
        fs::write(&blob_path, b"#!/bin/sh\necho test-helper\n").unwrap();
        fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&blob_path, fs::Permissions::from_mode(0o644)).unwrap();

        let target_path = Path::new("/usr/local/bin/t");
        let _ = std::process::Command::new("sudo")
            .args(["rm", "-f", target_path.to_str().unwrap()])
            .status();

        let plan_path = staging.path().join("test-plan.json");
        let change = Change {
            path: target_path.to_path_buf(),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 26,
            mode: 0o4755,
            owner: (0, 0),
            blob: Some(blob_path),
            link_target: None,
            origin: None,
        };
        let plan = HelperPlan {
            changes: vec![change],
        };
        fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

        let exe = find_installctl();
        let status = std::process::Command::new("sudo")
            .args(["--", exe.to_str().unwrap(), "--apply-helper", plan_path.to_str().unwrap()])
            .status()
            .expect("run sudo installctl --apply-helper");

        assert!(status.success(), "sudo helper exited with failure: {status}");

        // Target file must exist, owned 0:0, mode 04755 (verifying SUID bit preserved after fchown)
        let meta = fs::symlink_metadata(target_path).expect("/usr/local/bin/t must exist");
        assert_eq!(meta.uid(), 0, "target file must be owned by root uid 0");
        assert_eq!(meta.gid(), 0, "target file must be owned by root gid 0");
        assert_eq!(meta.mode() & 0o7777, 0o4755, "target file mode must be 04755 with SUID preserved");

        // Originals file must exist and be owned by tester
        let orig_path = originals_path(&plan_path);
        assert!(orig_path.is_file(), "originals file must exist at {}", orig_path.display());
        let orig_meta = fs::symlink_metadata(&orig_path).unwrap();
        let caller_uid = nix::unistd::getuid().as_raw();
        let caller_gid = nix::unistd::getgid().as_raw();
        assert_eq!(orig_meta.uid(), caller_uid, "originals file must be owned by caller uid");
        assert_eq!(orig_meta.gid(), caller_gid, "originals file must be owned by caller gid");

        // Cleanup
        let _ = std::process::Command::new("sudo")
            .args(["rm", "-f", target_path.to_str().unwrap()])
            .status();

        // Also test apply_root_part helper function
        let target2_path = Path::new("/usr/local/bin/t_root_part");
        let _ = std::process::Command::new("sudo")
            .args(["rm", "-f", target2_path.to_str().unwrap()])
            .status();

        let blob2_path = staging.path().join("t2.blob");
        fs::write(&blob2_path, b"#!/bin/sh\necho test-root-part\n").unwrap();
        fs::set_permissions(&blob2_path, fs::Permissions::from_mode(0o644)).unwrap();

        let change2 = Change {
            path: target2_path.to_path_buf(),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 27,
            mode: 0o755,
            owner: (0, 0),
            blob: Some(blob2_path),
            link_target: None,
            origin: None,
        };

        let origs = apply_root_part(&[change2], staging.path(), &exe)
            .expect("apply_root_part must succeed");
        assert!(origs.is_empty(), "created change has no originals");

        let meta2 = fs::symlink_metadata(target2_path).expect("/usr/local/bin/t_root_part must exist");
        assert_eq!(meta2.uid(), 0, "must be owned by root uid 0");
        assert_eq!(meta2.gid(), 0, "must be owned by root gid 0");
        assert_eq!(meta2.mode() & 0o777, 0o755);

        let _ = std::process::Command::new("sudo")
            .args(["rm", "-f", target2_path.to_str().unwrap()])
            .status();
    }

    #[test]
    fn test_needs_root_detection() {
        let root_dir = tempfile::tempdir().unwrap();
        let user_file = root_dir.path().join("file.txt");
        let change_user = Change {
            path: user_file.clone(),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 0,
            mode: 0o644,
            owner: (1000, 1000),
            blob: None,
            link_target: None,
            origin: None,
        };
        // root_dir is writable by current user
        assert!(!needs_root(&change_user, Path::new("/")));

        if nix::unistd::getuid().as_raw() != 0 {
            let change_root = Change {
                path: PathBuf::from("/usr/local/bin/test_needs_root_file"),
                kind: ChangeKind::Created,
                entry: FileKind::File,
                size: 0,
                mode: 0o644,
                owner: (0, 0),
                blob: None,
                link_target: None,
                origin: None,
            };
            assert!(needs_root(&change_root, Path::new("/")));
        }
    }
}
