use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub mode: u32, /* permission bits incl. suid/sgid/sticky, no type bits */
    pub uid: u32,
    pub gid: u32,
    pub atime_ns: i128,
    pub mtime_ns: i128,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LowerStamp {
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ns: i128,
    pub dev: u64,
    pub ino: u64,
    pub meta: Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UpperNode {
    File {
        blob: PathBuf,
        meta: Meta,
        origin: Option<LowerStamp>,
    },
    Dir {
        meta: Meta,
        opaque: bool,
        synthetic: bool,
        origin: Option<LowerStamp>,
    },
    Symlink {
        target: PathBuf,
        meta: Meta,
        origin: Option<LowerStamp>,
    },
    Whiteout {
        origin: LowerStamp,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub kind: FileKind,
    pub size: u64,
    pub nlink: u32,
    pub meta: Meta,
    pub rdev: u64,
}

pub struct Overlay {
    lower_root: PathBuf,
    upper_dir: PathBuf,
    blob_seq: u64,
    nodes: BTreeMap<PathBuf, UpperNode>,
}

pub fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::RootDir => {
                normalized.push("/");
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized != Path::new("/") {
                    normalized.pop();
                    if normalized.as_os_str().is_empty() {
                        normalized.push("/");
                    }
                }
            }
            Component::Normal(c) => {
                normalized.push(c);
            }
            Component::Prefix(_) => {}
        }
    }
    if !normalized.starts_with("/") {
        normalized = Path::new("/").join(normalized);
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        normalized
    }
}

fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

impl Overlay {
    pub fn new(lower_root: &Path, upper_dir: &Path) -> io::Result<Overlay> {
        let blobs_dir = upper_dir.join("blobs");
        std::fs::create_dir_all(&blobs_dir)?;

        let mut max_seq = 0u64;
        if let Ok(entries) = std::fs::read_dir(&blobs_dir) {
            for entry in entries.flatten() {
                if let Ok(n) = entry.file_name().to_string_lossy().parse::<u64>()
                    && n > max_seq
                {
                    max_seq = n;
                }
            }
        }

        Ok(Overlay {
            lower_root: lower_root.to_path_buf(),
            upper_dir: upper_dir.to_path_buf(),
            blob_seq: max_seq,
            nodes: BTreeMap::new(),
        })
    }

    pub fn lower_root(&self) -> &Path {
        &self.lower_root
    }

    pub fn nodes(&self) -> &BTreeMap<PathBuf, UpperNode> {
        &self.nodes
    }

    fn lower_path(&self, path: &Path) -> PathBuf {
        let rel = path.strip_prefix("/").unwrap_or(path);
        self.lower_root.join(rel)
    }

    fn is_lower_ancestor_hidden(&self, path: &Path) -> bool {
        let mut curr = path.parent();
        while let Some(parent) = curr {
            if let Some(node) = self.nodes.get(parent) {
                match node {
                    UpperNode::Whiteout { .. } => return true,
                    UpperNode::Dir { opaque, .. } => {
                        if *opaque {
                            return true;
                        }
                    }
                    UpperNode::File { .. } | UpperNode::Symlink { .. } => return true,
                }
            }
            if parent == Path::new("/") {
                break;
            }
            curr = parent.parent();
        }
        false
    }

    fn stat_lower(&self, path: &Path) -> io::Result<LowerStamp> {
        let lower_path = self.lower_path(path);
        let meta = std::fs::symlink_metadata(&lower_path)
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error().unwrap_or(libc::ENOENT)))?;
        let kind = if meta.file_type().is_file() {
            FileKind::File
        } else if meta.file_type().is_dir() {
            FileKind::Dir
        } else if meta.file_type().is_symlink() {
            FileKind::Symlink
        } else {
            FileKind::Other
        };
        let atime_ns = (meta.atime() as i128) * 1_000_000_000 + (meta.atime_nsec() as i128);
        let mtime_ns = (meta.mtime() as i128) * 1_000_000_000 + (meta.mtime_nsec() as i128);
        let meta_obj = Meta {
            mode: meta.mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
            atime_ns,
            mtime_ns,
        };
        Ok(LowerStamp {
            kind,
            size: meta.len(),
            mtime_ns,
            dev: meta.dev(),
            ino: meta.ino(),
            meta: meta_obj,
        })
    }

    pub fn stat(&self, path: &Path) -> io::Result<Attr> {
        let path = normalize_path(path);
        if let Some(node) = self.nodes.get(&path) {
            match node {
                UpperNode::Whiteout { .. } => Err(io::Error::from_raw_os_error(libc::ENOENT)),
                UpperNode::File { blob, meta, .. } => {
                    let size = std::fs::metadata(blob).map(|m| m.len()).unwrap_or(0);
                    Ok(Attr {
                        kind: FileKind::File,
                        size,
                        nlink: 1,
                        meta: meta.clone(),
                        rdev: 0,
                    })
                }
                UpperNode::Dir { meta, .. } => Ok(Attr {
                    kind: FileKind::Dir,
                    size: 4096,
                    nlink: 2,
                    meta: meta.clone(),
                    rdev: 0,
                }),
                UpperNode::Symlink { target, meta, .. } => Ok(Attr {
                    kind: FileKind::Symlink,
                    size: target.as_os_str().len() as u64,
                    nlink: 1,
                    meta: meta.clone(),
                    rdev: 0,
                }),
            }
        } else {
            if self.is_lower_ancestor_hidden(&path) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            let lower_path = self.lower_path(&path);
            let m = std::fs::symlink_metadata(&lower_path).map_err(|e| {
                io::Error::from_raw_os_error(e.raw_os_error().unwrap_or(libc::ENOENT))
            })?;
            let kind = if m.file_type().is_file() {
                FileKind::File
            } else if m.file_type().is_dir() {
                FileKind::Dir
            } else if m.file_type().is_symlink() {
                FileKind::Symlink
            } else {
                FileKind::Other
            };
            let atime_ns = (m.atime() as i128) * 1_000_000_000 + (m.atime_nsec() as i128);
            let mtime_ns = (m.mtime() as i128) * 1_000_000_000 + (m.mtime_nsec() as i128);
            Ok(Attr {
                kind,
                size: m.len(),
                nlink: m.nlink() as u32,
                meta: Meta {
                    mode: m.mode() & 0o7777,
                    uid: m.uid(),
                    gid: m.gid(),
                    atime_ns,
                    mtime_ns,
                },
                rdev: m.rdev(),
            })
        }
    }

    pub fn read_dir(&self, path: &Path) -> io::Result<Vec<(OsString, FileKind)>> {
        let path = normalize_path(path);
        let attr = self.stat(&path)?;
        if attr.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }

        let is_opaque = match self.nodes.get(&path) {
            Some(UpperNode::Dir { opaque, .. }) => *opaque,
            _ => false,
        };

        let mut whited_out = HashSet::new();
        let mut entries = BTreeMap::new();

        for (node_path, node) in &self.nodes {
            if node_path.parent() == Some(&path)
                && let Some(file_name) = node_path.file_name()
            {
                let name = file_name.to_os_string();
                match node {
                    UpperNode::Whiteout { .. } => {
                        whited_out.insert(name);
                    }
                    UpperNode::File { .. } => {
                        entries.insert(name, FileKind::File);
                    }
                    UpperNode::Dir { .. } => {
                        entries.insert(name, FileKind::Dir);
                    }
                    UpperNode::Symlink { .. } => {
                        entries.insert(name, FileKind::Symlink);
                    }
                }
            }
        }

        if !is_opaque && !self.is_lower_ancestor_hidden(&path) {
            let lower_path = self.lower_path(&path);
            if let Ok(rd) = std::fs::read_dir(&lower_path) {
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    if whited_out.contains(&name) || entries.contains_key(&name) {
                        continue;
                    }
                    if let Ok(ft) = entry.file_type() {
                        let kind = if ft.is_file() {
                            FileKind::File
                        } else if ft.is_dir() {
                            FileKind::Dir
                        } else if ft.is_symlink() {
                            FileKind::Symlink
                        } else {
                            FileKind::Other
                        };
                        entries.insert(name, kind);
                    }
                }
            }
        }

        let result: Vec<(OsString, FileKind)> = entries.into_iter().collect();
        Ok(result)
    }

    pub fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        let path = normalize_path(path);
        if let Some(node) = self.nodes.get(&path) {
            match node {
                UpperNode::Symlink { target, .. } => Ok(target.clone()),
                UpperNode::Whiteout { .. } => Err(io::Error::from_raw_os_error(libc::ENOENT)),
                _ => Err(io::Error::from_raw_os_error(libc::EINVAL)),
            }
        } else {
            if self.is_lower_ancestor_hidden(&path) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            let lower_path = self.lower_path(&path);
            std::fs::read_link(&lower_path)
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error().unwrap_or(libc::ENOENT)))
        }
    }

    pub fn open_read(&self, path: &Path) -> io::Result<File> {
        let path = normalize_path(path);
        if let Some(node) = self.nodes.get(&path) {
            match node {
                UpperNode::File { blob, .. } => std::fs::File::open(blob),
                UpperNode::Dir { .. } => Err(io::Error::from_raw_os_error(libc::EISDIR)),
                UpperNode::Symlink { .. } => Err(io::Error::from_raw_os_error(libc::EINVAL)),
                UpperNode::Whiteout { .. } => Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        } else {
            if self.is_lower_ancestor_hidden(&path) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            let lower_path = self.lower_path(&path);
            let m = std::fs::symlink_metadata(&lower_path).map_err(|e| {
                io::Error::from_raw_os_error(e.raw_os_error().unwrap_or(libc::ENOENT))
            })?;
            if m.file_type().is_dir() {
                return Err(io::Error::from_raw_os_error(libc::EISDIR));
            }
            if m.file_type().is_symlink() {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            std::fs::File::open(&lower_path)
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error().unwrap_or(libc::ENOENT)))
        }
    }

    pub fn open_write(&mut self, path: &Path, truncate: bool) -> io::Result<File> {
        let path = normalize_path(path);
        if let Some(node) = self.nodes.get(&path) {
            match node {
                UpperNode::File { blob, .. } => {
                    let blob_clone = blob.clone();
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .truncate(truncate)
                        .open(&blob_clone)
                }
                UpperNode::Dir { .. } => Err(io::Error::from_raw_os_error(libc::EISDIR)),
                UpperNode::Symlink { .. } => Err(io::Error::from_raw_os_error(libc::EINVAL)),
                UpperNode::Whiteout { .. } => Err(io::Error::from_raw_os_error(libc::ENOENT)),
            }
        } else {
            if self.is_lower_ancestor_hidden(&path) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            let stamp = self.stat_lower(&path)?;
            if stamp.kind == FileKind::Dir {
                return Err(io::Error::from_raw_os_error(libc::EISDIR));
            }
            if stamp.kind != FileKind::File {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }

            self.blob_seq += 1;
            let blob_path = self.upper_dir.join("blobs").join(self.blob_seq.to_string());
            let lower_path = self.lower_path(&path);
            std::fs::copy(&lower_path, &blob_path)?;

            let mut perms = std::fs::metadata(&blob_path)?.permissions();
            perms.set_mode(perms.mode() | 0o200);
            std::fs::set_permissions(&blob_path, perms)?;

            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .truncate(truncate)
                .open(&blob_path)?;

            self.nodes.insert(
                path,
                UpperNode::File {
                    blob: blob_path,
                    meta: stamp.meta.clone(),
                    origin: Some(stamp),
                },
            );

            Ok(file)
        }
    }

    pub fn create_file(&mut self, path: &Path, mode: u32, uid: u32, gid: u32) -> io::Result<File> {
        let path = normalize_path(path);
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let parent_attr = self.stat(parent)?;
        if parent_attr.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }

        let origin = if let Some(node) = self.nodes.get(&path) {
            match node {
                UpperNode::Whiteout { origin } => Some(origin.clone()),
                _ => return Err(io::Error::from_raw_os_error(libc::EEXIST)),
            }
        } else {
            if !self.is_lower_ancestor_hidden(&path) {
                let lower_path = self.lower_path(&path);
                if lower_path.symlink_metadata().is_ok() {
                    return Err(io::Error::from_raw_os_error(libc::EEXIST));
                }
            }
            None
        };

        self.blob_seq += 1;
        let blob_path = self.upper_dir.join("blobs").join(self.blob_seq.to_string());
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&blob_path)?;

        let now = now_ns();
        self.nodes.insert(
            path,
            UpperNode::File {
                blob: blob_path,
                meta: Meta {
                    mode: mode & 0o7777,
                    uid,
                    gid,
                    atime_ns: now,
                    mtime_ns: now,
                },
                origin,
            },
        );

        Ok(file)
    }

    pub fn mkdir(&mut self, path: &Path, mode: u32, uid: u32, gid: u32) -> io::Result<()> {
        let path = normalize_path(path);
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let parent_attr = self.stat(parent)?;
        if parent_attr.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }

        let (origin, opaque) = if let Some(node) = self.nodes.get(&path) {
            match node {
                UpperNode::Whiteout { origin } => (Some(origin.clone()), true),
                _ => return Err(io::Error::from_raw_os_error(libc::EEXIST)),
            }
        } else {
            if !self.is_lower_ancestor_hidden(&path) {
                let lower_path = self.lower_path(&path);
                if lower_path.symlink_metadata().is_ok() {
                    return Err(io::Error::from_raw_os_error(libc::EEXIST));
                }
            }
            (None, false)
        };

        let now = now_ns();
        self.nodes.insert(
            path,
            UpperNode::Dir {
                meta: Meta {
                    mode: mode & 0o7777,
                    uid,
                    gid,
                    atime_ns: now,
                    mtime_ns: now,
                },
                opaque,
                synthetic: false,
                origin,
            },
        );

        Ok(())
    }

    pub fn add_synthetic_dir(&mut self, path: &Path) -> io::Result<()> {
        let path = normalize_path(path);
        if path == Path::new("/") {
            return Ok(());
        }

        if let Some(parent) = path.parent()
            && parent != Path::new("/")
        {
            if let Ok(attr) = self.stat(parent) {
                if attr.kind != FileKind::Dir {
                    return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
                }
            } else {
                self.add_synthetic_dir(parent)?;
            }
        }

        if let Some(UpperNode::Dir {
            synthetic: true, ..
        }) = self.nodes.get(&path)
        {
            return Ok(());
        }

        let origin = if !self.is_lower_ancestor_hidden(&path) {
            self.stat_lower(&path).ok()
        } else {
            None
        };

        let now = now_ns();
        self.nodes.insert(
            path,
            UpperNode::Dir {
                meta: Meta {
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    atime_ns: now,
                    mtime_ns: now,
                },
                opaque: false,
                synthetic: true,
                origin,
            },
        );

        Ok(())
    }

    pub fn unlink(&mut self, path: &Path) -> io::Result<()> {
        let path = normalize_path(path);
        if path == Path::new("/") {
            return Err(io::Error::from_raw_os_error(libc::EISDIR));
        }

        if let Some(node) = self.nodes.get(&path) {
            match node {
                UpperNode::Whiteout { .. } => Err(io::Error::from_raw_os_error(libc::ENOENT)),
                UpperNode::Dir { .. } => Err(io::Error::from_raw_os_error(libc::EISDIR)),
                UpperNode::File { blob, origin, .. } => {
                    let _ = std::fs::remove_file(blob);
                    if let Some(origin) = origin.clone() {
                        self.nodes.insert(path, UpperNode::Whiteout { origin });
                    } else {
                        self.nodes.remove(&path);
                    }
                    Ok(())
                }
                UpperNode::Symlink { origin, .. } => {
                    if let Some(origin) = origin.clone() {
                        self.nodes.insert(path, UpperNode::Whiteout { origin });
                    } else {
                        self.nodes.remove(&path);
                    }
                    Ok(())
                }
            }
        } else {
            if self.is_lower_ancestor_hidden(&path) {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            let stamp = self.stat_lower(&path)?;
            if stamp.kind == FileKind::Dir {
                return Err(io::Error::from_raw_os_error(libc::EISDIR));
            }
            self.nodes
                .insert(path, UpperNode::Whiteout { origin: stamp });
            Ok(())
        }
    }

    pub fn rmdir(&mut self, path: &Path) -> io::Result<()> {
        let path = normalize_path(path);
        if path == Path::new("/") {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }

        let attr = self.stat(&path)?;
        if attr.kind != FileKind::Dir {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }

        let entries = self.read_dir(&path)?;
        if !entries.is_empty() {
            return Err(io::Error::from_raw_os_error(libc::ENOTEMPTY));
        }

        let origin = if let Some(node) = self.nodes.get(&path) {
            match node {
                UpperNode::Dir { origin, .. } => origin.clone(),
                _ => None,
            }
        } else {
            self.stat_lower(&path).ok()
        };

        if let Some(origin) = origin {
            self.nodes
                .insert(path.clone(), UpperNode::Whiteout { origin });
        } else {
            self.nodes.remove(&path);
        }

        self.nodes
            .retain(|k, _| !(k.starts_with(&path) && k != &path));

        Ok(())
    }
}
