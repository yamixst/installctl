pub mod actions;
pub mod app;
pub mod apply;
pub mod changes;
pub mod cli;
pub mod control;
pub mod ipc;
pub mod naming;
pub mod overlay;
pub mod prereq;
pub mod prompt;
pub mod report;
pub mod sandbox;
pub mod shim;
pub mod staging;
pub mod uninstall;

pub use control::{ActionController, ActionRecord, ActionRequest, Decision, run_on_host};
pub use prompt::Prompter;
pub use sandbox::SANDBOX_RUN_DIR;
pub use shim::SudoRequest;

use clap::Parser;

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
            let parsed = match cli::Args::try_parse_from(&args) {
                Ok(p) => p,
                Err(e) => {
                    let _ = e.print();
                    return e.exit_code();
                }
            };
            if let Some(plan) = parsed.apply_helper {
                if let Err(e) = apply::helper_main(&plan) {
                    eprintln!("installctl helper error: {e:#}");
                    return 1;
                }
                return 0;
            }
            app::run(parsed)
        }
        _ => shim::action_main(),
    }
}
