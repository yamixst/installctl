use std::path::{Path, PathBuf};

use installctl::apply::Original;
use installctl::changes::{Change, ChangeKind};
use installctl::overlay::FileKind;
use installctl::prompt::ScriptedPrompter;
use installctl::uninstall::{
    Item, RenderError, UninstallData, ask_location, build, merge, parse, render, write_script,
};

fn run_script_in(
    script: &Path,
    stdin: &str,
    home: &Path,
    extra_path: Option<&Path>,
) -> std::process::Output {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg(script).env("HOME", home);
    if let Some(p) = extra_path {
        let current_path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{}", p.display(), current_path));
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn sh");
    use std::io::Write;
    if let Some(mut stdin_pipe) = child.stdin.take() {
        let _ = stdin_pipe.write_all(stdin.as_bytes());
    }
    child.wait_with_output().expect("failed to wait on sh")
}

fn run_script(script: &Path, stdin: &str) -> std::process::Output {
    let home = script.parent().unwrap();
    run_script_in(script, stdin, home, None)
}

fn run_script_with_path(script: &Path, stdin: &str, extra_path: &Path) -> std::process::Output {
    let home = script.parent().unwrap();
    run_script_in(script, stdin, home, Some(extra_path))
}

#[test]
fn render_parse_roundtrip() {
    let mut large_blob = vec![0u8; 10 * 1024];
    for (i, byte) in large_blob.iter_mut().enumerate() {
        *byte = (i % 256) as u8;
    }
    let data = UninstallData {
        name: "herdr".to_string(),
        items: vec![
            Item::CreatedFile {
                path: PathBuf::from("/usr/local/bin/foo"),
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            },
            Item::CreatedSymlink {
                path: PathBuf::from("/usr/local/bin/bar"),
                target: PathBuf::from("foo"),
            },
            Item::CreatedDir {
                path: PathBuf::from("/etc/foo"),
            },
            Item::ModifiedPatch {
                path: PathBuf::from("/home/user/.bashrc"),
                blob: 0,
                installed_sha256: "abc1234".to_string(),
                mode: 0o644,
            },
            Item::ModifiedRestore {
                path: PathBuf::from("/etc/foo.conf"),
                blob: 1,
                installed_sha256: "def5678".to_string(),
                mode: 0o600,
            },
            Item::DeletedFile {
                path: PathBuf::from("/etc/old.conf"),
                blob: 2,
                mode: 0o644,
            },
            Item::DeletedSymlink {
                path: PathBuf::from("/usr/bin/old_link"),
                target: PathBuf::from("/usr/bin/target"),
            },
            Item::DeletedDir {
                path: PathBuf::from("/var/log/old_dir"),
                mode: 0o755,
            },
            Item::Action {
                as_root: true,
                command: "apt-get install -y pkg".to_string(),
                inverse: Some("apt-get remove -y pkg".to_string()),
            },
            Item::Action {
                as_root: false,
                command: "crontab -r".to_string(),
                inverse: None,
            },
        ],
        blobs: vec![
            b"diff --git a/file b/file\n".to_vec(),
            large_blob,
            b"deleted file content".to_vec(),
        ],
    };

    let rendered = render(&data).expect("rendering should succeed");
    let parsed = parse(&rendered).expect("parsing should succeed");
    assert_eq!(parsed, data);

    let tmp_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp_file.path(), &rendered).unwrap();
    let status = std::process::Command::new("sh")
        .arg("-n")
        .arg(tmp_file.path())
        .status()
        .expect("failed to execute sh -n");
    assert!(status.success(), "sh -n syntax check failed");
}

#[test]
fn uninstall_all() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let created_file = home.join("bin/tool");
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(&created_file, b"tool-binary").unwrap();

    let created_dir = home.join("share/tool");
    std::fs::create_dir_all(&created_dir).unwrap();

    let created_symlink = home.join("bin/tool-link");
    std::os::unix::fs::symlink("tool", &created_symlink).unwrap();

    let bashrc = home.join(".bashrc");
    let bashrc_orig = b"export PATH=/bin\n";
    let bashrc_installed = b"export PATH=/bin\nexport TOOL=1\n";
    std::fs::write(&bashrc, bashrc_installed).unwrap();

    let deleted_file = home.join("old.txt");
    let deleted_symlink = home.join("old_link");
    let deleted_dir = home.join("old_dir");

    let applied = vec![
        Change {
            path: created_file.clone(),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 11,
            mode: 0o755,
            owner: (1000, 1000),
            blob: Some(created_file.clone()),
            link_target: None,
            origin: None,
        },
        Change {
            path: created_dir.clone(),
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
            path: created_symlink.clone(),
            kind: ChangeKind::Created,
            entry: FileKind::Symlink,
            size: 4,
            mode: 0o777,
            owner: (1000, 1000),
            blob: None,
            link_target: Some(PathBuf::from("tool")),
            origin: None,
        },
        Change {
            path: bashrc.clone(),
            kind: ChangeKind::Modified,
            entry: FileKind::File,
            size: bashrc_installed.len() as u64,
            mode: 0o644,
            owner: (1000, 1000),
            blob: Some(bashrc.clone()),
            link_target: None,
            origin: None,
        },
        Change {
            path: deleted_file.clone(),
            kind: ChangeKind::Deleted,
            entry: FileKind::File,
            size: 11,
            mode: 0o640,
            owner: (1000, 1000),
            blob: None,
            link_target: None,
            origin: None,
        },
        Change {
            path: deleted_symlink.clone(),
            kind: ChangeKind::Deleted,
            entry: FileKind::Symlink,
            size: 4,
            mode: 0o777,
            owner: (1000, 1000),
            blob: None,
            link_target: None,
            origin: None,
        },
        Change {
            path: deleted_dir.clone(),
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

    let originals = vec![
        Original {
            path: bashrc.clone(),
            kind: FileKind::File,
            mode: 0o644,
            content: Some(bashrc_orig.to_vec()),
            link_target: None,
        },
        Original {
            path: deleted_file.clone(),
            kind: FileKind::File,
            mode: 0o640,
            content: Some(b"old content".to_vec()),
            link_target: None,
        },
        Original {
            path: deleted_symlink.clone(),
            kind: FileKind::Symlink,
            mode: 0o777,
            content: None,
            link_target: Some(PathBuf::from("tool")),
        },
        Original {
            path: deleted_dir.clone(),
            kind: FileKind::Dir,
            mode: 0o755,
            content: None,
            link_target: None,
        },
    ];

    let data = build("mytool", &applied, &originals, &[]).unwrap();
    let script_content = render(&data).unwrap();
    let script_path = home.join("uninstall-mytool.sh");
    std::fs::write(&script_path, &script_content).unwrap();

    let output = run_script(&script_path, "a\n");
    assert!(
        output.status.success(),
        "script failed: stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!created_file.exists());
    assert!(!created_dir.exists());
    assert!(!created_symlink.exists());
    assert_eq!(std::fs::read(&bashrc).unwrap(), bashrc_orig);
    assert_eq!(std::fs::read(&deleted_file).unwrap(), b"old content");
    assert!(deleted_symlink.is_symlink());
    assert_eq!(
        std::fs::read_link(&deleted_symlink).unwrap(),
        PathBuf::from("tool")
    );
    assert!(deleted_dir.is_dir());
    let dir_mode = std::fs::metadata(&deleted_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o755);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&deleted_file)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o640);
    assert!(!script_path.exists());
}

#[test]
fn revert_keeps_later_user_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let bashrc = home.join(".bashrc");
    let bashrc_orig = b"# my bashrc\nexport PATH=/bin\n";
    let bashrc_installed = b"# my bashrc\nexport PATH=/bin\nexport TOOL=1\n";
    std::fs::write(&bashrc, bashrc_installed).unwrap();

    let applied = vec![Change {
        path: bashrc.clone(),
        kind: ChangeKind::Modified,
        entry: FileKind::File,
        size: bashrc_installed.len() as u64,
        mode: 0o644,
        owner: (1000, 1000),
        blob: Some(bashrc.clone()),
        link_target: None,
        origin: None,
    }];

    let originals = vec![Original {
        path: bashrc.clone(),
        kind: FileKind::File,
        mode: 0o644,
        content: Some(bashrc_orig.to_vec()),
        link_target: None,
    }];

    let data = build("mytool", &applied, &originals, &[]).unwrap();
    let script_content = render(&data).unwrap();
    let script_path = home.join("uninstall-mytool.sh");
    std::fs::write(&script_path, &script_content).unwrap();

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&bashrc)
        .unwrap();
    use std::io::Write;
    writeln!(f, "alias x=y").unwrap();
    drop(f);

    let output = run_script(&script_path, "a\n");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let expected = b"# my bashrc\nexport PATH=/bin\nalias x=y\n";
    assert_eq!(std::fs::read(&bashrc).unwrap(), expected);
}

#[test]
fn revert_conflict_fails_and_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let bashrc = home.join(".bashrc");
    let bashrc_orig = b"# header\nold_line_1\nold_line_2\n";
    let bashrc_installed = b"# header\ninstalled_line_1\ninstalled_line_2\n";
    std::fs::write(&bashrc, bashrc_installed).unwrap();

    let applied = vec![Change {
        path: bashrc.clone(),
        kind: ChangeKind::Modified,
        entry: FileKind::File,
        size: bashrc_installed.len() as u64,
        mode: 0o644,
        owner: (1000, 1000),
        blob: Some(bashrc.clone()),
        link_target: None,
        origin: None,
    }];

    let originals = vec![Original {
        path: bashrc.clone(),
        kind: FileKind::File,
        mode: 0o644,
        content: Some(bashrc_orig.to_vec()),
        link_target: None,
    }];

    let data = build("mytool", &applied, &originals, &[]).unwrap();
    let script_content = render(&data).unwrap();
    let script_path = home.join("uninstall-mytool.sh");
    std::fs::write(&script_path, &script_content).unwrap();

    std::fs::write(&bashrc, b"totally different content\n").unwrap();

    let output = run_script(&script_path, "a\nn\n");
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("conflict with the installer changes; cannot revert automatically"));
    assert!(stdout.contains("conflicts with your edits"));
    assert!(script_path.exists());
}

#[test]
fn changed_file_is_called_out() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let created_file = home.join("tool.conf");
    std::fs::write(&created_file, b"original-conf").unwrap();

    let applied = vec![Change {
        path: created_file.clone(),
        kind: ChangeKind::Created,
        entry: FileKind::File,
        size: 13,
        mode: 0o644,
        owner: (1000, 1000),
        blob: Some(created_file.clone()),
        link_target: None,
        origin: None,
    }];

    let data = build("mytool", &applied, &[], &[]).unwrap();
    let script_content = render(&data).unwrap();
    let script_path = home.join("uninstall-mytool.sh");
    std::fs::write(&script_path, &script_content).unwrap();

    std::fs::write(&created_file, b"user-modified-conf").unwrap();

    let output = run_script(&script_path, "c\n");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("CHANGED since install"),
        "expected CHANGED since install in output, got:\n{}",
        stdout
    );
}

#[test]
fn cancel_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let created_file = home.join("tool.bin");
    std::fs::write(&created_file, b"content").unwrap();

    let applied = vec![Change {
        path: created_file.clone(),
        kind: ChangeKind::Created,
        entry: FileKind::File,
        size: 7,
        mode: 0o755,
        owner: (1000, 1000),
        blob: Some(created_file.clone()),
        link_target: None,
        origin: None,
    }];

    let data = build("mytool", &applied, &[], &[]).unwrap();
    let script_content = render(&data).unwrap();
    let script_path = home.join("uninstall-mytool.sh");
    std::fs::write(&script_path, &script_content).unwrap();

    let output = run_script(&script_path, "c\n");
    assert_eq!(output.status.code(), Some(1));
    assert!(created_file.exists());
    assert!(script_path.exists());
}

#[test]
fn select_some_keeps_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let file_a = home.join("file_a.txt");
    let file_b = home.join("file_b.txt");
    std::fs::write(&file_a, b"a").unwrap();
    std::fs::write(&file_b, b"b").unwrap();

    let applied = vec![
        Change {
            path: file_a.clone(),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 1,
            mode: 0o644,
            owner: (1000, 1000),
            blob: Some(file_a.clone()),
            link_target: None,
            origin: None,
        },
        Change {
            path: file_b.clone(),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 1,
            mode: 0o644,
            owner: (1000, 1000),
            blob: Some(file_b.clone()),
            link_target: None,
            origin: None,
        },
    ];

    let data = build("mytool", &applied, &[], &[]).unwrap();
    let script_content = render(&data).unwrap();
    let script_path = home.join("uninstall-mytool.sh");
    std::fs::write(&script_path, &script_content).unwrap();

    let output = run_script(&script_path, "s\ny\nn\nn\n");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!file_b.exists());
    assert!(file_a.exists());
    assert!(script_path.exists());
}

#[test]
fn awkward_paths_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let subdir = home.join("My Apps");
    std::fs::create_dir_all(&subdir).unwrap();
    let file_path = subdir.join("naïve 'q' $x.txt");
    std::fs::write(&file_path, b"special content").unwrap();

    let applied = vec![Change {
        path: file_path.clone(),
        kind: ChangeKind::Created,
        entry: FileKind::File,
        size: 15,
        mode: 0o644,
        owner: (1000, 1000),
        blob: Some(file_path.clone()),
        link_target: None,
        origin: None,
    }];

    let data = build("mytool", &applied, &[], &[]).unwrap();
    let script_content = render(&data).unwrap();
    let script_path = home.join("uninstall-mytool.sh");
    std::fs::write(&script_path, &script_content).unwrap();

    let output = run_script(&script_path, "a\n");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("naïve 'q' $x.txt"),
        "expected verbatim path in output, got:\n{}",
        stdout
    );
    assert!(!file_path.exists());
}

#[test]
fn tab_in_path_is_refused() {
    let bad_path = PathBuf::from("/tmp/bad\tpath.txt");
    let data = UninstallData {
        name: "mytool".to_string(),
        items: vec![Item::CreatedFile {
            path: bad_path.clone(),
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
        }],
        blobs: vec![],
    };

    let err = render(&data).unwrap_err();
    assert_eq!(err, RenderError::UnsupportedPath(bad_path));

    let bad_target = PathBuf::from("/tmp/target\nwithnewline");
    let data_symlink = UninstallData {
        name: "mytool".to_string(),
        items: vec![Item::CreatedSymlink {
            path: PathBuf::from("/tmp/good_link"),
            target: bad_target.clone(),
        }],
        blobs: vec![],
    };
    let err = render(&data_symlink).unwrap_err();
    assert_eq!(err, RenderError::UnsupportedPath(bad_target));
}

#[test]
fn action_items() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake_systemctl = bin_dir.join("systemctl");
    let log_file = home.join("systemctl.log");

    use std::os::unix::fs::PermissionsExt;
    let fake_script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\n",
        log_file.display()
    );
    std::fs::write(&fake_systemctl, fake_script).unwrap();
    std::fs::set_permissions(&fake_systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();

    let data = UninstallData {
        name: "testactions".to_string(),
        items: vec![
            Item::Action {
                as_root: false,
                command: "systemctl daemon-reload".to_string(),
                inverse: None,
            },
            Item::Action {
                as_root: false,
                command: "systemctl --user enable a.service".to_string(),
                inverse: Some("systemctl --user disable a.service".to_string()),
            },
        ],
        blobs: vec![],
    };

    let script_content = render(&data).unwrap();
    let script_path = home.join("uninstall-testactions.sh");
    std::fs::write(&script_path, &script_content).unwrap();

    let output = run_script_with_path(&script_path, "a\n", &bin_dir);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(
        "note: the installer ran \"systemctl daemon-reload\"; undo it by hand if needed"
    ));
    assert!(stdout.contains(
        "run: systemctl --user disable a.service   (undo of: systemctl --user enable a.service)"
    ));

    let log = std::fs::read_to_string(&log_file).expect("log file should exist");
    assert!(log.contains("--user disable a.service"));
    assert!(!log.contains("daemon-reload"));
}

#[test]
fn merge_shifts_blobs_and_concatenates_items() {
    let old = UninstallData {
        name: "old_name".to_string(),
        items: vec![Item::ModifiedPatch {
            path: PathBuf::from("/etc/a"),
            blob: 0,
            installed_sha256: "hash1".to_string(),
            mode: 0o644,
        }],
        blobs: vec![b"old_blob".to_vec()],
    };
    let new = UninstallData {
        name: "new_name".to_string(),
        items: vec![
            Item::ModifiedRestore {
                path: PathBuf::from("/etc/b"),
                blob: 0,
                installed_sha256: "hash2".to_string(),
                mode: 0o600,
            },
            Item::DeletedFile {
                path: PathBuf::from("/etc/c"),
                blob: 1,
                mode: 0o644,
            },
        ],
        blobs: vec![b"new_blob1".to_vec(), b"new_blob2".to_vec()],
    };
    let merged = merge(old, new);
    assert_eq!(merged.name, "new_name");
    assert_eq!(
        merged.blobs,
        vec![
            b"old_blob".to_vec(),
            b"new_blob1".to_vec(),
            b"new_blob2".to_vec()
        ]
    );
    assert_eq!(merged.items.len(), 3);
    match &merged.items[0] {
        Item::ModifiedPatch { blob, .. } => assert_eq!(*blob, 0),
        _ => panic!("unexpected item 0"),
    }
    match &merged.items[1] {
        Item::ModifiedRestore { blob, .. } => assert_eq!(*blob, 1),
        _ => panic!("unexpected item 1"),
    }
    match &merged.items[2] {
        Item::DeletedFile { blob, .. } => assert_eq!(*blob, 2),
        _ => panic!("unexpected item 2"),
    }
}

#[test]
fn merge_then_uninstall_removes_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let app_bin = bin_dir.join("app");
    std::fs::write(&app_bin, b"v1\0").unwrap();

    let applied_v1 = vec![Change {
        path: app_bin.clone(),
        kind: ChangeKind::Created,
        entry: FileKind::File,
        size: 3,
        mode: 0o755,
        owner: (1000, 1000),
        blob: Some(app_bin.clone()),
        link_target: None,
        origin: None,
    }];
    let data_v1 = build("app", &applied_v1, &[], &[]).unwrap();
    let mut p1 = ScriptedPrompter::new(vec![]);
    let script_path = write_script(home, data_v1, &mut p1).unwrap();
    assert_eq!(script_path, home.join("uninstall-app.sh"));
    assert!(script_path.exists());

    // Install v2
    let share_dir = home.join("share/app");
    std::fs::create_dir_all(&share_dir).unwrap();
    let share_new = share_dir.join("new");
    std::fs::write(&share_new, b"new-file-content").unwrap();
    std::fs::write(&app_bin, b"v2\0").unwrap();

    let applied_v2 = vec![
        Change {
            path: app_bin.clone(),
            kind: ChangeKind::Modified,
            entry: FileKind::File,
            size: 3,
            mode: 0o755,
            owner: (1000, 1000),
            blob: Some(app_bin.clone()),
            link_target: None,
            origin: None,
        },
        Change {
            path: share_dir.clone(),
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
            path: share_new.clone(),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 16,
            mode: 0o644,
            owner: (1000, 1000),
            blob: Some(share_new.clone()),
            link_target: None,
            origin: None,
        },
    ];

    let originals_v2 = vec![Original {
        path: app_bin.clone(),
        kind: FileKind::File,
        mode: 0o755,
        content: Some(b"v1\0".to_vec()),
        link_target: None,
    }];

    let data_v2 = build("app", &applied_v2, &originals_v2, &[]).unwrap();
    assert!(matches!(data_v2.items[0], Item::ModifiedRestore { .. }));

    let mut p2 = ScriptedPrompter::new(vec!["m".to_string()]);
    let script_path2 = write_script(home, data_v2, &mut p2).unwrap();
    assert_eq!(script_path2, script_path);

    // Run merged script with 'a'
    let output = run_script(&script_path, "a\n");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!app_bin.exists(), "app binary should be removed");
    assert!(!share_new.exists(), "share/app/new should be removed");
    assert!(!share_dir.exists(), "share/app directory should be removed");
    assert!(!script_path.exists(), "uninstall script should be removed");
}

#[test]
fn update_only_keeps_previous() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let app_bin = bin_dir.join("app");
    std::fs::write(&app_bin, b"v1\0").unwrap();

    let applied_v1 = vec![Change {
        path: app_bin.clone(),
        kind: ChangeKind::Created,
        entry: FileKind::File,
        size: 3,
        mode: 0o755,
        owner: (1000, 1000),
        blob: Some(app_bin.clone()),
        link_target: None,
        origin: None,
    }];
    let data_v1 = build("app", &applied_v1, &[], &[]).unwrap();
    let mut p1 = ScriptedPrompter::new(vec![]);
    let script_path = write_script(home, data_v1, &mut p1).unwrap();
    assert!(script_path.exists());

    // Install v2
    let share_dir = home.join("share/app");
    std::fs::create_dir_all(&share_dir).unwrap();
    let share_new = share_dir.join("new");
    std::fs::write(&share_new, b"new-file-content").unwrap();
    std::fs::write(&app_bin, b"v2\0").unwrap();

    let applied_v2 = vec![
        Change {
            path: app_bin.clone(),
            kind: ChangeKind::Modified,
            entry: FileKind::File,
            size: 3,
            mode: 0o755,
            owner: (1000, 1000),
            blob: Some(app_bin.clone()),
            link_target: None,
            origin: None,
        },
        Change {
            path: share_dir.clone(),
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
            path: share_new.clone(),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 16,
            mode: 0o644,
            owner: (1000, 1000),
            blob: Some(share_new.clone()),
            link_target: None,
            origin: None,
        },
    ];

    let originals_v2 = vec![Original {
        path: app_bin.clone(),
        kind: FileKind::File,
        mode: 0o755,
        content: Some(b"v1\0".to_vec()),
        link_target: None,
    }];

    let data_v2 = build("app", &applied_v2, &originals_v2, &[]).unwrap();

    let mut p2 = ScriptedPrompter::new(vec!["u".to_string()]);
    let script_path2 = write_script(home, data_v2, &mut p2).unwrap();
    assert_eq!(script_path2, script_path);

    // Two scripts should exist: uninstall-app.sh and uninstall-app.prev-<timestamp>.sh
    let entries: Vec<PathBuf> = std::fs::read_dir(home)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("uninstall-app") && n.ends_with(".sh"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(entries.len(), 2, "expected 2 scripts, got: {:?}", entries);
    let prev_script = entries.into_iter().find(|p| p != &script_path).unwrap();
    assert!(
        prev_script
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(".prev-"),
        "expected backup script name to contain .prev-, got: {:?}",
        prev_script
    );

    // Running the new one restores bin/app to "v1"
    let output = run_script(&script_path, "a\n");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(std::fs::read(&app_bin).unwrap(), b"v1\0");
    assert!(!share_new.exists());
    assert!(!share_dir.exists());
    assert!(!script_path.exists());
    assert!(prev_script.exists());
}

#[test]
fn unparseable_existing_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let script_path = home.join("uninstall-garbage.sh");
    let garbage_content = "this is not an installctl uninstall script\nrandom garbage\n";
    std::fs::write(&script_path, garbage_content).unwrap();

    let data = UninstallData {
        name: "garbage".to_string(),
        items: vec![Item::CreatedDir {
            path: home.join("some_dir"),
        }],
        blobs: vec![],
    };

    let mut p = ScriptedPrompter::new(vec![]);
    let new_script_path = write_script(home, data.clone(), &mut p).unwrap();
    assert_eq!(new_script_path, script_path);

    // Verify new script is valid
    let content = std::fs::read_to_string(&script_path).unwrap();
    let parsed = parse(&content).expect("new script should be parseable");
    assert_eq!(parsed, data);

    // Find the backup script
    let entries: Vec<PathBuf> = std::fs::read_dir(home)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("uninstall-garbage.prev-") && n.ends_with(".sh"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected 1 backup script, got: {:?}",
        entries
    );
    let backup_content = std::fs::read_to_string(&entries[0]).unwrap();
    assert_eq!(backup_content, garbage_content);
}

#[test]
fn ask_location_behavior() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    // 1. Default input ("") -> ~/.local/bin
    let mut p1 = ScriptedPrompter::new(vec!["".to_string()]);
    let loc1 = ask_location(&mut p1, home);
    assert_eq!(loc1, home.join(".local/bin"));
    assert!(loc1.is_dir());

    // 2. Tilde expansion ("~") -> home
    let mut p2 = ScriptedPrompter::new(vec!["~".to_string()]);
    let loc2 = ask_location(&mut p2, home);
    assert_eq!(loc2, home);

    // 3. Subdir with tilde ("~/custom/bin")
    let mut p3 = ScriptedPrompter::new(vec!["~/custom/bin".to_string()]);
    let loc3 = ask_location(&mut p3, home);
    assert_eq!(loc3, home.join("custom/bin"));
    assert!(loc3.is_dir());

    // 4. Invalid path (existing regular file, cannot mkdir under it)
    let regular_file = home.join("not_a_dir");
    std::fs::write(&regular_file, b"hello").unwrap();
    let invalid_path = regular_file.join("sub").to_str().unwrap().to_string();

    let mut p4 = ScriptedPrompter::new(vec![invalid_path, "~/retry_dir".to_string()]);
    let loc4 = ask_location(&mut p4, home);
    assert_eq!(loc4, home.join("retry_dir"));
    assert!(loc4.is_dir());
}
