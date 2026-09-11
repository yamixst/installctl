use crate::overlay::{FileKind, LowerStamp, Overlay, UpperNode};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub entry: FileKind, /* File|Dir|Symlink */
    pub size: u64,
    pub mode: u32,
    pub owner: (u32, u32),
    pub blob: Option<PathBuf>, /* absolute host path of staged content */
    pub link_target: Option<PathBuf>,
    pub origin: Option<LowerStamp>,
}

fn kind_order(k: ChangeKind) -> u8 {
    match k {
        ChangeKind::Deleted => 0,
        ChangeKind::Created => 1,
        ChangeKind::Modified => 2,
    }
}

fn deleted_change(path: PathBuf, stamp: LowerStamp) -> Change {
    Change {
        path,
        kind: ChangeKind::Deleted,
        entry: stamp.kind,
        size: stamp.size,
        mode: stamp.meta.mode,
        owner: (stamp.meta.uid, stamp.meta.gid),
        blob: None,
        link_target: None,
        origin: Some(stamp),
    }
}

fn read_chunk<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn files_content_equal(blob_path: &Path, lower_path: &Path) -> io::Result<bool> {
    let blob_meta = std::fs::metadata(blob_path)?;
    let lower_meta = match std::fs::symlink_metadata(lower_path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };

    if !lower_meta.file_type().is_file() {
        return Ok(false);
    }

    if blob_meta.len() != lower_meta.len() {
        return Ok(false);
    }

    let mut blob_file = File::open(blob_path)?;
    let mut lower_file = File::open(lower_path)?;

    let mut buf_blob = [0u8; 8192];
    let mut buf_lower = [0u8; 8192];

    loop {
        let n_blob = read_chunk(&mut blob_file, &mut buf_blob)?;
        let n_lower = read_chunk(&mut lower_file, &mut buf_lower)?;
        if n_blob != n_lower {
            return Ok(false);
        }
        if n_blob == 0 {
            return Ok(true);
        }
        if buf_blob[..n_blob] != buf_lower[..n_lower] {
            return Ok(false);
        }
    }
}

fn collect_lower_descendants(
    overlay: &Overlay,
    base_overlay_path: &Path,
    out: &mut Vec<(PathBuf, LowerStamp)>,
) -> io::Result<()> {
    let lower_dir = overlay.lower_path(base_overlay_path);
    let rd = match std::fs::read_dir(&lower_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    let mut names = Vec::new();
    for entry in rd {
        let entry = entry?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        names.push(name);
    }
    names.sort();

    for name in names {
        let child_overlay_path = base_overlay_path.join(name);
        let stamp = match overlay.stat_lower(&child_overlay_path) {
            Ok(s) => s,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
            Err(e) => return Err(e),
        };
        let is_dir = stamp.kind == FileKind::Dir;
        out.push((child_overlay_path.clone(), stamp));
        if is_dir {
            collect_lower_descendants(overlay, &child_overlay_path, out)?;
        }
    }
    Ok(())
}

fn add_change(
    changes: &mut Vec<Change>,
    seen: &mut HashSet<(PathBuf, ChangeKind)>,
    change: Change,
) {
    if seen.insert((change.path.clone(), change.kind)) {
        changes.push(change);
    }
}

fn delete_lower_descendants(
    overlay: &Overlay,
    dir_path: &Path,
    changes: &mut Vec<Change>,
    seen: &mut HashSet<(PathBuf, ChangeKind)>,
) -> io::Result<()> {
    let mut descendants = Vec::new();
    collect_lower_descendants(overlay, dir_path, &mut descendants)?;
    for (desc_path, stamp) in descendants {
        if !overlay.nodes().contains_key(&desc_path) {
            add_change(changes, seen, deleted_change(desc_path, stamp));
        }
    }
    Ok(())
}

pub fn collect(overlay: &Overlay) -> io::Result<Vec<Change>> {
    let mut seen = HashSet::new();
    let mut changes = Vec::new();

    for (path, node) in overlay.nodes() {
        match node {
            UpperNode::File { blob, meta, origin } => {
                let size = std::fs::metadata(blob)?.len();
                match origin {
                    None => {
                        add_change(
                            &mut changes,
                            &mut seen,
                            Change {
                                path: path.clone(),
                                kind: ChangeKind::Created,
                                entry: FileKind::File,
                                size,
                                mode: meta.mode,
                                owner: (meta.uid, meta.gid),
                                blob: Some(blob.clone()),
                                link_target: None,
                                origin: None,
                            },
                        );
                    }
                    Some(origin) => {
                        if origin.kind == FileKind::File {
                            let mode_equal = meta.mode == origin.meta.mode;
                            let owner_equal =
                                (meta.uid, meta.gid) == (origin.meta.uid, origin.meta.gid);
                            let content_equal =
                                files_content_equal(blob, &overlay.lower_path(path))?;
                            if !(mode_equal && owner_equal && content_equal) {
                                add_change(
                                    &mut changes,
                                    &mut seen,
                                    Change {
                                        path: path.clone(),
                                        kind: ChangeKind::Modified,
                                        entry: FileKind::File,
                                        size,
                                        mode: meta.mode,
                                        owner: (meta.uid, meta.gid),
                                        blob: Some(blob.clone()),
                                        link_target: None,
                                        origin: Some(origin.clone()),
                                    },
                                );
                            }
                        } else {
                            add_change(
                                &mut changes,
                                &mut seen,
                                deleted_change(path.clone(), origin.clone()),
                            );
                            if origin.kind == FileKind::Dir {
                                delete_lower_descendants(
                                    overlay,
                                    path,
                                    &mut changes,
                                    &mut seen,
                                )?;
                            }
                            add_change(
                                &mut changes,
                                &mut seen,
                                Change {
                                    path: path.clone(),
                                    kind: ChangeKind::Created,
                                    entry: FileKind::File,
                                    size,
                                    mode: meta.mode,
                                    owner: (meta.uid, meta.gid),
                                    blob: Some(blob.clone()),
                                    link_target: None,
                                    origin: None,
                                },
                            );
                        }
                    }
                }
            }
            UpperNode::Dir {
                meta,
                opaque,
                synthetic,
                origin,
            } => {
                if *synthetic {
                    continue;
                }
                match origin {
                    None => {
                        add_change(
                            &mut changes,
                            &mut seen,
                            Change {
                                path: path.clone(),
                                kind: ChangeKind::Created,
                                entry: FileKind::Dir,
                                size: 4096,
                                mode: meta.mode,
                                owner: (meta.uid, meta.gid),
                                blob: None,
                                link_target: None,
                                origin: None,
                            },
                        );
                    }
                    Some(origin) => {
                        if origin.kind == FileKind::Dir {
                            let mode_equal = meta.mode == origin.meta.mode;
                            let owner_equal =
                                (meta.uid, meta.gid) == (origin.meta.uid, origin.meta.gid);
                            if !(mode_equal && owner_equal) {
                                add_change(
                                    &mut changes,
                                    &mut seen,
                                    Change {
                                        path: path.clone(),
                                        kind: ChangeKind::Modified,
                                        entry: FileKind::Dir,
                                        size: 4096,
                                        mode: meta.mode,
                                        owner: (meta.uid, meta.gid),
                                        blob: None,
                                        link_target: None,
                                        origin: Some(origin.clone()),
                                    },
                                );
                            }
                            if *opaque {
                                delete_lower_descendants(
                                    overlay,
                                    path,
                                    &mut changes,
                                    &mut seen,
                                )?;
                            }
                        } else {
                            add_change(
                                &mut changes,
                                &mut seen,
                                deleted_change(path.clone(), origin.clone()),
                            );
                            add_change(
                                &mut changes,
                                &mut seen,
                                Change {
                                    path: path.clone(),
                                    kind: ChangeKind::Created,
                                    entry: FileKind::Dir,
                                    size: 4096,
                                    mode: meta.mode,
                                    owner: (meta.uid, meta.gid),
                                    blob: None,
                                    link_target: None,
                                    origin: None,
                                },
                            );
                        }
                    }
                }
            }
            UpperNode::Symlink {
                target,
                meta,
                origin,
            } => {
                let size = target.as_os_str().len() as u64;
                match origin {
                    None => {
                        add_change(
                            &mut changes,
                            &mut seen,
                            Change {
                                path: path.clone(),
                                kind: ChangeKind::Created,
                                entry: FileKind::Symlink,
                                size,
                                mode: meta.mode,
                                owner: (meta.uid, meta.gid),
                                blob: None,
                                link_target: Some(target.clone()),
                                origin: None,
                            },
                        );
                    }
                    Some(origin) => {
                        if origin.kind == FileKind::Symlink {
                            let mode_equal = meta.mode == origin.meta.mode;
                            let owner_equal =
                                (meta.uid, meta.gid) == (origin.meta.uid, origin.meta.gid);
                            let lower_target = std::fs::read_link(overlay.lower_path(path)).ok();
                            let target_equal = lower_target.as_ref() == Some(target);
                            if !(mode_equal && owner_equal && target_equal) {
                                add_change(
                                    &mut changes,
                                    &mut seen,
                                    Change {
                                        path: path.clone(),
                                        kind: ChangeKind::Modified,
                                        entry: FileKind::Symlink,
                                        size,
                                        mode: meta.mode,
                                        owner: (meta.uid, meta.gid),
                                        blob: None,
                                        link_target: Some(target.clone()),
                                        origin: Some(origin.clone()),
                                    },
                                );
                            }
                        } else {
                            add_change(
                                &mut changes,
                                &mut seen,
                                deleted_change(path.clone(), origin.clone()),
                            );
                            if origin.kind == FileKind::Dir {
                                delete_lower_descendants(
                                    overlay,
                                    path,
                                    &mut changes,
                                    &mut seen,
                                )?;
                            }
                            add_change(
                                &mut changes,
                                &mut seen,
                                Change {
                                    path: path.clone(),
                                    kind: ChangeKind::Created,
                                    entry: FileKind::Symlink,
                                    size,
                                    mode: meta.mode,
                                    owner: (meta.uid, meta.gid),
                                    blob: None,
                                    link_target: Some(target.clone()),
                                    origin: None,
                                },
                            );
                        }
                    }
                }
            }
            UpperNode::Whiteout { origin } => {
                add_change(
                    &mut changes,
                    &mut seen,
                    deleted_change(path.clone(), origin.clone()),
                );
                if origin.kind == FileKind::Dir {
                    delete_lower_descendants(
                        overlay,
                        path,
                        &mut changes,
                        &mut seen,
                    )?;
                }
            }
        }
    }

    changes.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| kind_order(a.kind).cmp(&kind_order(b.kind)))
    });

    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    struct TestEnv {
        lower_dir: TempDir,
        _upper_dir: TempDir,
        overlay: Overlay,
    }

    impl TestEnv {
        fn new() -> Self {
            let lower_dir = TempDir::new().unwrap();
            let upper_dir = TempDir::new().unwrap();
            let overlay = Overlay::new(lower_dir.path(), upper_dir.path()).unwrap();
            TestEnv {
                lower_dir,
                _upper_dir: upper_dir,
                overlay,
            }
        }
    }

    #[test]
    fn created_modified_deleted() {
        let mut env = TestEnv::new();
        let lower_h = env.lower_dir.path().join("h");
        std::fs::create_dir_all(lower_h.join(".local/bin")).unwrap();
        std::fs::write(lower_h.join(".bashrc"), b"# bashrc\n").unwrap();
        std::fs::write(lower_h.join("old.conf"), b"old\n").unwrap();

        let mut f = env
            .overlay
            .create_file(Path::new("/h/.local/bin/app"), 0o755, 1000, 1000)
            .unwrap();
        f.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
        drop(f);

        let mut f = env
            .overlay
            .open_write(Path::new("/h/.bashrc"), false)
            .unwrap();
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(b"export PATH=$PATH:/h/.local/bin\n").unwrap();
        drop(f);

        env.overlay.unlink(Path::new("/h/old.conf")).unwrap();

        let changes = collect(&env.overlay).unwrap();
        assert_eq!(changes.len(), 3);

        assert_eq!(changes[0].path, Path::new("/h/.bashrc"));
        assert_eq!(changes[0].kind, ChangeKind::Modified);

        assert_eq!(changes[1].path, Path::new("/h/.local/bin/app"));
        assert_eq!(changes[1].kind, ChangeKind::Created);
        assert_eq!(changes[1].mode, 0o755);

        assert_eq!(changes[2].path, Path::new("/h/old.conf"));
        assert_eq!(changes[2].kind, ChangeKind::Deleted);
    }

    #[test]
    fn transient_and_identical_are_not_reported() {
        let mut env = TestEnv::new();
        let lower_h = env.lower_dir.path().join("h");
        std::fs::create_dir_all(&lower_h).unwrap();
        let bashrc_content = b"# my bashrc\n";
        std::fs::write(lower_h.join(".bashrc"), bashrc_content).unwrap();

        let mut f = env
            .overlay
            .create_file(Path::new("/h/.tmp-x"), 0o644, 1000, 1000)
            .unwrap();
        f.write_all(b"temporary").unwrap();
        drop(f);
        env.overlay.unlink(Path::new("/h/.tmp-x")).unwrap();

        let mut f = env
            .overlay
            .open_write(Path::new("/h/.bashrc"), true)
            .unwrap();
        f.write_all(bashrc_content).unwrap();
        drop(f);

        let changes = collect(&env.overlay).unwrap();
        assert!(changes.is_empty(), "expected empty changes, got {:?}", changes);
    }

    #[test]
    fn deleted_dir_expands_subtree() {
        let mut env = TestEnv::new();
        let lower_d = env.lower_dir.path().join("h").join("d");
        std::fs::create_dir_all(lower_d.join("y")).unwrap();
        std::fs::write(lower_d.join("x"), b"file x").unwrap();
        std::fs::write(lower_d.join("y").join("z"), b"file z").unwrap();

        env.overlay.unlink(Path::new("/h/d/x")).unwrap();
        env.overlay.unlink(Path::new("/h/d/y/z")).unwrap();
        env.overlay.rmdir(Path::new("/h/d/y")).unwrap();
        env.overlay.rmdir(Path::new("/h/d")).unwrap();

        let changes = collect(&env.overlay).unwrap();
        let expected_paths = vec![
            PathBuf::from("/h/d"),
            PathBuf::from("/h/d/x"),
            PathBuf::from("/h/d/y"),
            PathBuf::from("/h/d/y/z"),
        ];
        let actual_paths: Vec<PathBuf> = changes.iter().map(|c| c.path.clone()).collect();
        assert_eq!(actual_paths, expected_paths);
        for c in &changes {
            assert_eq!(c.kind, ChangeKind::Deleted);
        }
    }

    #[test]
    fn kind_change_is_delete_plus_create() {
        let mut env = TestEnv::new();
        let lower_h = env.lower_dir.path().join("h");
        std::fs::create_dir_all(&lower_h).unwrap();
        std::fs::write(lower_h.join("p"), b"original file").unwrap();

        env.overlay.unlink(Path::new("/h/p")).unwrap();
        env.overlay.mkdir(Path::new("/h/p"), 0o755, 1000, 1000).unwrap();

        let changes = collect(&env.overlay).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, Path::new("/h/p"));
        assert_eq!(changes[0].kind, ChangeKind::Deleted);
        assert_eq!(changes[0].entry, FileKind::File);

        assert_eq!(changes[1].path, Path::new("/h/p"));
        assert_eq!(changes[1].kind, ChangeKind::Created);
        assert_eq!(changes[1].entry, FileKind::Dir);
    }

    #[test]
    fn synthetic_dir_ignored() {
        let mut env = TestEnv::new();
        let run = env.lower_dir.path().join("run");
        std::fs::create_dir_all(&run).unwrap();

        env.overlay
            .add_synthetic_dir(Path::new("/run/installctl"))
            .unwrap();

        let changes = collect(&env.overlay).unwrap();
        assert!(changes.is_empty(), "expected empty changes, got {:?}", changes);
    }

    #[test]
    fn modified_file_mode_and_owner_only() {
        use crate::overlay::SetAttr;

        let mut env = TestEnv::new();
        let lower = env.lower_dir.path();
        std::fs::write(lower.join("f1"), b"hello").unwrap();
        std::fs::write(lower.join("f2"), b"world").unwrap();

        // Change mode of f1
        env.overlay
            .setattr(
                Path::new("/f1"),
                &SetAttr {
                    mode: Some(0o700),
                    ..Default::default()
                },
            )
            .unwrap();

        // Change uid/gid of f2
        env.overlay
            .setattr(
                Path::new("/f2"),
                &SetAttr {
                    uid: Some(2000),
                    gid: Some(3000),
                    ..Default::default()
                },
            )
            .unwrap();

        let changes = collect(&env.overlay).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, Path::new("/f1"));
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].mode, 0o700);

        assert_eq!(changes[1].path, Path::new("/f2"));
        assert_eq!(changes[1].kind, ChangeKind::Modified);
        assert_eq!(changes[1].owner, (2000, 3000));
    }

    #[test]
    fn symlink_created_modified_dropped() {
        let mut env = TestEnv::new();
        let lower = env.lower_dir.path();
        std::os::unix::fs::symlink(Path::new("target1"), lower.join("s1")).unwrap();
        std::os::unix::fs::symlink(Path::new("target2"), lower.join("s2")).unwrap();

        // Create new symlink s0
        env.overlay
            .symlink(Path::new("/s0"), Path::new("target0"), 1000, 1000)
            .unwrap();

        // Modify s1 by unlinking and recreating with target_new
        env.overlay.unlink(Path::new("/s1")).unwrap();
        env.overlay
            .symlink(Path::new("/s1"), Path::new("target_new"), 1000, 1000)
            .unwrap();

        // Recreate s2 with identical target
        env.overlay.unlink(Path::new("/s2")).unwrap();
        env.overlay
            .symlink(Path::new("/s2"), Path::new("target2"), 1000, 1000)
            .unwrap();

        let changes = collect(&env.overlay).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, Path::new("/s0"));
        assert_eq!(changes[0].kind, ChangeKind::Created);
        assert_eq!(changes[0].link_target, Some(PathBuf::from("target0")));

        assert_eq!(changes[1].path, Path::new("/s1"));
        assert_eq!(changes[1].kind, ChangeKind::Modified);
        assert_eq!(changes[1].link_target, Some(PathBuf::from("target_new")));
    }

    #[test]
    fn opaque_dir_deletes_lower_children_without_nodes() {
        let mut env = TestEnv::new();
        let lower_d = env.lower_dir.path().join("d");
        std::fs::create_dir_all(&lower_d).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&lower_d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(lower_d.join("a"), b"child a").unwrap();
        std::fs::write(lower_d.join("b"), b"child b").unwrap();

        // Unlink children and rmdir d
        env.overlay.unlink(Path::new("/d/a")).unwrap();
        env.overlay.unlink(Path::new("/d/b")).unwrap();
        env.overlay.rmdir(Path::new("/d")).unwrap();

        // Recreate /d -> becomes opaque dir with origin
        env.overlay.mkdir(Path::new("/d"), 0o755, 1000, 1000).unwrap();

        // Create new file /d/b in upper
        let mut f = env
            .overlay
            .create_file(Path::new("/d/b"), 0o644, 1000, 1000)
            .unwrap();
        f.write_all(b"new b").unwrap();
        drop(f);

        let changes = collect(&env.overlay).unwrap();
        // /d has same mode/owner as lower dir -> not reported as Modified
        // /d/a was in lower, has no upper node -> Deleted
        // /d/b was created in upper -> Created
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, Path::new("/d/a"));
        assert_eq!(changes[0].kind, ChangeKind::Deleted);

        assert_eq!(changes[1].path, Path::new("/d/b"));
        assert_eq!(changes[1].kind, ChangeKind::Created);
    }

    #[test]
    fn dir_metadata_modified() {
        use crate::overlay::SetAttr;

        let mut env = TestEnv::new();
        let lower_d = env.lower_dir.path().join("d");
        std::fs::create_dir_all(&lower_d).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&lower_d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Modify mode of /d
        env.overlay
            .setattr(
                Path::new("/d"),
                &SetAttr {
                    mode: Some(0o700),
                    ..Default::default()
                },
            )
            .unwrap();

        let changes = collect(&env.overlay).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, Path::new("/d"));
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].entry, FileKind::Dir);
        assert_eq!(changes[0].mode, 0o700);
    }

    #[test]
    fn identical_large_file_dropped() {
        let mut env = TestEnv::new();
        let lower = env.lower_dir.path();
        // Create 32KB file in lower
        let large_content = vec![0x42u8; 32768];
        std::fs::write(lower.join("large.bin"), &large_content).unwrap();

        // Rewrite identical content in overlay
        let mut f = env
            .overlay
            .open_write(Path::new("/large.bin"), true)
            .unwrap();
        f.write_all(&large_content).unwrap();
        drop(f);

        let changes = collect(&env.overlay).unwrap();
        assert!(
            changes.is_empty(),
            "identical large file should be dropped, got: {:?}",
            changes
        );
    }
}
