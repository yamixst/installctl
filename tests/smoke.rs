mod common;

#[test]
fn binary_runs() {
    let bin = env!("CARGO_BIN_EXE_installctl");
    let status = std::process::Command::new(bin)
        .arg("--help")
        .status()
        .expect("failed to execute binary");
    assert!(status.success(), "binary exited with failure: {:?}", status);
}

#[test]
fn privileged_env_works() {
    if !common::priv_env() {
        return;
    }

    let status = std::process::Command::new("unshare")
        .args(["-Urm", "true"])
        .status()
        .expect("failed to execute unshare");
    assert!(status.success(), "unshare -Urm true failed: {:?}", status);

    let fuse = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse");
    assert!(fuse.is_ok(), "failed to open /dev/fuse: {:?}", fuse.err());
}
