pub mod actions;
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
    let argv0 = std::env::args().next().unwrap_or_default();
    let basename = std::path::Path::new(&argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match basename {
        "sudo" => shim::sudo_main(),
        "installctl" | "" => 0,
        _ => shim::action_main(),
    }
}
