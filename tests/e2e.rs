mod common;

use common::{priv_env, sudo_env};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn installctl_bin() -> PathBuf {
    if let Ok(cargo_bin) = std::env::var("CARGO_BIN_EXE_installctl") {
        PathBuf::from(cargo_bin)
    } else {
        let mut path = std::env::current_exe().unwrap();
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.push("installctl");
        path
    }
}

fn create_test_home() -> (tempfile::TempDir, PathBuf) {
    let user = installctl::prereq::current_user().expect("current user");
    let temp = tempfile::Builder::new()
        .prefix("e2e-")
        .tempdir_in(&user.home)
        .expect("create test home");
    let home = temp.path().to_path_buf();
    std::fs::write(home.join(".bashrc"), "# original\n").expect("write .bashrc");
    (temp, home)
}

fn create_answers_file(home: &Path, answers: &[&str]) -> PathBuf {
    let path = home.join("answers.txt");
    let mut content = String::new();
    for a in answers {
        content.push_str(a);
        content.push('\n');
    }
    std::fs::write(&path, content).expect("write answers");
    path
}

#[test]
fn e2e_full_install_and_uninstall() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !priv_env() || !sudo_env() {
        return;
    }

    let _ = std::process::Command::new("sudo")
        .args(["rm", "-f", "/usr/local/bin/fakeapp-helper"])
        .status();
    let _ = std::fs::write("/var/log/fake-real-actions.log", "");

    let (_temp_home, home) = create_test_home();
    let answers_path = create_answers_file(&home, &["s", "r", "a", "y", ""]);

    let bin = installctl_bin();
    let output = std::process::Command::new(&bin)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", &home)
        .env("INSTALLCTL_TEST_ANSWERS", &answers_path)
        .args(["--name", "fakeapp", "sh tests/fixtures/fake-install.sh"])
        .output()
        .expect("run installctl");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "installctl failed.\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
    );

    // fakeapp symlink + target + gc.txt exist
    let target = home.join(".local/share/fakeapp/fakeapp");
    assert!(target.is_file(), "fakeapp target binary must exist");
    let symlink = home.join(".local/bin/fakeapp");
    assert!(
        symlink
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "fakeapp symlink must exist"
    );
    assert_eq!(
        std::fs::read_link(&symlink).unwrap(),
        target,
        "symlink points to target"
    );
    let gc = home.join(".local/share/fakeapp/gc.txt");
    assert!(gc.is_file(), "gc.txt must exist");

    // .fakeapp-tmp absent and not in stdout
    assert!(
        !home.join(".fakeapp-tmp").exists(),
        ".fakeapp-tmp must not exist"
    );
    assert!(
        !stdout.contains(".fakeapp-tmp"),
        "stdout must not contain .fakeapp-tmp"
    );

    // /usr/local/bin/fakeapp-helper owned 0:0 mode 0o755
    let helper = Path::new("/usr/local/bin/fakeapp-helper");
    assert!(helper.exists(), "fakeapp-helper must exist on host");
    use std::os::unix::fs::MetadataExt;
    let meta = helper.symlink_metadata().unwrap();
    assert_eq!(meta.uid(), 0, "fakeapp-helper must be owned by uid 0");
    assert_eq!(meta.gid(), 0, "fakeapp-helper must be owned by gid 0");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o755,
        "fakeapp-helper mode must be 0755"
    );

    // stdout contains:
    assert!(
        stdout.contains("! skipped: sudo apt-get install -y fakepkg"),
        "stdout must mention skipped apt-get: {stdout}"
    );
    assert!(
        stdout.contains("! ran (exit 0): systemctl --user enable --now fakeapp.service"),
        "stdout must mention ran systemctl: {stdout}"
    );
    assert!(
        stdout.contains("sleep 300"),
        "stdout must report leftover sleep 300: {stdout}"
    );

    // ~/.local/bin/uninstall-fakeapp.sh exists
    let uninstall_script = home.join(".local/bin/uninstall-fakeapp.sh");
    assert!(
        uninstall_script.exists(),
        "uninstall-fakeapp.sh must exist at {}",
        uninstall_script.display()
    );

    // staging dir under ~/.local/state/installctl/runs/ empty
    let runs_dir = home.join(".local/state/installctl/runs");
    if runs_dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "staging runs dir must be empty, but found: {entries:?}"
        );
    }

    // Now run the uninstall script with stdin "a\n"
    let mut uninst_cmd = std::process::Command::new("sh");
    uninst_cmd
        .arg(&uninstall_script)
        .env("HOME", &home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = uninst_cmd.spawn().expect("spawn uninstall script");
    use std::io::Write;
    if let Some(mut stdin_pipe) = child.stdin.take() {
        let _ = stdin_pipe.write_all(b"a\n");
    }
    let uninst_output = child.wait_with_output().expect("wait on uninstall script");
    let u_out = String::from_utf8_lossy(&uninst_output.stdout);
    let u_err = String::from_utf8_lossy(&uninst_output.stderr);
    assert!(
        uninst_output.status.success(),
        "uninstall script failed.\nSTDOUT:\n{u_out}\nSTDERR:\n{u_err}"
    );

    // all files gone, helper gone, .bashrc == "# original\n", fake log has disable --now fakeapp.service, script removed
    assert!(
        !home.join(".local/share/fakeapp").exists(),
        "fakeapp share dir must be removed"
    );
    assert!(
        !home.join(".local/bin/fakeapp").exists(),
        "fakeapp bin symlink must be removed"
    );
    assert!(
        !helper.exists(),
        "fakeapp-helper must be removed by uninstall script"
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".bashrc")).unwrap(),
        "# original\n",
        ".bashrc must be restored to original"
    );

    let log = std::fs::read_to_string("/var/log/fake-real-actions.log").unwrap_or_default();
    assert!(
        log.contains("disable --now fakeapp.service"),
        "fake log must contain disable --now fakeapp.service, got: {log}"
    );

    assert!(
        !uninstall_script.exists(),
        "uninstall-fakeapp.sh must be removed after complete uninstall"
    );
}

#[test]
fn e2e_reject_leaves_host_unchanged() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !priv_env() || !sudo_env() {
        return;
    }

    let _ = std::process::Command::new("sudo")
        .args(["rm", "-f", "/usr/local/bin/fakeapp-helper"])
        .status();

    let (_temp_home, home) = create_test_home();
    let answers_path = create_answers_file(&home, &["s", "s", "r"]);

    let bin = installctl_bin();
    let output = std::process::Command::new(&bin)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", &home)
        .env("INSTALLCTL_TEST_ANSWERS", &answers_path)
        .args(["--name", "fakeapp", "sh tests/fixtures/fake-install.sh"])
        .output()
        .expect("run installctl");

    assert_eq!(output.status.code(), Some(1), "expected exit 1 on reject");

    assert!(
        !home.join(".local/share/fakeapp").exists(),
        "fakeapp share dir must not exist"
    );
    assert!(
        !home.join(".local/bin/fakeapp").exists(),
        "fakeapp bin symlink must not exist"
    );
    assert!(
        !Path::new("/usr/local/bin/fakeapp-helper").exists(),
        "fakeapp-helper must not exist on host"
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".bashrc")).unwrap(),
        "# original\n",
        ".bashrc must be unchanged"
    );

    let runs_dir = home.join(".local/state/installctl/runs");
    if runs_dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "staging runs dir must be empty");
    }
}

#[test]
fn e2e_sigint_then_reject_leaves_host_unchanged() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !priv_env() || !sudo_env() {
        return;
    }

    let (_temp_home, home) = create_test_home();
    let answers_path = create_answers_file(&home, &["r"]);

    let bin = installctl_bin();
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&bin);
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", &home)
        .env("INSTALLCTL_TEST_ANSWERS", &answers_path)
        .args([
            "sh",
            "-c",
            "echo a > \"$HOME/a\"; touch \"$HOME/started\"; sleep 30; echo b > \"$HOME/b\"",
        ])
        .process_group(0);

    let child = cmd.spawn().expect("spawn installctl");

    // Poll until a blob appears under staging upper/blobs/ dir (max 10 s)
    let start = std::time::Instant::now();
    let runs_dir = home.join(".local/state/installctl/runs");
    let mut blob_found = false;
    while start.elapsed() < std::time::Duration::from_secs(10) {
        if runs_dir.is_dir()
            && let Ok(entries) = std::fs::read_dir(&runs_dir)
        {
            for entry in entries.flatten() {
                let blobs_dir = entry.path().join("upper/blobs");
                if blobs_dir.is_dir()
                    && let Ok(blobs) = std::fs::read_dir(&blobs_dir)
                    && blobs.flatten().next().is_some()
                {
                    blob_found = true;
                    break;
                }
            }
        }
        if blob_found {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(blob_found, "timed out waiting for staged blob to appear");

    // Send SIGINT to the process group
    let pgid = nix::unistd::Pid::from_raw(child.id() as i32);
    let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGINT);

    let output = child.wait_with_output().expect("wait on installctl");
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 on reject after SIGINT"
    );

    // host has neither a nor b
    assert!(!home.join("a").exists(), "file a must not exist on host");
    assert!(!home.join("b").exists(), "file b must not exist on host");
    assert!(
        !home.join("started").exists(),
        "file started must not exist on host"
    );

    // staging gone
    if runs_dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "staging runs dir must be empty");
    }
}

#[test]
fn e2e_installer_reads_original_stdin() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !priv_env() || !sudo_env() {
        return;
    }

    let (_temp_home, home) = create_test_home();
    let answers_path = create_answers_file(&home, &["a", "n"]);

    let bin = installctl_bin();
    let mut cmd = std::process::Command::new(&bin);
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", &home)
        .env("INSTALLCTL_TEST_ANSWERS", &answers_path)
        .arg("sh")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("spawn installctl");
    use std::io::Write;
    if let Some(mut stdin_pipe) = child.stdin.take() {
        stdin_pipe
            .write_all(b"echo from-stdin > \"$HOME/stdin.txt\"\n")
            .expect("write to stdin");
    }

    let output = child.wait_with_output().expect("wait on installctl");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "installctl failed.\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
    );

    let stdin_txt = home.join("stdin.txt");
    assert!(stdin_txt.is_file(), "stdin.txt must exist");
    assert_eq!(
        std::fs::read_to_string(&stdin_txt).unwrap().trim(),
        "from-stdin"
    );
}

#[test]
fn e2e_missing_prereq_exit_3() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if std::path::Path::new("/dev/fuse").exists() {
        eprintln!("skipping: /dev/fuse is present");
        return;
    }

    let bin = installctl_bin();
    let output = std::process::Command::new(&bin)
        .args(["sh", "-c", "true"])
        .output()
        .expect("run installctl");

    assert_eq!(
        output.status.code(),
        Some(3),
        "expected exit 3 when prerequisites missing"
    );

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("/dev/fuse"),
        "expected output to mention /dev/fuse, got: {combined}"
    );
}

#[test]
fn e2e_preserve_arguments_with_spaces() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !priv_env() {
        return;
    }

    let (_temp_home, home) = create_test_home();
    let answers_path = create_answers_file(&home, &["a", "n"]);

    let bin = installctl_bin();
    let output = std::process::Command::new(&bin)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", &home)
        .env("INSTALLCTL_TEST_ANSWERS", &answers_path)
        .args([
            "--name",
            "spacetest",
            "sh",
            "-c",
            "touch \"$HOME/$1\"",
            "_",
            "arg with spaces",
        ])
        .output()
        .expect("run installctl");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "installctl failed.\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
    );

    let target_file = home.join("arg with spaces");
    assert!(
        target_file.is_file(),
        "file with spaces should exist at {}",
        target_file.display()
    );
}
