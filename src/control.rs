use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;

use crate::actions::DangerousAction;
use crate::ipc::{self, ExitStatus, HostCommand};
use crate::prompt::Prompter;
use crate::sandbox::SandboxEvents;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct ActionRequest {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub as_root: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    Skip,
    RunForReal,
    Abort,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ActionRecord {
    pub action: DangerousAction,
    pub decision: Decision,
    pub exit_code: Option<i32>,
}

pub type HostRunner<'a> = Box<dyn FnMut(&ActionRequest, [OwnedFd; 3]) -> io::Result<i32> + 'a>;

pub struct ActionController<'a> {
    pub records: Vec<ActionRecord>,
    prompter: &'a mut dyn Prompter,
    host_runner: HostRunner<'a>,
    skipped_commands: HashSet<String>,
}

impl<'a> ActionController<'a> {
    pub fn new(prompter: &'a mut dyn Prompter, host_runner: HostRunner<'a>) -> Self {
        Self {
            records: Vec::new(),
            prompter,
            host_runner,
            skipped_commands: HashSet::new(),
        }
    }
}

pub fn run_on_host(req: &ActionRequest, fds: [OwnedFd; 3]) -> io::Result<i32> {
    if req.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    }

    let mut cmd = if req.as_root {
        let mut c = std::process::Command::new("sudo");
        c.arg("--");
        c.args(&req.argv);
        c
    } else {
        let mut c = std::process::Command::new(&req.argv[0]);
        c.args(&req.argv[1..]);
        c
    };

    cmd.env_clear();
    for (k, v) in &req.env {
        if k == "PATH" {
            let filtered_path = v
                .split(':')
                .filter(|entry| !entry.starts_with("/run/installctl"))
                .collect::<Vec<_>>()
                .join(":");
            cmd.env(k, filtered_path);
        } else {
            cmd.env(k, v);
        }
    }

    let cwd = if req.cwd.exists() {
        req.cwd.clone()
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"))
    };
    cmd.current_dir(cwd);

    let [in_fd, out_fd, err_fd] = fds;
    cmd.stdin(std::process::Stdio::from(in_fd));
    cmd.stdout(std::process::Stdio::from(out_fd));
    cmd.stderr(std::process::Stdio::from(err_fd));

    let status = cmd.status()?;
    let code = status
        .code()
        .unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1));
    Ok(code)
}

impl<'a> SandboxEvents for ActionController<'a> {
    fn on_action_conn(&mut self, conn: UnixStream) -> Option<HostCommand> {
        let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let _ = conn.set_write_timeout(Some(std::time::Duration::from_secs(5)));

        let (req, fds) = match ipc::recv_msg::<ActionRequest>(&conn) {
            Ok(res) => res,
            Err(e) => {
                eprintln!("ActionController: failed to receive ActionRequest: {e}");
                let _ = ipc::send_msg(&conn, &ExitStatus { code: 1 }, &[]);
                return None;
            }
        };

        let action = crate::actions::classify(&req.argv, req.as_root);
        let cmd_key = req.argv.first().cloned().unwrap_or_default();

        let decision = if self.skipped_commands.contains(&cmd_key) {
            Decision::Skip
        } else {
            let text = format!(
                "installctl: the installer wants to run a command that changes your system outside the sandbox:\n  {}\n  → {}",
                crate::actions::display(&action),
                action.summary
            );
            self.prompter.say(&text);

            let options = [
                ('s', "skip it, tell the installer it succeeded"),
                ('r', "run it for real"),
                ('S', "skip this and every further one"),
                ('a', "abort the installation"),
            ];

            let choice = self.prompter.choose("What would you like to do?", &options);
            match choice {
                's' => Decision::Skip,
                'S' => {
                    self.skipped_commands.insert(cmd_key);
                    Decision::Skip
                }
                'r' => Decision::RunForReal,
                'a' => Decision::Abort,
                _ => Decision::Skip,
            }
        };

        match decision {
            Decision::Skip => {
                self.records.push(ActionRecord {
                    action,
                    decision: Decision::Skip,
                    exit_code: None,
                });
                let _ = ipc::send_msg(&conn, &ExitStatus { code: 0 }, &[]);
                None
            }
            Decision::RunForReal => {
                let fds_arr: [OwnedFd; 3] = match fds.try_into() {
                    Ok(arr) => arr,
                    Err(_) => {
                        eprintln!("ActionController: expected 3 fds for RunForReal");
                        let _ = ipc::send_msg(&conn, &ExitStatus { code: 1 }, &[]);
                        self.records.push(ActionRecord {
                            action,
                            decision: Decision::RunForReal,
                            exit_code: Some(1),
                        });
                        return None;
                    }
                };
                let exit_code = match (self.host_runner)(&req, fds_arr) {
                    Ok(code) => code,
                    Err(e) => {
                        eprintln!("ActionController: host runner failed: {e}");
                        1
                    }
                };
                self.records.push(ActionRecord {
                    action,
                    decision: Decision::RunForReal,
                    exit_code: Some(exit_code),
                });
                let _ = ipc::send_msg(&conn, &ExitStatus { code: exit_code }, &[]);
                None
            }
            Decision::Abort => {
                self.records.push(ActionRecord {
                    action,
                    decision: Decision::Abort,
                    exit_code: None,
                });
                let _ = ipc::send_msg(&conn, &ExitStatus { code: 1 }, &[]);
                Some(HostCommand::Kill)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use std::os::fd::AsFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn controller_skip_run_skipall_abort() {
        let mut answers =
            ScriptedPrompter::new(vec!["s".into(), "r".into(), "S".into(), "a".into()]);
        let runner_calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = runner_calls.clone();
        let host_runner = Box::new(move |_req: &ActionRequest, _fds: [OwnedFd; 3]| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        });

        let mut controller = ActionController::new(&mut answers, host_runner);

        let requests = [
            ActionRequest {
                argv: vec!["systemctl".into(), "start".into(), "s1".into()],
                env: vec![],
                cwd: PathBuf::from("/"),
                as_root: false,
            },
            ActionRequest {
                argv: vec!["systemctl".into(), "start".into(), "s2".into()],
                env: vec![],
                cwd: PathBuf::from("/"),
                as_root: false,
            },
            ActionRequest {
                argv: vec!["systemctl".into(), "start".into(), "s3".into()],
                env: vec![],
                cwd: PathBuf::from("/"),
                as_root: false,
            },
            ActionRequest {
                argv: vec!["systemctl".into(), "start".into(), "s4".into()],
                env: vec![],
                cwd: PathBuf::from("/"),
                as_root: false,
            },
            ActionRequest {
                argv: vec!["apt".into(), "install".into(), "pkg".into()],
                env: vec![],
                cwd: PathBuf::from("/"),
                as_root: false,
            },
        ];

        let mut last_cmd = None;
        for req in &requests {
            let (client, server) = UnixStream::pair().unwrap();
            let dev_null = std::fs::File::open("/dev/null").unwrap();
            let fds = [dev_null.as_fd(), dev_null.as_fd(), dev_null.as_fd()];
            ipc::send_msg(&client, req, &fds).unwrap();
            last_cmd = controller.on_action_conn(server);
        }

        let decisions: Vec<Decision> = controller.records.iter().map(|r| r.decision).collect();
        assert_eq!(
            decisions,
            vec![
                Decision::Skip,
                Decision::RunForReal,
                Decision::Skip,
                Decision::Skip,
                Decision::Abort,
            ]
        );
        assert_eq!(runner_calls.load(Ordering::SeqCst), 1);
        assert_eq!(last_cmd, Some(HostCommand::Kill));
    }
}
