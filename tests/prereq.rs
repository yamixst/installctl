mod common;
use common::priv_env;

#[test]
fn docker_prereqs_satisfied() {
    if !priv_env() {
        return;
    }
    let user = installctl::prereq::current_user().expect("failed to get current user");
    let missing = installctl::prereq::check(&user);
    assert_eq!(
        missing,
        Vec::new(),
        "unexpected missing prerequisites: {missing:?}"
    );
}
