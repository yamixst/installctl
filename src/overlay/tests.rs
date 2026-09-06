use super::*;
use std::io::{Read, Seek, SeekFrom, Write};
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
fn stat_falls_through_to_lower() {
    let env = TestEnv::new();
    let lower_a = env.lower_dir.path().join("a");
    std::fs::create_dir_all(&lower_a).unwrap();
    std::fs::write(lower_a.join("b.txt"), b"hi").unwrap();

    let attr = env.overlay.stat(Path::new("/a/b.txt")).unwrap();
    assert_eq!(attr.size, 2);
    assert_eq!(attr.kind, FileKind::File);
}

#[test]
fn create_is_visible_and_lower_untouched() {
    let mut env = TestEnv::new();
    let lower_a = env.lower_dir.path().join("a");
    std::fs::create_dir_all(&lower_a).unwrap();
    std::fs::write(lower_a.join("b.txt"), b"hi").unwrap();

    let mut file = env
        .overlay
        .create_file(Path::new("/a/new"), 0o644, 1000, 1000)
        .unwrap();
    file.write_all(b"x").unwrap();
    drop(file);

    let attr = env.overlay.stat(Path::new("/a/new")).unwrap();
    assert_eq!(attr.size, 1);
    assert_eq!(attr.kind, FileKind::File);

    let entries = env.overlay.read_dir(Path::new("/a")).unwrap();
    let names: Vec<String> = entries
        .into_iter()
        .map(|(n, _)| n.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["b.txt", "new"]);

    assert!(!lower_a.join("new").exists());
}

#[test]
fn write_copies_up() {
    let mut env = TestEnv::new();
    let lower_a = env.lower_dir.path().join("a");
    std::fs::create_dir_all(&lower_a).unwrap();
    std::fs::write(lower_a.join("b.txt"), b"hi").unwrap();

    let mut f = env
        .overlay
        .open_write(Path::new("/a/b.txt"), false)
        .unwrap();
    f.seek(SeekFrom::End(0)).unwrap();
    f.write_all(b"!").unwrap();
    drop(f);

    let mut r = env.overlay.open_read(Path::new("/a/b.txt")).unwrap();
    let mut contents = String::new();
    r.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "hi!");

    // Lower file is still "hi"
    let lower_contents = std::fs::read_to_string(lower_a.join("b.txt")).unwrap();
    assert_eq!(lower_contents, "hi");

    // Node is File with origin: Some(_)
    let node = env
        .overlay
        .nodes()
        .get(Path::new("/a/b.txt"))
        .expect("node exists");
    match node {
        UpperNode::File { origin, .. } => {
            assert!(origin.is_some(), "expected origin: Some(_)");
        }
        _ => panic!("expected File node"),
    }
}

#[test]
fn unlink_lower_creates_whiteout() {
    let mut env = TestEnv::new();
    let lower_a = env.lower_dir.path().join("a");
    std::fs::create_dir_all(&lower_a).unwrap();
    std::fs::write(lower_a.join("b.txt"), b"hi").unwrap();

    env.overlay.unlink(Path::new("/a/b.txt")).unwrap();

    let err = env.overlay.stat(Path::new("/a/b.txt")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOENT));

    let entries = env.overlay.read_dir(Path::new("/a")).unwrap();
    assert!(!entries.iter().any(|(n, _)| n == "b.txt"));

    let node = env
        .overlay
        .nodes()
        .get(Path::new("/a/b.txt"))
        .expect("node exists");
    match node {
        UpperNode::Whiteout { .. } => {}
        _ => panic!("expected Whiteout node"),
    }

    assert!(lower_a.join("b.txt").exists());
}

#[test]
fn unlink_upper_only_removes_node() {
    let mut env = TestEnv::new();
    let lower_a = env.lower_dir.path().join("a");
    std::fs::create_dir_all(&lower_a).unwrap();

    env.overlay
        .create_file(Path::new("/a/tmp"), 0o644, 1000, 1000)
        .unwrap();
    assert!(env.overlay.nodes().contains_key(Path::new("/a/tmp")));

    env.overlay.unlink(Path::new("/a/tmp")).unwrap();
    assert!(!env.overlay.nodes().contains_key(Path::new("/a/tmp")));
}

#[test]
fn rmdir_nonempty_fails() {
    let mut env = TestEnv::new();
    let lower_a = env.lower_dir.path().join("a");
    std::fs::create_dir_all(&lower_a).unwrap();
    std::fs::write(lower_a.join("b.txt"), b"hi").unwrap();

    let err = env.overlay.rmdir(Path::new("/a")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOTEMPTY));

    env.overlay.unlink(Path::new("/a/b.txt")).unwrap();
    assert!(env.overlay.nodes().contains_key(Path::new("/a/b.txt")));

    env.overlay.rmdir(Path::new("/a")).unwrap();

    let err = env.overlay.stat(Path::new("/a")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOENT));

    let node = env
        .overlay
        .nodes()
        .get(Path::new("/a"))
        .expect("node exists for /a");
    match node {
        UpperNode::Whiteout { .. } => {}
        _ => panic!("expected Whiteout node for /a"),
    }

    // child whiteout node removed
    assert!(!env.overlay.nodes().contains_key(Path::new("/a/b.txt")));
}

#[test]
fn mkdir_over_whiteout_is_opaque() {
    let mut env = TestEnv::new();
    let lower_a = env.lower_dir.path().join("a");
    std::fs::create_dir_all(&lower_a).unwrap();
    std::fs::write(lower_a.join("b.txt"), b"hi").unwrap();

    env.overlay.unlink(Path::new("/a/b.txt")).unwrap();
    env.overlay.rmdir(Path::new("/a")).unwrap();

    env.overlay
        .mkdir(Path::new("/a"), 0o755, 1000, 1000)
        .unwrap();

    let node = env
        .overlay
        .nodes()
        .get(Path::new("/a"))
        .expect("node exists for /a");
    match node {
        UpperNode::Dir { opaque, .. } => {
            assert!(opaque, "expected opaque: true");
        }
        _ => panic!("expected Dir node for /a"),
    }

    let entries = env.overlay.read_dir(Path::new("/a")).unwrap();
    assert!(
        entries.is_empty(),
        "expected read_dir to be empty, got: {:?}",
        entries
    );
}

#[test]
fn whiteout_hides_lower_descendants() {
    let mut env = TestEnv::new();
    let lower_ab = env.lower_dir.path().join("a").join("b");
    std::fs::create_dir_all(&lower_ab).unwrap();
    std::fs::write(lower_ab.join("c.txt"), b"nested").unwrap();

    env.overlay.unlink(Path::new("/a/b/c.txt")).unwrap();
    env.overlay.rmdir(Path::new("/a/b")).unwrap();

    // Now /a/b is whiteout, stat on /a/b/c.txt should be ENOENT
    let err = env.overlay.stat(Path::new("/a/b/c.txt")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
}

#[test]
fn create_existing_is_eexist() {
    let mut env = TestEnv::new();
    let lower_a = env.lower_dir.path().join("a");
    std::fs::create_dir_all(&lower_a).unwrap();
    std::fs::write(lower_a.join("b.txt"), b"hi").unwrap();

    // Creating existing lower file fails with EEXIST
    let err = env
        .overlay
        .create_file(Path::new("/a/b.txt"), 0o644, 1000, 1000)
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EEXIST));

    // Creating new file succeeds
    env.overlay
        .create_file(Path::new("/a/new"), 0o644, 1000, 1000)
        .unwrap();

    // Creating existing upper file fails with EEXIST
    let err = env
        .overlay
        .create_file(Path::new("/a/new"), 0o644, 1000, 1000)
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
}

#[test]
fn mkdir_missing_parent_is_enoent() {
    let mut env = TestEnv::new();
    let err = env
        .overlay
        .mkdir(Path::new("/missing/dir"), 0o755, 1000, 1000)
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
}

#[test]
fn unlink_dir_is_eisdir() {
    let mut env = TestEnv::new();
    let lower_a = env.lower_dir.path().join("a");
    std::fs::create_dir_all(&lower_a).unwrap();

    let err = env.overlay.unlink(Path::new("/a")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EISDIR));

    env.overlay
        .mkdir(Path::new("/upperdir"), 0o755, 1000, 1000)
        .unwrap();
    let err = env.overlay.unlink(Path::new("/upperdir")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EISDIR));
}

#[test]
fn read_link_lower() {
    let env = TestEnv::new();
    let lower = env.lower_dir.path();
    std::os::unix::fs::symlink("target.txt", lower.join("sym.txt")).unwrap();

    let target = env.overlay.read_link(Path::new("/sym.txt")).unwrap();
    assert_eq!(target, PathBuf::from("target.txt"));

    // read_link on regular file returns EINVAL
    std::fs::write(lower.join("regular.txt"), b"test").unwrap();
    let err = env
        .overlay
        .read_link(Path::new("/regular.txt"))
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
}

#[test]
fn synthetic_dir_creation_and_stat() {
    let mut env = TestEnv::new();
    env.overlay
        .add_synthetic_dir(Path::new("/run/installctl"))
        .unwrap();

    let attr = env.overlay.stat(Path::new("/run/installctl")).unwrap();
    assert_eq!(attr.kind, FileKind::Dir);

    let node = env
        .overlay
        .nodes()
        .get(Path::new("/run/installctl"))
        .expect("node exists");
    match node {
        UpperNode::Dir { synthetic, .. } => {
            assert!(synthetic);
        }
        _ => panic!("expected Dir node"),
    }
}
