use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Category {
    Packages,
    Accounts,
    Services,
    Scheduling,
    System,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DangerousAction {
    pub argv: Vec<String>,
    pub as_root: bool,
    pub category: Category,
    pub summary: String,
}

pub const COMMANDS: &[(&str, Category)] = &[
    // Packages
    ("apt", Category::Packages),
    ("apt-get", Category::Packages),
    ("dpkg", Category::Packages),
    ("dnf", Category::Packages),
    ("yum", Category::Packages),
    ("rpm", Category::Packages),
    ("pacman", Category::Packages),
    ("zypper", Category::Packages),
    ("apk", Category::Packages),
    ("snap", Category::Packages),
    ("flatpak", Category::Packages),
    // Accounts
    ("useradd", Category::Accounts),
    ("usermod", Category::Accounts),
    ("userdel", Category::Accounts),
    ("groupadd", Category::Accounts),
    ("groupmod", Category::Accounts),
    ("groupdel", Category::Accounts),
    ("adduser", Category::Accounts),
    ("addgroup", Category::Accounts),
    // Services
    ("systemctl", Category::Services),
    ("service", Category::Services),
    ("loginctl", Category::Services),
    ("update-rc.d", Category::Services),
    ("chkconfig", Category::Services),
    // Scheduling
    ("crontab", Category::Scheduling),
    ("at", Category::Scheduling),
    // System
    ("ldconfig", Category::System),
    ("modprobe", Category::System),
    ("sysctl", Category::System),
];

fn is_safe_shell_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '_' | '.' | '/' | '=' | ':' | ',' | '+' | '@' | '%' | '-')
}

fn quote_shell_word(word: &str) -> String {
    if !word.is_empty() && word.chars().all(is_safe_shell_char) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

pub fn shell_quote(argv: &[String]) -> String {
    argv.iter()
        .map(|w| quote_shell_word(w))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn display(action: &DangerousAction) -> String {
    let quoted = shell_quote(&action.argv);
    if action.as_root {
        if quoted.is_empty() {
            "sudo".to_string()
        } else {
            format!("sudo {quoted}")
        }
    } else {
        quoted
    }
}

fn command_basename(cmd: &str) -> &str {
    Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd)
}

pub fn classify(argv: &[String], as_root: bool) -> DangerousAction {
    if argv.is_empty() {
        return DangerousAction {
            argv: Vec::new(),
            as_root,
            category: Category::System,
            summary: "runs unknown command (changes system state)".to_string(),
        };
    }

    let cmd = command_basename(&argv[0]);
    let category = COMMANDS
        .iter()
        .find(|(name, _)| *name == cmd)
        .map(|(_, cat)| *cat)
        .unwrap_or(Category::System);

    let summary = match category {
        Category::Packages => classify_packages(cmd, &argv[1..]),
        Category::Services => classify_services(cmd, &argv[1..]),
        Category::Accounts => classify_accounts(cmd, &argv[1..]),
        Category::Scheduling => classify_scheduling(cmd, &argv[1..]),
        Category::System => classify_system(cmd),
    };

    DangerousAction {
        argv: argv.to_vec(),
        as_root,
        category,
        summary,
    }
}

fn classify_packages(cmd: &str, args: &[String]) -> String {
    match cmd {
        "pacman" => {
            let is_install = args.iter().any(|a| a.starts_with("-S"));
            let is_remove = args.iter().any(|a| a.starts_with("-R"));
            let pkgs: Vec<&str> = args
                .iter()
                .filter(|a| !a.starts_with('-'))
                .map(|s| s.as_str())
                .collect();
            if is_install {
                if pkgs.is_empty() {
                    format!("installs system packages via {cmd}")
                } else {
                    format!("installs system packages {} via {cmd}", pkgs.join(", "))
                }
            } else if is_remove {
                if pkgs.is_empty() {
                    format!("removes system packages via {cmd}")
                } else {
                    format!("removes system packages {} via {cmd}", pkgs.join(", "))
                }
            } else {
                classify_system(cmd)
            }
        }
        "apk" => {
            let is_add = args.iter().any(|a| a == "add");
            let is_del = args.iter().any(|a| a == "del");
            let pkgs: Vec<&str> = args
                .iter()
                .skip_while(|a| a.as_str() != "add" && a.as_str() != "del")
                .skip(1)
                .filter(|a| !a.starts_with('-'))
                .map(|s| s.as_str())
                .collect();
            if is_add {
                if pkgs.is_empty() {
                    format!("installs system packages via {cmd}")
                } else {
                    format!("installs system packages {} via {cmd}", pkgs.join(", "))
                }
            } else if is_del {
                if pkgs.is_empty() {
                    format!("removes system packages via {cmd}")
                } else {
                    format!("removes system packages {} via {cmd}", pkgs.join(", "))
                }
            } else {
                classify_system(cmd)
            }
        }
        "dpkg" => {
            let is_install = args.iter().any(|a| a == "-i" || a == "--install");
            let is_remove = args
                .iter()
                .any(|a| a == "-r" || a == "--remove" || a == "-P" || a == "--purge");
            let pkgs: Vec<&str> = args
                .iter()
                .filter(|a| !a.starts_with('-'))
                .map(|s| s.as_str())
                .collect();
            if is_install {
                if pkgs.is_empty() {
                    format!("installs system packages via {cmd}")
                } else {
                    format!("installs system packages {} via {cmd}", pkgs.join(", "))
                }
            } else if is_remove {
                if pkgs.is_empty() {
                    format!("removes system packages via {cmd}")
                } else {
                    format!("removes system packages {} via {cmd}", pkgs.join(", "))
                }
            } else {
                classify_system(cmd)
            }
        }
        "rpm" => {
            let is_install = args
                .iter()
                .any(|a| a.starts_with("-i") || a.starts_with("-U") || a == "--install");
            let is_remove = args.iter().any(|a| a.starts_with("-e") || a == "--erase");
            let pkgs: Vec<&str> = args
                .iter()
                .filter(|a| !a.starts_with('-'))
                .map(|s| s.as_str())
                .collect();
            if is_install {
                if pkgs.is_empty() {
                    format!("installs system packages via {cmd}")
                } else {
                    format!("installs system packages {} via {cmd}", pkgs.join(", "))
                }
            } else if is_remove {
                if pkgs.is_empty() {
                    format!("removes system packages via {cmd}")
                } else {
                    format!("removes system packages {} via {cmd}", pkgs.join(", "))
                }
            } else {
                classify_system(cmd)
            }
        }
        _ => {
            // apt, apt-get, dnf, yum, zypper, snap, flatpak
            let subcommand = args
                .iter()
                .find(|a| !a.starts_with('-'))
                .map(|s| s.as_str());
            let pkgs: Vec<&str> = if let Some(sub) = subcommand {
                args.iter()
                    .skip_while(|a| a.as_str() != sub)
                    .skip(1)
                    .filter(|a| !a.starts_with('-'))
                    .map(|s| s.as_str())
                    .collect()
            } else {
                Vec::new()
            };

            match subcommand {
                Some("install") => {
                    if pkgs.is_empty() {
                        format!("installs system packages via {cmd}")
                    } else {
                        format!("installs system packages {} via {cmd}", pkgs.join(", "))
                    }
                }
                Some("remove") | Some("purge") | Some("uninstall") => {
                    if pkgs.is_empty() {
                        format!("removes system packages via {cmd}")
                    } else {
                        format!("removes system packages {} via {cmd}", pkgs.join(", "))
                    }
                }
                Some("update") | Some("upgrade") => {
                    format!("updates system packages via {cmd}")
                }
                _ => classify_system(cmd),
            }
        }
    }
}

fn classify_services(cmd: &str, args: &[String]) -> String {
    match cmd {
        "systemctl" => {
            let is_user = args.iter().any(|a| a == "--user");
            let scope = if is_user { "user" } else { "system" };
            let has_now = args.iter().any(|a| a == "--now");

            let action = args.iter().find(|a| {
                matches!(
                    a.as_str(),
                    "enable"
                        | "disable"
                        | "start"
                        | "stop"
                        | "restart"
                        | "reload"
                        | "mask"
                        | "unmask"
                )
            });

            let units: Vec<&str> = args
                .iter()
                .filter(|a| !a.starts_with('-') && Some(*a) != action)
                .map(|s| s.as_str())
                .collect();

            let verb = match (action.map(|s| s.as_str()), has_now) {
                (Some("enable"), true) => "enables and starts",
                (Some("enable"), false) => "enables",
                (Some("disable"), true) => "disables and stops",
                (Some("disable"), false) => "disables",
                (Some("start"), _) => "starts",
                (Some("stop"), _) => "stops",
                (Some("restart"), _) => "restarts",
                (Some("reload"), _) => "reloads",
                (Some("mask"), _) => "masks",
                (Some("unmask"), _) => "unmasks",
                _ => "",
            };

            if verb.is_empty() {
                classify_system(cmd)
            } else if units.is_empty() {
                format!("{verb} {scope} service")
            } else {
                format!("{verb} {scope} service {}", units.join(", "))
            }
        }
        "service" => {
            let non_flags: Vec<&str> = args
                .iter()
                .filter(|a| !a.starts_with('-'))
                .map(|s| s.as_str())
                .collect();
            if non_flags.len() >= 2 {
                let name = non_flags[0];
                let action = non_flags[1];
                let verb = match action {
                    "start" => "starts",
                    "stop" => "stops",
                    "restart" => "restarts",
                    "reload" => "reloads",
                    _ => "",
                };
                if !verb.is_empty() {
                    format!("{verb} system service {name}")
                } else {
                    classify_system(cmd)
                }
            } else {
                classify_system(cmd)
            }
        }
        "loginctl" => {
            if args.iter().any(|a| a == "enable-linger") {
                let target = args
                    .iter()
                    .skip_while(|a| a.as_str() != "enable-linger")
                    .skip(1)
                    .find(|a| !a.starts_with('-'));
                if let Some(user) = target {
                    format!("enables lingering for user {user}")
                } else {
                    "enables lingering".to_string()
                }
            } else {
                classify_system(cmd)
            }
        }
        _ => classify_system(cmd),
    }
}

fn classify_accounts(cmd: &str, args: &[String]) -> String {
    let target = args
        .iter()
        .rfind(|a| !a.starts_with('-'))
        .map(|s| s.as_str());

    match cmd {
        "useradd" | "adduser" => match target {
            Some(t) => format!("creates user account {t}"),
            None => "creates user account".to_string(),
        },
        "userdel" => match target {
            Some(t) => format!("deletes user account {t}"),
            None => "deletes user account".to_string(),
        },
        "usermod" => match target {
            Some(t) => format!("modifies user account {t}"),
            None => "modifies user account".to_string(),
        },
        "groupadd" | "addgroup" => match target {
            Some(t) => format!("creates group {t}"),
            None => "creates group".to_string(),
        },
        "groupdel" => match target {
            Some(t) => format!("deletes group {t}"),
            None => "deletes group".to_string(),
        },
        "groupmod" => match target {
            Some(t) => format!("modifies group {t}"),
            None => "modifies group".to_string(),
        },
        _ => classify_system(cmd),
    }
}

fn classify_scheduling(cmd: &str, args: &[String]) -> String {
    match cmd {
        "crontab" => {
            if args.iter().any(|a| a == "-r" || a == "--remove") {
                "removes your crontab".to_string()
            } else {
                "replaces your crontab".to_string()
            }
        }
        "at" => "schedules command via at".to_string(),
        _ => classify_system(cmd),
    }
}

fn classify_system(cmd: &str) -> String {
    format!("runs {cmd} (changes system state)")
}

pub fn inverse(argv: &[String]) -> Option<Vec<String>> {
    if argv.is_empty() {
        return None;
    }

    let cmd = command_basename(&argv[0]);

    match cmd {
        "apt" | "apt-get" | "dnf" | "yum" | "zypper" | "snap" => {
            let sub_idx = argv[1..].iter().position(|a| a == "install")?;
            let pkgs: Vec<String> = argv[1..][sub_idx + 1..]
                .iter()
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            if pkgs.is_empty() {
                return None;
            }
            let mut res = vec![cmd.to_string(), "remove".to_string()];
            res.extend(pkgs);
            Some(res)
        }
        "flatpak" => {
            let sub_idx = argv[1..].iter().position(|a| a == "install")?;
            let positional: Vec<String> = argv[1..][sub_idx + 1..]
                .iter()
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            if positional.is_empty() {
                return None;
            }
            let refs = if positional.len() >= 2 {
                &positional[1..]
            } else {
                &positional[..]
            };
            let mut res = vec!["flatpak".to_string(), "uninstall".to_string()];
            res.extend(refs.iter().cloned());
            Some(res)
        }
        "pacman" => {
            let is_install = argv[1..].iter().any(|a| a.starts_with("-S"));
            if !is_install {
                return None;
            }
            let pkgs: Vec<String> = argv[1..]
                .iter()
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            if pkgs.is_empty() {
                return None;
            }
            let mut res = vec!["pacman".to_string(), "-R".to_string()];
            res.extend(pkgs);
            Some(res)
        }
        "apk" => {
            let sub_idx = argv[1..].iter().position(|a| a == "add")?;
            let pkgs: Vec<String> = argv[1..][sub_idx + 1..]
                .iter()
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            if pkgs.is_empty() {
                return None;
            }
            let mut res = vec!["apk".to_string(), "del".to_string()];
            res.extend(pkgs);
            Some(res)
        }
        "systemctl" => {
            if argv[1..].iter().any(|a| a == "enable") {
                let res = argv
                    .iter()
                    .map(|arg| {
                        if arg == "enable" {
                            "disable".to_string()
                        } else {
                            arg.clone()
                        }
                    })
                    .collect();
                Some(res)
            } else if argv[1..].iter().any(|a| a == "start") {
                let res = argv
                    .iter()
                    .map(|arg| {
                        if arg == "start" {
                            "stop".to_string()
                        } else {
                            arg.clone()
                        }
                    })
                    .collect();
                Some(res)
            } else {
                None
            }
        }
        "service" => {
            if argv[1..].iter().any(|a| a == "start") {
                let res = argv
                    .iter()
                    .map(|arg| {
                        if arg == "start" {
                            "stop".to_string()
                        } else {
                            arg.clone()
                        }
                    })
                    .collect();
                Some(res)
            } else {
                None
            }
        }
        "useradd" | "adduser" => {
            let user = argv[1..].iter().rfind(|a| !a.starts_with('-'))?.clone();
            Some(vec!["userdel".to_string(), user])
        }
        "groupadd" | "addgroup" => {
            let group = argv[1..].iter().rfind(|a| !a.starts_with('-'))?.clone();
            Some(vec!["groupdel".to_string(), group])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_vec(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_classify_apt_get() {
        let action = classify(&to_vec(&["apt-get", "install", "-y", "curl", "jq"]), true);
        assert_eq!(
            action.summary,
            "installs system packages curl, jq via apt-get"
        );
        assert_eq!(action.category, Category::Packages);
        assert!(action.as_root);
    }

    #[test]
    fn test_classify_systemctl_user_enable_now() {
        let action = classify(
            &to_vec(&["systemctl", "--user", "enable", "--now", "herdr.service"]),
            false,
        );
        assert_eq!(
            action.summary,
            "enables and starts user service herdr.service"
        );
        assert_eq!(action.category, Category::Services);
        assert!(!action.as_root);
    }

    #[test]
    fn test_classify_systemctl_start() {
        let action = classify(&to_vec(&["systemctl", "start", "x.service"]), true);
        assert_eq!(action.summary, "starts system service x.service");
        assert_eq!(action.category, Category::Services);
        assert!(action.as_root);
    }

    #[test]
    fn test_classify_useradd() {
        let action = classify(&to_vec(&["useradd", "-r", "herdr"]), true);
        assert_eq!(action.summary, "creates user account herdr");
        assert_eq!(action.category, Category::Accounts);
        assert!(action.as_root);
    }

    #[test]
    fn test_classify_crontab() {
        let action = classify(&to_vec(&["crontab", "-"]), false);
        assert_eq!(action.summary, "replaces your crontab");
        assert_eq!(action.category, Category::Scheduling);
        assert!(!action.as_root);
    }

    #[test]
    fn test_classify_ldconfig() {
        let action = classify(&to_vec(&["ldconfig"]), true);
        assert_eq!(action.summary, "runs ldconfig (changes system state)");
        assert_eq!(action.category, Category::System);
        assert!(action.as_root);
    }

    #[test]
    fn test_classify_unknown_command() {
        let action = classify(&to_vec(&["custom-daemon", "--flag"]), false);
        assert_eq!(action.summary, "runs custom-daemon (changes system state)");
        assert_eq!(action.category, Category::System);
        assert!(!action.as_root);
    }

    #[test]
    fn test_inverse_packages() {
        assert_eq!(
            inverse(&to_vec(&["apt-get", "install", "-y", "curl", "jq"])),
            Some(to_vec(&["apt-get", "remove", "curl", "jq"]))
        );
        assert_eq!(
            inverse(&to_vec(&["apt", "install", "curl", "jq"])),
            Some(to_vec(&["apt", "remove", "curl", "jq"]))
        );
        assert_eq!(
            inverse(&to_vec(&["dnf", "install", "-y", "pkg"])),
            Some(to_vec(&["dnf", "remove", "pkg"]))
        );
        assert_eq!(
            inverse(&to_vec(&["yum", "install", "pkg"])),
            Some(to_vec(&["yum", "remove", "pkg"]))
        );
        assert_eq!(
            inverse(&to_vec(&["zypper", "install", "pkg"])),
            Some(to_vec(&["zypper", "remove", "pkg"]))
        );
        assert_eq!(
            inverse(&to_vec(&["pacman", "-S", "x"])),
            Some(to_vec(&["pacman", "-R", "x"]))
        );
        assert_eq!(
            inverse(&to_vec(&["apk", "add", "x"])),
            Some(to_vec(&["apk", "del", "x"]))
        );
        assert_eq!(
            inverse(&to_vec(&["snap", "install", "x"])),
            Some(to_vec(&["snap", "remove", "x"]))
        );
        assert_eq!(
            inverse(&to_vec(&["flatpak", "install", "r", "x"])),
            Some(to_vec(&["flatpak", "uninstall", "x"]))
        );
    }

    #[test]
    fn test_inverse_services() {
        assert_eq!(
            inverse(&to_vec(&[
                "systemctl",
                "--user",
                "enable",
                "--now",
                "a.service"
            ])),
            Some(to_vec(&[
                "systemctl",
                "--user",
                "disable",
                "--now",
                "a.service"
            ]))
        );
        assert_eq!(
            inverse(&to_vec(&["systemctl", "start", "a.service"])),
            Some(to_vec(&["systemctl", "stop", "a.service"]))
        );
    }

    #[test]
    fn test_inverse_accounts() {
        assert_eq!(
            inverse(&to_vec(&["useradd", "-r", "herdr"])),
            Some(to_vec(&["userdel", "herdr"]))
        );
        assert_eq!(
            inverse(&to_vec(&["groupadd", "g"])),
            Some(to_vec(&["groupdel", "g"]))
        );
    }

    #[test]
    fn test_inverse_none_cases() {
        assert_eq!(inverse(&to_vec(&["crontab", "-"])), None);
        assert_eq!(inverse(&to_vec(&["dpkg", "-i", "x.deb"])), None);
    }

    #[test]
    fn test_shell_quote() {
        assert_eq!(
            shell_quote(&to_vec(&["echo", "a b", "it's"])),
            "echo 'a b' 'it'\\''s'"
        );
        assert_eq!(shell_quote(&to_vec(&[])), "");
        assert_eq!(
            shell_quote(&to_vec(&[
                "curl",
                "-fsSL",
                "https://example.com/install.sh",
                "foo@bar:1.0,+%-/."
            ])),
            "curl -fsSL https://example.com/install.sh foo@bar:1.0,+%-/."
        );
    }

    #[test]
    fn test_display() {
        let action_root = DangerousAction {
            argv: to_vec(&["apt-get", "install", "-y", "curl", "jq"]),
            as_root: true,
            category: Category::Packages,
            summary: "installs system packages curl, jq via apt-get".to_string(),
        };
        assert_eq!(display(&action_root), "sudo apt-get install -y curl jq");

        let action_user = DangerousAction {
            argv: to_vec(&["systemctl", "--user", "enable", "--now", "herdr.service"]),
            as_root: false,
            category: Category::Services,
            summary: "enables and starts user service herdr.service".to_string(),
        };
        assert_eq!(
            display(&action_user),
            "systemctl --user enable --now herdr.service"
        );
    }

    #[test]
    fn test_serde_roundtrip() {
        let action = DangerousAction {
            argv: to_vec(&["apt-get", "install", "-y", "curl"]),
            as_root: true,
            category: Category::Packages,
            summary: "installs system packages curl via apt-get".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: DangerousAction = serde_json::from_str(&json).unwrap();
        assert_eq!(action, deserialized);
    }
}
