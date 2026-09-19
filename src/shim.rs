use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::ipc;
use crate::sandbox::SANDBOX_RUN_DIR;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct SudoRequest {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub preserve_env: bool,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ParsedSudo {
    pub preserve_env: bool,
    pub run_as_current_user: bool,
    pub command: Vec<String>,
}

fn get_current_username() -> Option<String> {
    if let Ok(info) = crate::prereq::current_user() {
        return Some(info.name);
    }
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        return Some(user);
    }
    None
}

pub fn parse_sudo_args_full(args: &[String]) -> Result<ParsedSudo, String> {
    let mut i = 0;
    let mut preserve_env = false;
    let mut run_as_current_user = false;

    let current_user = get_current_username();

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            i += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        match arg.as_str() {
            "-E" | "--preserve-env" => {
                preserve_env = true;
                i += 1;
            }
            "-H" | "-n" | "-S" | "-k" => {
                i += 1;
            }
            "-u" => {
                i += 1;
                if i >= args.len() {
                    return Err(
                        "usage: sudo [-E] [-H] [-n] [-S] [-k] [-u user] command [arg ...]"
                            .to_string(),
                    );
                }
                let target = &args[i];
                i += 1;
                if target == "root" {
                    run_as_current_user = false;
                } else if current_user.as_deref() == Some(target.as_str()) {
                    run_as_current_user = true;
                } else {
                    return Err(format!(
                        "installctl: sudo option -u {} is not supported",
                        target
                    ));
                }
            }
            _ if arg.starts_with("-u") => {
                let target = &arg[2..];
                i += 1;
                if target == "root" {
                    run_as_current_user = false;
                } else if current_user.as_deref() == Some(target) {
                    run_as_current_user = true;
                } else {
                    return Err(format!(
                        "installctl: sudo option -u {} is not supported",
                        target
                    ));
                }
            }
            _ => {
                return Err(format!("installctl: sudo option {} is not supported", arg));
            }
        }
    }

    let command = args[i..].to_vec();
    if command.is_empty() {
        return Err("usage: sudo [-E] [-H] [-n] [-S] [-k] [-u user] command [arg ...]".to_string());
    }

    Ok(ParsedSudo {
        preserve_env,
        run_as_current_user,
        command,
    })
}

pub fn parse_sudo_args(
    args: &[String],
) -> Result<(bool /*preserve_env*/, Vec<String> /*command*/), String> {
    let parsed = parse_sudo_args_full(args)?;
    Ok((parsed.preserve_env, parsed.command))
}

pub fn sudo_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match parse_sudo_args_full(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    if parsed.run_as_current_user {
        let c_cmd = match CString::new(parsed.command[0].as_str()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("sudo: invalid command name: {e}");
                return 1;
            }
        };
        let c_args: Vec<CString> = parsed
            .command
            .iter()
            .map(|s| CString::new(s.as_str()).unwrap_or_default())
            .collect();

        let _ = nix::unistd::execvp(&c_cmd, &c_args);
        eprintln!("sudo: {}: command not found", parsed.command[0]);
        return 127;
    }

    let sock_path = format!("{}/sudo.sock", SANDBOX_RUN_DIR);
    let sock = match UnixStream::connect(&sock_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sudo: cannot connect to sudo service at {sock_path}: {e}");
            return 1;
        }
    };

    let env: Vec<(String, String)> = std::env::vars().collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    let req = SudoRequest {
        argv: parsed.command,
        env,
        cwd,
        preserve_env: parsed.preserve_env,
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();

    if let Err(e) = ipc::send_msg(
        &sock,
        &req,
        &[stdin.as_fd(), stdout.as_fd(), stderr.as_fd()],
    ) {
        eprintln!("sudo: failed to send request: {e}");
        return 1;
    }

    match ipc::recv_msg::<ipc::ExitStatus>(&sock) {
        Ok((status, _)) => status.code,
        Err(e) => {
            eprintln!("sudo: failed to receive response: {e}");
            1
        }
    }
}

pub fn action_main() -> i32 {
    let mut argv: Vec<String> = std::env::args().collect();
    if !argv.is_empty() {
        let basename = std::path::Path::new(&argv[0])
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&argv[0])
            .to_string();
        argv[0] = basename;
    }

    let sock_path = format!("{}/action.sock", SANDBOX_RUN_DIR);
    let sock = match UnixStream::connect(&sock_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "installctl action shim: cannot connect to action service at {sock_path}: {e}"
            );
            return 1;
        }
    };

    let env: Vec<(String, String)> = std::env::vars().collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let as_root = nix::unistd::getuid().as_raw() == 0 || nix::unistd::geteuid().as_raw() == 0;

    let req = crate::control::ActionRequest {
        argv,
        env,
        cwd,
        as_root,
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();

    if let Err(e) = ipc::send_msg(
        &sock,
        &req,
        &[stdin.as_fd(), stdout.as_fd(), stderr.as_fd()],
    ) {
        eprintln!("installctl action shim: failed to send request: {e}");
        return 1;
    }

    match ipc::recv_msg::<ipc::ExitStatus>(&sock) {
        Ok((status, _)) => status.code,
        Err(e) => {
            eprintln!("installctl action shim: failed to receive response: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sudo_args_preserve_env() {
        let args: Vec<String> = vec!["-E", "sh", "-c", "x"]
            .into_iter()
            .map(String::from)
            .collect();
        let (preserve_env, cmd) = parse_sudo_args(&args).unwrap();
        assert!(preserve_env);
        assert_eq!(cmd, vec!["sh", "-c", "x"]);
    }

    #[test]
    fn parse_sudo_args_preserve_env_long() {
        let args: Vec<String> = vec!["--preserve-env", "id"]
            .into_iter()
            .map(String::from)
            .collect();
        let (preserve_env, cmd) = parse_sudo_args(&args).unwrap();
        assert!(preserve_env);
        assert_eq!(cmd, vec!["id"]);
    }

    #[test]
    fn parse_sudo_args_user_root_separator() {
        let args: Vec<String> = vec!["-u", "root", "--", "id"]
            .into_iter()
            .map(String::from)
            .collect();
        let (preserve_env, cmd) = parse_sudo_args(&args).unwrap();
        assert!(!preserve_env);
        assert_eq!(cmd, vec!["id"]);
    }

    #[test]
    fn parse_sudo_args_user_bob_err() {
        let args: Vec<String> = vec!["-u", "bob", "id"]
            .into_iter()
            .map(String::from)
            .collect();
        let res = parse_sudo_args(&args);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("bob"));
    }

    #[test]
    fn parse_sudo_args_empty_err() {
        let args: Vec<String> = vec![];
        let res = parse_sudo_args(&args);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("usage"));
    }

    #[test]
    fn parse_sudo_args_unsupported_flag_err() {
        let args: Vec<String> = vec!["-x", "id"].into_iter().map(String::from).collect();
        let res = parse_sudo_args(&args);
        assert!(res.is_err());
        assert_eq!(
            res.unwrap_err(),
            "installctl: sudo option -x is not supported"
        );
    }

    #[test]
    fn parse_sudo_args_current_user() {
        if let Some(user) = get_current_username() {
            let args: Vec<String> = vec!["-u", &user, "whoami"]
                .into_iter()
                .map(String::from)
                .collect();
            let parsed = parse_sudo_args_full(&args).unwrap();
            assert!(parsed.run_as_current_user);
            assert_eq!(parsed.command, vec!["whoami"]);
        }
    }
}
