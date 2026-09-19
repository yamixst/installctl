mod common;

use installctl::changes::{ChangeKind, collect};
use installctl::ipc::HostCommand;
use installctl::overlay::UpperNode;
use installctl::prereq::current_user;
use installctl::sandbox::{SandboxConfig, SandboxEvents, SandboxOutcome, run_sandboxed};
use installctl::staging::{self, StagingDir};
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

struct NoopEvents;
impl SandboxEvents for NoopEvents {
    fn on_action_conn(&mut self, _conn: UnixStream) -> Option<HostCommand> {
        None
    }
}

fn create_sbx_home() -> (tempfile::TempDir, PathBuf) {
    let user = current_user().expect("current user");
    let base = user.home;
    fs::create_dir_all(&base).unwrap();
    let temp = tempfile::Builder::new()
        .prefix("sbx-")
        .tempdir_in(&base)
        .expect("create sbx home");
    let path = temp.path().to_path_buf();
    (temp, path)
}

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_with_home(home: &Path, cmd: &str) -> SandboxOutcome {
    let _guard = TEST_LOCK.lock().unwrap();

    let user_info = current_user().expect("current user");
    let mut user = user_info;
    user.home = home.to_path_buf();

    let state_home = staging::state_home(&user);
    let staging = StagingDir::create(&state_home).expect("create staging dir");

    let exe = staging.run_dir().join("bin/installctl");
    if let Ok(cargo_bin) = std::env::var("CARGO_BIN_EXE_installctl") {
        fs::copy(cargo_bin, &exe).expect("copy installctl exe");
    } else {
        let cur_exe = std::env::current_exe().expect("current exe");
        fs::copy(cur_exe, &exe).expect("copy current exe");
    }

    let cfg = SandboxConfig {
        command: cmd.to_string(),
        user,
        staging_upper: staging.upper(),
        run_dir: staging.run_dir(),
        mountpoint: staging.mountpoint(),
        exe,
        shim_targets: Vec::new(),
        cwd: home.to_path_buf(),
    };

    let mut events = NoopEvents;
    run_sandboxed(&cfg, &mut events).expect("run_sandboxed failed")
}

#[allow(dead_code)]
fn run(cmd: &str) -> SandboxOutcome {
    let (_keep_temp, home) = create_sbx_home();
    run_with_home(&home, cmd)
}

#[test]
fn writes_are_staged_not_real() {
    if !common::priv_env() {
        return;
    }
    let (_keep_temp, home) = create_sbx_home();
    let profile = home.join(".profile");
    fs::write(&profile, "original profile\n").expect("write original profile");

    let outcome = run_with_home(
        &home,
        "echo hi > $HOME/a; mkdir -p $HOME/.local/bin; echo x >> $HOME/.profile",
    );
    assert_eq!(outcome.exit_code, 0);

    // Host $HOME/a absent
    assert!(!home.join("a").exists(), "host $HOME/a should not exist");

    // .profile unchanged on host
    let profile_content = fs::read_to_string(&profile).expect("read profile");
    assert_eq!(
        profile_content, "original profile\n",
        "host .profile should be unchanged"
    );

    // changes::collect has Created $HOME/a
    let changes = collect(&outcome.overlay).expect("collect changes");
    let has_created_a = changes
        .iter()
        .any(|c| c.path == home.join("a") && c.kind == ChangeKind::Created);
    assert!(
        has_created_a,
        "changes should contain Created for $HOME/a: {:?}",
        changes
    );
}

#[test]
fn grandchildren_are_captured() {
    if !common::priv_env() {
        return;
    }
    let (_keep_temp, home) = create_sbx_home();
    let outcome = run_with_home(&home, "sh -c 'sh -c \"echo gc > $HOME/gc\"'");
    assert_eq!(outcome.exit_code, 0);

    let changes = collect(&outcome.overlay).expect("collect changes");
    let has_created_gc = changes
        .iter()
        .any(|c| c.path == home.join("gc") && c.kind == ChangeKind::Created);
    assert!(
        has_created_gc,
        "changes should contain Created for $HOME/gc: {:?}",
        changes
    );
}

#[test]
fn reads_see_own_writes() {
    if !common::priv_env() {
        return;
    }
    let (_keep_temp, home) = create_sbx_home();
    let outcome = run_with_home(&home, "echo 1 > $HOME/f && cat $HOME/f > $HOME/g");
    assert_eq!(outcome.exit_code, 0);

    let g_path = home.join("g");
    let node = outcome.overlay.nodes().get(&g_path);
    match node {
        Some(UpperNode::File { blob, .. }) => {
            let content = fs::read_to_string(blob).expect("read blob of g");
            assert_eq!(content, "1\n", "blob of g should be '1\\n'");
        }
        other => panic!("expected File node for $HOME/g, got {:?}", other),
    }
}

#[test]
fn tmp_is_private() {
    if !common::priv_env() {
        return;
    }
    let outcome = run("echo t > /tmp/x");
    assert_eq!(outcome.exit_code, 0);

    assert!(
        !Path::new("/tmp/x").exists(),
        "host /tmp/x should not exist"
    );

    let changes = collect(&outcome.overlay).expect("collect changes");
    let tmp_changes: Vec<_> = changes
        .iter()
        .filter(|c| c.path.starts_with("/tmp"))
        .collect();
    assert!(
        tmp_changes.is_empty(),
        "no tmp changes should be reported: {:?}",
        tmp_changes
    );
}

#[test]
fn plain_user_cannot_write_system_dirs() {
    if !common::priv_env() {
        return;
    }
    let outcome = run("touch /usr/local/bin/x");
    assert_ne!(
        outcome.exit_code, 0,
        "touch in /usr/local/bin should fail for plain user"
    );

    let changes = collect(&outcome.overlay).expect("collect changes");
    let usr_changes: Vec<_> = changes
        .iter()
        .filter(|c| c.path.starts_with("/usr/local/bin"))
        .collect();
    assert!(
        usr_changes.is_empty(),
        "no system dir changes should be recorded: {:?}",
        usr_changes
    );
}

#[test]
fn background_process_reported() {
    if !common::priv_env() {
        return;
    }
    let outcome = run("(sleep 300 &) ; true");
    assert_eq!(outcome.exit_code, 0);

    assert_eq!(
        outcome.leftovers.len(),
        1,
        "expected 1 leftover process, got: {:?}",
        outcome.leftovers
    );
    assert!(
        outcome.leftovers[0].cmdline.contains("sleep 300"),
        "cmdline should contain 'sleep 300': {}",
        outcome.leftovers[0].cmdline
    );

    let output = std::process::Command::new("pgrep")
        .args(["-f", "sleep 300"])
        .output()
        .expect("pgrep");
    let pgrep_stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        pgrep_stdout.trim().is_empty(),
        "no sleep process should survive on host: {}",
        pgrep_stdout
    );
}

#[test]
fn exit_code_propagates() {
    if !common::priv_env() {
        return;
    }
    let outcome = run("exit 7");
    assert_eq!(outcome.exit_code, 7);
}

#[test]
fn sigint_terminates_child() {
    if !common::priv_env() {
        return;
    }
    let (_keep_temp, home) = create_sbx_home();
    let outcome = run_with_home(&home, "kill -INT $$; echo survived > $HOME/survived");
    assert_eq!(outcome.exit_code, 130);
    assert!(!home.join("survived").exists());
}

#[test]
fn sudo_writes_system_path_staged() {
    if !common::priv_env() {
        return;
    }
    let (_keep_temp, home) = create_sbx_home();
    let outcome = run_with_home(
        &home,
        "sudo sh -c 'echo x > /usr/local/bin/tool && chmod 755 /usr/local/bin/tool' && test \"$(sudo id -u)\" = 0",
    );
    assert_eq!(outcome.exit_code, 0);

    let host_tool = Path::new("/usr/local/bin/tool");
    assert!(
        !host_tool.exists(),
        "host /usr/local/bin/tool should not exist"
    );

    let changes = collect(&outcome.overlay).expect("collect changes");
    let change = changes
        .iter()
        .find(|c| c.path == host_tool)
        .expect("/usr/local/bin/tool should be in changes");
    assert_eq!(change.kind, ChangeKind::Created);

    let node = outcome
        .overlay
        .nodes()
        .get(host_tool)
        .expect("node for /usr/local/bin/tool in overlay");
    match node {
        UpperNode::File { meta, .. } => {
            assert_eq!((meta.uid, meta.gid), (0, 0), "owner should be (0, 0)");
            assert_eq!(meta.mode & 0o777, 0o755, "mode should be 0o755");
        }
        other => panic!("expected File node, got {:?}", other),
    }
}

#[test]
fn sudo_never_asks_password() {
    if !common::priv_env() {
        return;
    }
    let outcome = run("sudo true < /dev/null");
    assert_eq!(outcome.exit_code, 0);
}

fn run_with_answers(
    home: &Path,
    cmd: &str,
    answers: &str,
) -> (SandboxOutcome, Vec<installctl::control::ActionRecord>) {
    let user_info = current_user().expect("current user");
    let mut user = user_info;
    user.home = home.to_path_buf();

    let state_home = staging::state_home(&user);
    let staging = StagingDir::create(&state_home).expect("create staging dir");

    let exe = staging.run_dir().join("bin/installctl");
    if let Ok(cargo_bin) = std::env::var("CARGO_BIN_EXE_installctl") {
        fs::copy(cargo_bin, &exe).expect("copy installctl exe");
    } else {
        let cur_exe = std::env::current_exe().expect("current exe");
        fs::copy(cur_exe, &exe).expect("copy current exe");
    }

    let cfg = SandboxConfig {
        command: cmd.to_string(),
        user,
        staging_upper: staging.upper(),
        run_dir: staging.run_dir(),
        mountpoint: staging.mountpoint(),
        exe,
        shim_targets: Vec::new(),
        cwd: home.to_path_buf(),
    };

    let answers_file = staging.run_dir().join("test_answers.txt");
    fs::write(&answers_file, answers).expect("write answers");
    unsafe {
        std::env::set_var("INSTALLCTL_TEST_ANSWERS", answers_file.to_str().unwrap());
    }

    let mut prompter = installctl::prompt::from_env().expect("prompter from env");
    let mut controller = installctl::control::ActionController::new(
        &mut *prompter,
        Box::new(installctl::control::run_on_host),
    );

    let outcome = run_sandboxed(&cfg, &mut controller).expect("run_sandboxed failed");
    unsafe {
        std::env::remove_var("INSTALLCTL_TEST_ANSWERS");
    }

    (outcome, controller.records)
}

#[test]
fn systemctl_skip_returns_success() {
    if !common::priv_env() {
        return;
    }
    let _guard = TEST_LOCK.lock().unwrap();
    let (_keep_temp, home) = create_sbx_home();
    let fake_log = Path::new("/var/log/fake-real-actions.log");
    let log_before = fs::read_to_string(fake_log).unwrap_or_default();

    let (outcome, records) = run_with_answers(
        &home,
        "set -e; systemctl --user enable --now x.service; echo after > $HOME/after",
        "s\n",
    );
    assert_eq!(outcome.exit_code, 0);

    let changes = collect(&outcome.overlay).expect("collect changes");
    let has_created_after = changes
        .iter()
        .any(|c| c.path == home.join("after") && c.kind == ChangeKind::Created);
    assert!(
        has_created_after,
        "changes should contain Created for $HOME/after: {:?}",
        changes
    );

    let log_after = fs::read_to_string(fake_log).unwrap_or_default();
    assert_eq!(
        log_before, log_after,
        "/var/log/fake-real-actions.log should not have changed"
    );

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision, installctl::control::Decision::Skip);
}

#[test]
fn apt_run_for_real_uses_real_sudo() {
    if !common::priv_env() {
        return;
    }
    let _guard = TEST_LOCK.lock().unwrap();
    let (_keep_temp, home) = create_sbx_home();
    let fake_log = Path::new("/var/log/fake-real-actions.log");
    let log_before = fs::read_to_string(fake_log).unwrap_or_default();

    let (outcome, records) = run_with_answers(&home, "sudo apt-get install -y fakepkg", "r\n");
    assert_eq!(outcome.exit_code, 0);

    let log_after = fs::read_to_string(fake_log).unwrap_or_default();
    let diff = &log_after[log_before.len()..];
    assert!(
        diff.lines().any(|l| l.trim() == "0 install -y fakepkg"),
        "expected '0 install -y fakepkg' in log diff, got: {:?}",
        diff
    );

    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].decision,
        installctl::control::Decision::RunForReal
    );
    assert_eq!(records[0].exit_code, Some(0));
}

#[test]
fn abort_kills_installer() {
    if !common::priv_env() {
        return;
    }
    let _guard = TEST_LOCK.lock().unwrap();
    let (_keep_temp, home) = create_sbx_home();

    let (outcome, records) = run_with_answers(&home, "systemctl start x; touch $HOME/never", "a\n");
    assert!(outcome.aborted, "outcome should be aborted");
    assert!(
        !home.join("never").exists(),
        "$HOME/never should not exist on host"
    );

    let changes = collect(&outcome.overlay).expect("collect changes");
    let has_never = changes.iter().any(|c| c.path == home.join("never"));
    assert!(!has_never, "changes should not contain $HOME/never");

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision, installctl::control::Decision::Abort);
}
