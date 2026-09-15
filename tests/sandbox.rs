mod common;

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use installctl::changes::{collect, ChangeKind};
use installctl::ipc::HostCommand;
use installctl::overlay::UpperNode;
use installctl::prereq::current_user;
use installctl::sandbox::{run_sandboxed, SandboxConfig, SandboxEvents, SandboxOutcome};
use installctl::staging::{self, StagingDir};

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

    assert!(!Path::new("/tmp/x").exists(), "host /tmp/x should not exist");

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
