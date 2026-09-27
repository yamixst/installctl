use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use sha2::{Digest, Sha256};

use crate::actions;
use crate::apply::Original;
use crate::changes::{Change, ChangeKind};
use crate::control::{ActionRecord, Decision};
use crate::overlay::FileKind;
use crate::prompt::Prompter;

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    CreatedFile {
        path: PathBuf,
        sha256: String,
    },
    CreatedSymlink {
        path: PathBuf,
        target: PathBuf,
    },
    CreatedDir {
        path: PathBuf,
    },
    ModifiedPatch {
        path: PathBuf,
        blob: usize,
        installed_sha256: String,
        mode: u32,
    },
    ModifiedRestore {
        path: PathBuf,
        blob: usize,
        installed_sha256: String,
        mode: u32,
    },
    DeletedFile {
        path: PathBuf,
        blob: usize,
        mode: u32,
    },
    DeletedSymlink {
        path: PathBuf,
        target: PathBuf,
    },
    DeletedDir {
        path: PathBuf,
        mode: u32,
    },
    Action {
        as_root: bool,
        command: String,
        inverse: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct UninstallData {
    pub name: String,
    pub items: Vec<Item>,
    pub blobs: Vec<Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RenderError {
    UnsupportedPath(PathBuf),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::UnsupportedPath(p) => {
                write!(f, "unsupported path with tab or newline: {}", p.display())
            }
        }
    }
}

impl std::error::Error for RenderError {}

fn check_path(p: &Path) -> Result<(), RenderError> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = p.as_os_str().as_bytes();
    if bytes.contains(&b'\t') || bytes.contains(&b'\n') {
        return Err(RenderError::UnsupportedPath(p.to_path_buf()));
    }
    Ok(())
}

fn hex_digest(hasher: Sha256) -> String {
    let result = hasher.finalize();
    let mut s = String::with_capacity(result.len() * 2);
    for b in result {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

pub fn build(
    name: &str,
    applied: &[Change],
    originals: &[Original],
    actions: &[ActionRecord],
) -> io::Result<UninstallData> {
    let mut items = Vec::new();
    let mut blobs = Vec::new();

    for record in actions {
        if record.decision == Decision::RunForReal {
            let as_root = record.action.as_root;
            let command = actions::shell_quote(&record.action.argv);
            let inverse =
                actions::inverse(&record.action.argv).map(|inv| actions::shell_quote(&inv));
            items.push(Item::Action {
                as_root,
                command,
                inverse,
            });
        }
    }

    for change in applied {
        match change.kind {
            ChangeKind::Created => match change.entry {
                FileKind::File => {
                    let content = match &change.blob {
                        Some(p) => std::fs::read(p)?,
                        None => {
                            if change.path.exists() {
                                std::fs::read(&change.path)?
                            } else {
                                Vec::new()
                            }
                        }
                    };
                    let mut hasher = Sha256::new();
                    hasher.update(&content);
                    let sha256 = hex_digest(hasher);
                    items.push(Item::CreatedFile {
                        path: change.path.clone(),
                        sha256,
                    });
                }
                FileKind::Symlink => {
                    let target = change.link_target.clone().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "missing symlink target")
                    })?;
                    items.push(Item::CreatedSymlink {
                        path: change.path.clone(),
                        target,
                    });
                }
                FileKind::Dir => {
                    items.push(Item::CreatedDir {
                        path: change.path.clone(),
                    });
                }
                FileKind::Other => {}
            },
            ChangeKind::Modified => {
                let orig = originals
                    .iter()
                    .find(|o| o.path == change.path)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("missing original for {}", change.path.display()),
                        )
                    })?;

                let inst_bytes = match &change.blob {
                    Some(p) => std::fs::read(p)?,
                    None => {
                        if change.path.exists() {
                            std::fs::read(&change.path)?
                        } else {
                            Vec::new()
                        }
                    }
                };

                let mut hasher = Sha256::new();
                hasher.update(&inst_bytes);
                let installed_sha256 = hex_digest(hasher);

                let orig_bytes = orig.content.as_deref().unwrap_or(&[]);

                let is_text = !orig_bytes.contains(&0)
                    && !inst_bytes.contains(&0)
                    && std::str::from_utf8(orig_bytes).is_ok()
                    && std::str::from_utf8(&inst_bytes).is_ok();

                if is_text {
                    let orig_str = std::str::from_utf8(orig_bytes).unwrap();
                    let inst_str = std::str::from_utf8(&inst_bytes).unwrap();
                    let diff = similar::TextDiff::from_lines(orig_str, inst_str)
                        .unified_diff()
                        .header("original", "installed")
                        .to_string();
                    let blob_idx = blobs.len();
                    blobs.push(diff.into_bytes());
                    items.push(Item::ModifiedPatch {
                        path: change.path.clone(),
                        blob: blob_idx,
                        installed_sha256,
                        mode: orig.mode & 0o7777,
                    });
                } else {
                    let blob_idx = blobs.len();
                    blobs.push(orig_bytes.to_vec());
                    items.push(Item::ModifiedRestore {
                        path: change.path.clone(),
                        blob: blob_idx,
                        installed_sha256,
                        mode: orig.mode & 0o7777,
                    });
                }
            }
            ChangeKind::Deleted => {
                let orig = originals
                    .iter()
                    .find(|o| o.path == change.path)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("missing original for {}", change.path.display()),
                        )
                    })?;

                match change.entry {
                    FileKind::File => {
                        let orig_bytes = orig.content.as_deref().unwrap_or(&[]);
                        let blob_idx = blobs.len();
                        blobs.push(orig_bytes.to_vec());
                        items.push(Item::DeletedFile {
                            path: change.path.clone(),
                            blob: blob_idx,
                            mode: orig.mode & 0o7777,
                        });
                    }
                    FileKind::Symlink => {
                        let target = orig.link_target.clone().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "missing symlink target in original",
                            )
                        })?;
                        items.push(Item::DeletedSymlink {
                            path: change.path.clone(),
                            target,
                        });
                    }
                    FileKind::Dir => {
                        items.push(Item::DeletedDir {
                            path: change.path.clone(),
                            mode: orig.mode & 0o7777,
                        });
                    }
                    FileKind::Other => {}
                }
            }
        }
    }

    Ok(UninstallData {
        name: name.to_string(),
        items,
        blobs,
    })
}

pub fn render(data: &UninstallData) -> Result<String, RenderError> {
    for item in &data.items {
        match item {
            Item::CreatedFile { path, .. } => check_path(path)?,
            Item::CreatedSymlink { path, target } => {
                check_path(path)?;
                check_path(target)?;
            }
            Item::CreatedDir { path } => check_path(path)?,
            Item::ModifiedPatch { path, .. } => check_path(path)?,
            Item::ModifiedRestore { path, .. } => check_path(path)?,
            Item::DeletedFile { path, .. } => check_path(path)?,
            Item::DeletedSymlink { path, target } => {
                check_path(path)?;
                check_path(target)?;
            }
            Item::DeletedDir { path, .. } => check_path(path)?,
            Item::Action { .. } => {}
        }
    }

    let mut out = String::new();
    let template = include_str!("template.sh");
    out.push_str(template);
    if !out.ends_with('\n') {
        out.push('\n');
    }

    out.push_str("#@installctl-uninstall\tv1\n");
    out.push_str(&format!("#@name\t{}\n", data.name));

    for item in &data.items {
        match item {
            Item::CreatedFile { path, sha256 } => {
                out.push_str(&format!(
                    "#@item\tcreated_file\t{}\t{}\n",
                    path.display(),
                    sha256
                ));
            }
            Item::CreatedSymlink { path, target } => {
                out.push_str(&format!(
                    "#@item\tcreated_symlink\t{}\t{}\n",
                    path.display(),
                    target.display()
                ));
            }
            Item::CreatedDir { path } => {
                out.push_str(&format!("#@item\tcreated_dir\t{}\n", path.display()));
            }
            Item::ModifiedPatch {
                path,
                blob,
                installed_sha256,
                mode,
            } => {
                out.push_str(&format!(
                    "#@item\tmodified_patch\t{}\t{}\t{}\t{:o}\n",
                    path.display(),
                    blob,
                    installed_sha256,
                    mode
                ));
            }
            Item::ModifiedRestore {
                path,
                blob,
                installed_sha256,
                mode,
            } => {
                out.push_str(&format!(
                    "#@item\tmodified_restore\t{}\t{}\t{}\t{:o}\n",
                    path.display(),
                    blob,
                    installed_sha256,
                    mode
                ));
            }
            Item::DeletedFile { path, blob, mode } => {
                out.push_str(&format!(
                    "#@item\tdeleted_file\t{}\t{}\t{:o}\n",
                    path.display(),
                    blob,
                    mode
                ));
            }
            Item::DeletedSymlink { path, target } => {
                out.push_str(&format!(
                    "#@item\tdeleted_symlink\t{}\t{}\n",
                    path.display(),
                    target.display()
                ));
            }
            Item::DeletedDir { path, mode } => {
                out.push_str(&format!(
                    "#@item\tdeleted_dir\t{}\t{:o}\n",
                    path.display(),
                    mode
                ));
            }
            Item::Action {
                as_root,
                command,
                inverse,
            } => {
                let root_str = if *as_root { "root" } else { "user" };
                let inv_str = inverse.as_deref().unwrap_or("-");
                out.push_str(&format!(
                    "#@item\taction\t{}\t{}\t{}\n",
                    root_str, command, inv_str
                ));
            }
        }
    }

    for (idx, blob) in data.blobs.iter().enumerate() {
        out.push_str(&format!("#@blob\t{}\n", idx));
        let encoded = BASE64_STANDARD.encode(blob);
        for chunk in encoded.as_bytes().chunks(76) {
            let chunk_str = std::str::from_utf8(chunk).unwrap();
            out.push_str(&format!("#@b64\t{}\n", chunk_str));
        }
    }

    Ok(out)
}

pub fn parse(script: &str) -> Result<UninstallData, String> {
    let mut name = String::new();
    let mut items = Vec::new();
    let mut blob_b64: Vec<String> = Vec::new();
    let mut cur_blob_idx: Option<usize> = None;
    let mut seen_header = false;

    for line in script.lines() {
        if !line.starts_with("#@") {
            continue;
        }
        let rest = &line[2..];
        let parts: Vec<&str> = rest.split('\t').collect();
        if parts.is_empty() {
            continue;
        }
        match parts[0] {
            "installctl-uninstall" => {
                if parts.get(1) != Some(&"v1") {
                    return Err(format!("unsupported version: {:?}", parts.get(1)));
                }
                seen_header = true;
            }
            "name" => {
                name = parts.get(1).unwrap_or(&"").to_string();
            }
            "item" => {
                let kind = parts.get(1).ok_or("missing item kind")?;
                match *kind {
                    "created_file" => {
                        let path = PathBuf::from(parts.get(2).ok_or("missing path")?);
                        let sha256 = parts.get(3).ok_or("missing sha256")?.to_string();
                        items.push(Item::CreatedFile { path, sha256 });
                    }
                    "created_symlink" => {
                        let path = PathBuf::from(parts.get(2).ok_or("missing path")?);
                        let target = PathBuf::from(parts.get(3).ok_or("missing target")?);
                        items.push(Item::CreatedSymlink { path, target });
                    }
                    "created_dir" => {
                        let path = PathBuf::from(parts.get(2).ok_or("missing path")?);
                        items.push(Item::CreatedDir { path });
                    }
                    "modified_patch" => {
                        let path = PathBuf::from(parts.get(2).ok_or("missing path")?);
                        let blob = parts
                            .get(3)
                            .ok_or("missing blob")?
                            .parse::<usize>()
                            .map_err(|e| e.to_string())?;
                        let installed_sha256 = parts.get(4).ok_or("missing sha")?.to_string();
                        let mode_str = parts.get(5).ok_or("missing mode")?;
                        let mode = u32::from_str_radix(mode_str, 8).map_err(|e| e.to_string())?;
                        items.push(Item::ModifiedPatch {
                            path,
                            blob,
                            installed_sha256,
                            mode,
                        });
                    }
                    "modified_restore" => {
                        let path = PathBuf::from(parts.get(2).ok_or("missing path")?);
                        let blob = parts
                            .get(3)
                            .ok_or("missing blob")?
                            .parse::<usize>()
                            .map_err(|e| e.to_string())?;
                        let installed_sha256 = parts.get(4).ok_or("missing sha")?.to_string();
                        let mode_str = parts.get(5).ok_or("missing mode")?;
                        let mode = u32::from_str_radix(mode_str, 8).map_err(|e| e.to_string())?;
                        items.push(Item::ModifiedRestore {
                            path,
                            blob,
                            installed_sha256,
                            mode,
                        });
                    }
                    "deleted_file" => {
                        let path = PathBuf::from(parts.get(2).ok_or("missing path")?);
                        let blob = parts
                            .get(3)
                            .ok_or("missing blob")?
                            .parse::<usize>()
                            .map_err(|e| e.to_string())?;
                        let mode_str = parts.get(4).ok_or("missing mode")?;
                        let mode = u32::from_str_radix(mode_str, 8).map_err(|e| e.to_string())?;
                        items.push(Item::DeletedFile { path, blob, mode });
                    }
                    "deleted_symlink" => {
                        let path = PathBuf::from(parts.get(2).ok_or("missing path")?);
                        let target = PathBuf::from(parts.get(3).ok_or("missing target")?);
                        items.push(Item::DeletedSymlink { path, target });
                    }
                    "deleted_dir" => {
                        let path = PathBuf::from(parts.get(2).ok_or("missing path")?);
                        let mode_str = parts.get(3).ok_or("missing mode")?;
                        let mode = u32::from_str_radix(mode_str, 8).map_err(|e| e.to_string())?;
                        items.push(Item::DeletedDir { path, mode });
                    }
                    "action" => {
                        let as_root = match parts.get(2) {
                            Some(&"root") => true,
                            Some(&"user") => false,
                            other => return Err(format!("invalid root/user: {:?}", other)),
                        };
                        let command = parts.get(3).ok_or("missing command")?.to_string();
                        let inv_raw = parts.get(4).ok_or("missing inverse")?;
                        let inverse = if *inv_raw == "-" {
                            None
                        } else {
                            Some(inv_raw.to_string())
                        };
                        items.push(Item::Action {
                            as_root,
                            command,
                            inverse,
                        });
                    }
                    other => return Err(format!("unknown item kind: {}", other)),
                }
            }
            "blob" => {
                let idx = parts
                    .get(1)
                    .ok_or("missing blob idx")?
                    .parse::<usize>()
                    .map_err(|e| e.to_string())?;
                while blob_b64.len() <= idx {
                    blob_b64.push(String::new());
                }
                cur_blob_idx = Some(idx);
            }
            "b64" => {
                let idx = cur_blob_idx.ok_or("b64 line before blob header")?;
                let chunk = parts.get(1).unwrap_or(&"");
                blob_b64[idx].push_str(chunk);
            }
            _ => {}
        }
    }

    if !seen_header {
        return Err("missing #@installctl-uninstall header".to_string());
    }

    let mut blobs = Vec::with_capacity(blob_b64.len());
    for (i, b64_str) in blob_b64.into_iter().enumerate() {
        let bytes = BASE64_STANDARD
            .decode(&b64_str)
            .map_err(|e| format!("failed to decode blob {}: {}", i, e))?;
        blobs.push(bytes);
    }

    Ok(UninstallData { name, items, blobs })
}

fn is_writable(p: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    if let Ok(c) = CString::new(p.as_os_str().as_bytes()) {
        unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
    } else {
        false
    }
}

fn current_timestamp() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}{:02}{:02}{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

pub fn merge(old: UninstallData, new: UninstallData) -> UninstallData {
    let shift = old.blobs.len();
    let mut items = old.items;
    for mut item in new.items {
        match &mut item {
            Item::ModifiedPatch { blob, .. }
            | Item::ModifiedRestore { blob, .. }
            | Item::DeletedFile { blob, .. } => {
                *blob += shift;
            }
            _ => {}
        }
        items.push(item);
    }
    let mut blobs = old.blobs;
    blobs.extend(new.blobs);

    UninstallData {
        name: new.name,
        items,
        blobs,
    }
}

pub fn write_script(
    dir: &Path,
    mut data: UninstallData,
    p: &mut dyn Prompter,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(format!("uninstall-{}.sh", data.name));

    if target.symlink_metadata().is_ok() {
        let parse_result = std::fs::read_to_string(&target)
            .map_err(|e| e.to_string())
            .and_then(|c| parse(&c));
        match parse_result {
            Ok(old) => {
                let choice = p.choose(
                    &format!("An uninstall script for {} already exists.", data.name),
                    &[
                        (
                            'm',
                            "merge: the uninstall script will remove the program completely",
                        ),
                        (
                            'u',
                            "update only: new script undoes just this update; the old one is kept as uninstall-<name>.prev-<YYYYmmddHHMMSS>.sh",
                        ),
                    ],
                );
                if choice == 'm' {
                    data = merge(old, data);
                } else {
                    let ts = current_timestamp();
                    let backup = dir.join(format!("uninstall-{}.prev-{}.sh", data.name, ts));
                    std::fs::rename(&target, &backup)?;
                }
            }
            Err(_) => {
                let ts = current_timestamp();
                let backup = dir.join(format!("uninstall-{}.prev-{}.sh", data.name, ts));
                std::fs::rename(&target, &backup)?;
                p.say(&format!(
                    "Existing uninstall script could not be parsed and was backed up as {}",
                    backup.display()
                ));
            }
        }
    }

    let rendered = render(&data).map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write(&target, rendered)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;

    Ok(target)
}

pub fn ask_location(p: &mut dyn Prompter, home: &Path) -> PathBuf {
    loop {
        let ans = p.input("Where should uninstall-<name>.sh go?", Some("~/.local/bin"));
        let path = if ans == "~" {
            home.to_path_buf()
        } else if let Some(rest) = ans.strip_prefix("~/") {
            home.join(rest)
        } else {
            PathBuf::from(ans)
        };

        if let Err(e) = std::fs::create_dir_all(&path) {
            p.say(&format!(
                "Cannot create directory {}: {}",
                path.display(),
                e
            ));
            continue;
        }

        if !path.is_dir() {
            p.say(&format!("Not a directory: {}", path.display()));
            continue;
        }

        if !is_writable(&path) {
            p.say(&format!("Directory is not writable: {}", path.display()));
            continue;
        }

        return path;
    }
}
