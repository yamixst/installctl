use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType,
    Filesystem, FopenFlags, Generation, INodeNo, LockOwner, OpenAccMode, OpenFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, ReplyXattr, Request, Session, SessionACL, TimeOrNow, WriteFlags,
};

use crate::overlay::{normalize_path, Attr, FileKind, Overlay, RenameFlags, SetAttr};

const TTL: Duration = Duration::from_secs(1);

struct OpenHandle {
    file: Arc<File>,
    #[allow(dead_code)]
    is_write: bool,
    path: PathBuf,
}

pub struct FsState {
    pub(crate) overlay: Arc<Mutex<Overlay>>,
    pub(crate) ino_to_path: HashMap<INodeNo, PathBuf>,
    pub(crate) path_to_ino: HashMap<PathBuf, INodeNo>,
    pub(crate) lookup_counts: HashMap<INodeNo, u64>,
    open_handles: HashMap<FileHandle, OpenHandle>,
    open_dir_handles: HashSet<FileHandle>,
    next_ino: u64,
    next_fh: u64,
}

impl FsState {
    fn get_or_create_ino(&mut self, path: &Path) -> INodeNo {
        let norm = normalize_path(path);
        if let Some(&ino) = self.path_to_ino.get(&norm) {
            ino
        } else {
            let ino = INodeNo(self.next_ino);
            self.next_ino += 1;
            self.path_to_ino.insert(norm.clone(), ino);
            self.ino_to_path.insert(ino, norm);
            ino
        }
    }
}

pub struct OverlayFs {
    state: Arc<Mutex<FsState>>,
}

pub fn new_fs(overlay: Overlay) -> (OverlayFs, Arc<Mutex<Overlay>>) {
    let overlay_arc = Arc::new(Mutex::new(overlay));
    let mut ino_to_path = HashMap::new();
    let mut path_to_ino = HashMap::new();
    let mut lookup_counts = HashMap::new();

    ino_to_path.insert(INodeNo::ROOT, PathBuf::from("/"));
    path_to_ino.insert(PathBuf::from("/"), INodeNo::ROOT);
    lookup_counts.insert(INodeNo::ROOT, 1);

    let state = Arc::new(Mutex::new(FsState {
        overlay: Arc::clone(&overlay_arc),
        ino_to_path,
        path_to_ino,
        lookup_counts,
        open_handles: HashMap::new(),
        open_dir_handles: HashSet::new(),
        next_ino: 2,
        next_fh: 1,
    }));

    (OverlayFs { state }, overlay_arc)
}

pub fn mount_fuse_fd(mountpoint: &Path) -> io::Result<OwnedFd> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")?;
    let fd = OwnedFd::from(file);

    let source = CString::new("installctl")
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let target = CString::new(mountpoint.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let fstype = CString::new("fuse")
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let opts = format!(
        "fd={},rootmode=40000,user_id=0,group_id=0,allow_other,default_permissions",
        fd.as_raw_fd()
    );
    let data = CString::new(opts)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let res = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            data.as_ptr() as *const libc::c_void,
        )
    };

    if res != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(fd)
}

pub fn serve(fs: OverlayFs, fd: OwnedFd) -> io::Result<BackgroundSession> {
    Session::from_fd(fs, fd, SessionACL::All, Config::default())?.spawn()
}

fn attr_to_fileattr(attr: &Attr, ino: INodeNo) -> FileAttr {
    let uid = attr.meta.uid;
    let gid = attr.meta.gid;

    let kind = match attr.kind {
        FileKind::File => FileType::RegularFile,
        FileKind::Dir => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
        FileKind::Other => FileType::RegularFile,
    };
    let atime = UNIX_EPOCH + Duration::from_nanos(attr.meta.atime_ns.max(0) as u64);
    let mtime = UNIX_EPOCH + Duration::from_nanos(attr.meta.mtime_ns.max(0) as u64);
    let ctime = mtime;
    let blocks = attr.size.div_ceil(512);
    FileAttr {
        ino,
        size: attr.size,
        blocks,
        atime,
        mtime,
        ctime,
        crtime: UNIX_EPOCH,
        kind,
        perm: (attr.meta.mode & 0o7777) as u16,
        nlink: attr.nlink,
        uid,
        gid,
        rdev: attr.rdev as u32,
        blksize: 4096,
        flags: 0,
    }
}

fn time_or_now_to_ns(t: Option<TimeOrNow>) -> Option<i128> {
    match t {
        Some(TimeOrNow::Now) => Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as i128)
                .unwrap_or(0),
        ),
        Some(TimeOrNow::SpecificTime(st)) => Some(
            st.duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as i128)
                .unwrap_or(0),
        ),
        None => None,
    }
}

fn child_path(parent_path: &Path, name: &OsStr) -> PathBuf {
    let rel = Path::new(name);
    normalize_path(&if parent_path == Path::new("/") {
        Path::new("/").join(rel)
    } else {
        parent_path.join(rel)
    })
}

impl Filesystem for OverlayFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let mut state = self.state.lock().unwrap();
        let parent_path = match state.ino_to_path.get(&parent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = child_path(&parent_path, name);
        let overlay = Arc::clone(&state.overlay);
        let attr_res = overlay.lock().unwrap().stat(&path);
        match attr_res {
            Ok(attr) => {
                let ino = state.get_or_create_ino(&path);
                *state.lookup_counts.entry(ino).or_insert(0) += 1;
                let file_attr = attr_to_fileattr(&attr, ino);
                reply.entry(&TTL, &file_attr, Generation(0));
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(count) = state.lookup_counts.get_mut(&ino) {
            *count = count.saturating_sub(nlookup);
            if *count == 0 && ino != INodeNo::ROOT {
                state.lookup_counts.remove(&ino);
                if let Some(path) = state.ino_to_path.remove(&ino)
                    && state.path_to_ino.get(&path) == Some(&ino)
                {
                    state.path_to_ino.remove(&path);
                }
            }
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let state = self.state.lock().unwrap();
        let path = match state.ino_to_path.get(&ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let overlay = Arc::clone(&state.overlay);
        drop(state);

        match overlay.lock().unwrap().stat(&path) {
            Ok(attr) => {
                let file_attr = attr_to_fileattr(&attr, ino);
                reply.attr(&TTL, &file_attr);
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let state = self.state.lock().unwrap();
        let path = match state.ino_to_path.get(&ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let overlay = Arc::clone(&state.overlay);
        drop(state);

        let set = SetAttr {
            mode,
            uid,
            gid,
            size,
            atime_ns: time_or_now_to_ns(atime),
            mtime_ns: time_or_now_to_ns(mtime),
        };

        match overlay.lock().unwrap().setattr(&path, &set) {
            Ok(attr) => {
                let file_attr = attr_to_fileattr(&attr, ino);
                reply.attr(&TTL, &file_attr);
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let state = self.state.lock().unwrap();
        let path = match state.ino_to_path.get(&ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let overlay = Arc::clone(&state.overlay);
        drop(state);

        match overlay.lock().unwrap().read_link(&path) {
            Ok(target) => {
                reply.data(target.as_os_str().as_bytes());
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        let file_type = mode & libc::S_IFMT;
        if file_type != libc::S_IFREG && file_type != 0 {
            reply.error(Errno::EPERM);
            return;
        }

        let mut state = self.state.lock().unwrap();
        let parent_path = match state.ino_to_path.get(&parent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = child_path(&parent_path, name);
        let overlay = Arc::clone(&state.overlay);

        let eff_mode = (mode & !umask) & 0o7777;
        let mut overlay_guard = overlay.lock().unwrap();
        match overlay_guard.create_file(&path, eff_mode, req.uid(), req.gid()) {
            Ok(_) => {
                let ino = state.get_or_create_ino(&path);
                *state.lookup_counts.entry(ino).or_insert(0) += 1;
                let attr = overlay_guard.stat(&path).expect("stat after create_file");
                let file_attr = attr_to_fileattr(&attr, ino);
                reply.entry(&TTL, &file_attr, Generation(0));
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let mut state = self.state.lock().unwrap();
        let parent_path = match state.ino_to_path.get(&parent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = child_path(&parent_path, name);
        let overlay = Arc::clone(&state.overlay);

        let eff_mode = (mode & !umask) & 0o7777;
        let mut overlay_guard = overlay.lock().unwrap();
        match overlay_guard.mkdir(&path, eff_mode, req.uid(), req.gid()) {
            Ok(()) => {
                let ino = state.get_or_create_ino(&path);
                *state.lookup_counts.entry(ino).or_insert(0) += 1;
                let attr = overlay_guard.stat(&path).expect("stat after mkdir");
                let file_attr = attr_to_fileattr(&attr, ino);
                reply.entry(&TTL, &file_attr, Generation(0));
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut state = self.state.lock().unwrap();
        let parent_path = match state.ino_to_path.get(&parent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = child_path(&parent_path, name);
        let overlay = Arc::clone(&state.overlay);

        match overlay.lock().unwrap().unlink(&path) {
            Ok(()) => {
                state.path_to_ino.remove(&path);
                reply.ok();
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut state = self.state.lock().unwrap();
        let parent_path = match state.ino_to_path.get(&parent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = child_path(&parent_path, name);
        let overlay = Arc::clone(&state.overlay);

        match overlay.lock().unwrap().rmdir(&path) {
            Ok(()) => {
                state.path_to_ino.remove(&path);
                state.path_to_ino.retain(|p, _| !p.starts_with(&path));
                reply.ok();
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let mut state = self.state.lock().unwrap();
        let parent_path = match state.ino_to_path.get(&parent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = child_path(&parent_path, link_name);
        let overlay = Arc::clone(&state.overlay);

        let mut overlay_guard = overlay.lock().unwrap();
        match overlay_guard.symlink(&path, target, req.uid(), req.gid()) {
            Ok(()) => {
                let ino = state.get_or_create_ino(&path);
                *state.lookup_counts.entry(ino).or_insert(0) += 1;
                let attr = overlay_guard.stat(&path).expect("stat after symlink");
                let file_attr = attr_to_fileattr(&attr, ino);
                reply.entry(&TTL, &file_attr, Generation(0));
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let mut state = self.state.lock().unwrap();
        let old_parent = match state.ino_to_path.get(&parent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let new_parent = match state.ino_to_path.get(&newparent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let from_path = child_path(&old_parent, name);
        let to_path = child_path(&new_parent, newname);

        let rename_flags = RenameFlags {
            noreplace: flags.contains(fuser::RenameFlags::RENAME_NOREPLACE),
            exchange: flags.contains(fuser::RenameFlags::RENAME_EXCHANGE),
        };

        let overlay = Arc::clone(&state.overlay);
        match overlay
            .lock()
            .unwrap()
            .rename(&from_path, &to_path, rename_flags)
        {
            Ok(()) => {
                let from_ino = state.path_to_ino.remove(&from_path);
                state.path_to_ino.remove(&to_path);
                if let Some(ino) = from_ino {
                    state.ino_to_path.insert(ino, to_path.clone());
                    state.path_to_ino.insert(to_path.clone(), ino);
                }

                // Update descendants if directory
                let mut updates = Vec::new();
                for (p, &ino) in &state.path_to_ino {
                    if p.starts_with(&from_path)
                        && p != &from_path
                        && let Ok(rel) = p.strip_prefix(&from_path)
                    {
                        let new_p = to_path.join(rel);
                        updates.push((p.clone(), new_p, ino));
                    }
                }
                for (old_p, new_p, ino) in updates {
                    state.path_to_ino.remove(&old_p);
                    state.path_to_ino.remove(&new_p);
                    state.path_to_ino.insert(new_p.clone(), ino);
                    state.ino_to_path.insert(ino, new_p);
                }

                // Update open handles pointing to this path
                for handle in state.open_handles.values_mut() {
                    if handle.path == from_path {
                        handle.path = to_path.clone();
                    } else if handle.path.starts_with(&from_path)
                        && let Ok(rel) = handle.path.strip_prefix(&from_path)
                    {
                        handle.path = to_path.join(rel);
                    }
                }

                reply.ok();
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let mut state = self.state.lock().unwrap();
        let existing = match state.ino_to_path.get(&ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let new_parent = match state.ino_to_path.get(&newparent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let new_path = child_path(&new_parent, newname);
        let overlay = Arc::clone(&state.overlay);

        let mut overlay_guard = overlay.lock().unwrap();
        match overlay_guard.link(&existing, &new_path) {
            Ok(()) => {
                let new_ino = state.get_or_create_ino(&new_path);
                *state.lookup_counts.entry(new_ino).or_insert(0) += 1;
                let attr = overlay_guard.stat(&new_path).expect("stat after link");
                let file_attr = attr_to_fileattr(&attr, new_ino);
                reply.entry(&TTL, &file_attr, Generation(0));
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let mut state = self.state.lock().unwrap();
        let path = match state.ino_to_path.get(&ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let overlay = Arc::clone(&state.overlay);

        let is_write = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let truncate = flags.0 & libc::O_TRUNC != 0;

        let file_res = if is_write {
            overlay.lock().unwrap().open_write(&path, truncate)
        } else {
            overlay.lock().unwrap().open_read(&path)
        };

        match file_res {
            Ok(file) => {
                let fh = FileHandle(state.next_fh);
                state.next_fh += 1;
                state.open_handles.insert(
                    fh,
                    OpenHandle {
                        file: Arc::new(file),
                        is_write,
                        path,
                    },
                );
                reply.opened(fh, FopenFlags::empty());
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let mut state = self.state.lock().unwrap();
        let parent_path = match state.ino_to_path.get(&parent).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let path = child_path(&parent_path, name);
        let overlay = Arc::clone(&state.overlay);

        let eff_mode = (mode & !umask) & 0o7777;
        let mut overlay_guard = overlay.lock().unwrap();

        let exists = overlay_guard.stat(&path).is_ok();
        let file_res = if exists {
            if flags & libc::O_EXCL != 0 {
                Err(io::Error::from_raw_os_error(libc::EEXIST))
            } else {
                let truncate = flags & libc::O_TRUNC != 0;
                overlay_guard.open_write(&path, truncate)
            }
        } else {
            overlay_guard.create_file(&path, eff_mode, req.uid(), req.gid())
        };

        match file_res {
            Ok(file) => {
                let ino = state.get_or_create_ino(&path);
                *state.lookup_counts.entry(ino).or_insert(0) += 1;
                let fh = FileHandle(state.next_fh);
                state.next_fh += 1;
                let attr = overlay_guard.stat(&path).expect("stat after create");
                let file_attr = attr_to_fileattr(&attr, ino);
                state.open_handles.insert(
                    fh,
                    OpenHandle {
                        file: Arc::new(file),
                        is_write: true,
                        path,
                    },
                );
                reply.created(&TTL, &file_attr, Generation(0), fh, FopenFlags::empty());
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let file = {
            let state = self.state.lock().unwrap();
            state.open_handles.get(&fh).map(|h| Arc::clone(&h.file))
        };

        match file {
            Some(file) => {
                let mut buf = vec![0u8; size as usize];
                match file.read_at(&mut buf, offset) {
                    Ok(n) => reply.data(&buf[..n]),
                    Err(e) => reply.error(e.into()),
                }
            }
            None => reply.error(Errno::EBADF),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let file = {
            let state = self.state.lock().unwrap();
            state.open_handles.get(&fh).map(|h| Arc::clone(&h.file))
        };

        match file {
            Some(file) => match file.write_all_at(data, offset) {
                Ok(()) => reply.written(data.len() as u32),
                Err(e) => reply.error(e.into()),
            },
            None => reply.error(Errno::EBADF),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let file = {
            let state = self.state.lock().unwrap();
            state.open_handles.get(&fh).map(|h| Arc::clone(&h.file))
        };

        if let Some(file) = file {
            let _ = file.sync_data();
        }
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let file = {
            let state = self.state.lock().unwrap();
            state.open_handles.get(&fh).map(|h| Arc::clone(&h.file))
        };

        match file {
            Some(file) => {
                let res = if datasync {
                    file.sync_data()
                } else {
                    file.sync_all()
                };
                match res {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(e.into()),
                }
            }
            None => reply.error(Errno::EBADF),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut state = self.state.lock().unwrap();
        state.open_handles.remove(&fh);
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let mut state = self.state.lock().unwrap();
        let path = match state.ino_to_path.get(&ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let overlay = Arc::clone(&state.overlay);

        match overlay.lock().unwrap().stat(&path) {
            Ok(attr) => {
                if attr.kind != FileKind::Dir {
                    reply.error(Errno::ENOTDIR);
                } else {
                    let fh = FileHandle(state.next_fh);
                    state.next_fh += 1;
                    state.open_dir_handles.insert(fh);
                    reply.opened(fh, FopenFlags::empty());
                }
            }
            Err(e) => reply.error(e.into()),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut state = self.state.lock().unwrap();
        let dir_path = match state.ino_to_path.get(&ino).cloned() {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let overlay = Arc::clone(&state.overlay);

        let entries_res = overlay.lock().unwrap().read_dir(&dir_path);
        match entries_res {
            Ok(entries) => {
                let parent_ino = if dir_path == Path::new("/") {
                    INodeNo::ROOT
                } else if let Some(parent) = dir_path.parent() {
                    state.get_or_create_ino(parent)
                } else {
                    INodeNo::ROOT
                };

                let mut full_entries = Vec::with_capacity(entries.len() + 2);
                full_entries.push((ino, FileType::Directory, OsString::from(".")));
                full_entries.push((parent_ino, FileType::Directory, OsString::from("..")));

                for (name, kind) in entries {
                    let child_p = child_path(&dir_path, &name);
                    let child_ino = state.get_or_create_ino(&child_p);
                    let ftype = match kind {
                        FileKind::File => FileType::RegularFile,
                        FileKind::Dir => FileType::Directory,
                        FileKind::Symlink => FileType::Symlink,
                        FileKind::Other => FileType::RegularFile,
                    };
                    full_entries.push((child_ino, ftype, name));
                }

                for (i, (e_ino, e_kind, e_name)) in
                    full_entries.into_iter().enumerate().skip(offset as usize)
                {
                    if reply.add(e_ino, (i + 1) as u64, e_kind, &e_name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => {
                reply.error(e.into());
            }
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let mut state = self.state.lock().unwrap();
        state.open_dir_handles.remove(&fh);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let root = CString::new("/").unwrap();
        let ret = unsafe { libc::statvfs(root.as_ptr(), &mut stat) };
        if ret != 0 {
            reply.error(io::Error::last_os_error().into());
        } else {
            reply.statfs(
                stat.f_blocks,
                stat.f_bfree,
                stat.f_bavail,
                stat.f_files,
                stat.f_ffree,
                stat.f_bsize as u32,
                stat.f_namemax as u32,
                stat.f_frsize as u32,
            );
        }
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::NO_XATTR);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOTSUP);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOTSUP);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }
}
