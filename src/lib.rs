pub mod actions;
pub mod apply;
pub mod changes;
pub mod control;
pub mod ipc;
pub mod overlay;
pub mod prereq;
pub mod prompt;
pub mod report;
pub mod sandbox;
pub mod shim;
pub mod staging;

pub use control::{ActionController, ActionRecord, ActionRequest, Decision, run_on_host};
pub use prompt::Prompter;
pub use sandbox::SANDBOX_RUN_DIR;
pub use shim::SudoRequest;

pub fn main_entry() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().map(|s| s.as_str()).unwrap_or_default();
    let basename = std::path::Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    match basename {
        "sudo" => shim::sudo_main(),
        "installctl" | "" => {
            if let Some(pos) = args.iter().position(|a| a == "--apply-helper") {
                if let Some(plan_str) = args.get(pos + 1) {
                    let plan = std::path::Path::new(plan_str);
                    if let Err(e) = apply::helper_main(plan) {
                        eprintln!("installctl helper error: {e:#}");
                        return 1;
                    }
                    return 0;
                } else {
                    eprintln!("installctl: missing plan argument for --apply-helper");
                    return 1;
                }
            }
            0
        }
        _ => shim::action_main(),
    }
}
