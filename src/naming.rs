use crate::changes::{Change, ChangeKind};
use crate::overlay::FileKind;
use crate::prereq::UserInfo;
use crate::prompt::Prompter;
use std::path::PathBuf;

/// Discovers executable and symlink candidate basenames installed into PATH directories.
///
/// Filters for:
/// - Created Files with any execute bit (`mode & 0o111 != 0`)
/// - Created Symlinks
///
/// whose parent directory is in `path_dirs`. Returns basenames, sorted and deduplicated.
pub fn candidates(changes: &[Change], path_dirs: &[PathBuf]) -> Vec<String> {
    let mut names = Vec::new();
    for c in changes {
        let is_exec_file = c.kind == ChangeKind::Created
            && c.entry == FileKind::File
            && (c.mode & 0o111 != 0);
        let is_created_symlink = c.kind == ChangeKind::Created
            && c.entry == FileKind::Symlink;

        if (is_exec_file || is_created_symlink)
            && let Some(parent) = c.path.parent()
            && path_dirs.iter().any(|d| d == parent)
            && let Some(file_name) = c.path.file_name().and_then(|n| n.to_str())
        {
            names.push(file_name.to_string());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Validates program name: ^[A-Za-z0-9][A-Za-z0-9._-]*$
pub fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        }
        _ => false,
    }
}

/// Discovers directories in PATH for the given user:
/// Host `$PATH` entries plus `~/.local/bin` and `/usr/local/bin`, preserving order
/// and deduplicating.
pub fn path_dirs(user: &UserInfo) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    let local_bin = user.home.join(".local/bin");
    if !dirs.contains(&local_bin) {
        dirs.push(local_bin);
    }
    let usr_local_bin = PathBuf::from("/usr/local/bin");
    if !dirs.contains(&usr_local_bin) {
        dirs.push(usr_local_bin);
    }
    dirs
}

fn prompt_valid_name(p: &mut dyn Prompter) -> String {
    loop {
        let input = p.input("Program name:", None);
        if valid_name(&input) {
            return input;
        }
    }
}

/// Selects a program name:
/// - Valid `cli` wins.
/// - Invalid `cli` or no candidates -> `input("Program name:", None)` until valid.
/// - If `cli` is None:
///   - One candidate -> return it.
///   - 2..=9 candidates -> `choose` with digits '1'..'9'.
///   - >9 candidates -> `input` asking to type one of them, validating the response.
pub fn choose_name(
    cli: Option<&str>,
    candidates: &[String],
    p: &mut dyn Prompter,
) -> String {
    if let Some(name) = cli {
        if valid_name(name) {
            return name.to_string();
        }
        return prompt_valid_name(p);
    }

    match candidates.len() {
        0 => prompt_valid_name(p),
        1 => candidates[0].clone(),
        2..=9 => {
            let options: Vec<(char, &str)> = candidates
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let ch = char::from_digit((i + 1) as u32, 10).expect("1..=9 fits in digit");
                    (ch, s.as_str())
                })
                .collect();
            let ch = p.choose("Select program name:", &options);
            let idx = (ch as u32 - '1' as u32) as usize;
            candidates[idx].clone()
        }
        _ => {
            p.say("Multiple candidates found:");
            for c in candidates {
                p.say(&format!("  {c}"));
            }
            loop {
                let input = p.input("Program name:", None);
                if candidates.iter().any(|c| c == &input) {
                    return input;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ScriptedPrompter;
    use std::collections::HashSet;

    fn make_change(
        path: &str,
        kind: ChangeKind,
        entry: FileKind,
        mode: u32,
    ) -> Change {
        Change {
            path: PathBuf::from(path),
            kind,
            entry,
            size: 0,
            mode,
            owner: (1000, 1000),
            blob: None,
            link_target: None,
            origin: None,
        }
    }

    #[test]
    fn test_candidates_filtering_and_dedup() {
        let path_dirs = vec![
            PathBuf::from("/home/tester/.local/bin"),
            PathBuf::from("/usr/local/bin"),
        ];

        let changes = vec![
            // Executable file in ~/.local/bin -> should be picked
            make_change(
                "/home/tester/.local/bin/herdr",
                ChangeKind::Created,
                FileKind::File,
                0o755,
            ),
            // Non-executable file in ~/.local/bin -> ignored
            make_change(
                "/home/tester/.local/bin/herdr.conf",
                ChangeKind::Created,
                FileKind::File,
                0o644,
            ),
            // Executable file in ~/.local/share/herdr/ -> ignored (not in path_dirs)
            make_change(
                "/home/tester/.local/share/herdr/herdr-exec",
                ChangeKind::Created,
                FileKind::File,
                0o755,
            ),
            // Created symlink in /usr/local/bin -> should be picked
            make_change(
                "/usr/local/bin/herdr-link",
                ChangeKind::Created,
                FileKind::Symlink,
                0o777,
            ),
            // Modified file in ~/.local/bin -> ignored (not Created)
            make_change(
                "/home/tester/.local/bin/existing",
                ChangeKind::Modified,
                FileKind::File,
                0o755,
            ),
            // Duplicate basename in another path dir -> should be deduplicated
            make_change(
                "/usr/local/bin/herdr",
                ChangeKind::Created,
                FileKind::File,
                0o755,
            ),
        ];

        let cands = candidates(&changes, &path_dirs);
        assert_eq!(cands, vec!["herdr".to_string(), "herdr-link".to_string()]);
    }

    #[test]
    fn test_valid_name() {
        // Valid names
        assert!(valid_name("herdr"));
        assert!(valid_name("a"));
        assert!(valid_name("A1"));
        assert!(valid_name("my-tool_1.0"));
        assert!(valid_name("installctl"));

        // Invalid names
        assert!(!valid_name("../x"));
        assert!(!valid_name(""));
        assert!(!valid_name("-herdr"));
        assert!(!valid_name(".herdr"));
        assert!(!valid_name("_herdr"));
        assert!(!valid_name("bad/name"));
        assert!(!valid_name("has space"));
        assert!(!valid_name("special$char"));
    }

    #[test]
    fn test_choose_name_valid_cli_wins() {
        let mut p = ScriptedPrompter::new(vec![]);
        let chosen = choose_name(Some("my-app"), &["cand1".into(), "cand2".into()], &mut p);
        assert_eq!(chosen, "my-app");
    }

    #[test]
    fn test_choose_name_single_candidate() {
        let mut p = ScriptedPrompter::new(vec![]);
        let chosen = choose_name(None, &["herdr".into()], &mut p);
        assert_eq!(chosen, "herdr");
    }

    #[test]
    fn test_choose_name_choose_digits() {
        let mut p = ScriptedPrompter::new(vec!["2".into()]);
        let chosen = choose_name(None, &["a".into(), "b".into()], &mut p);
        assert_eq!(chosen, "b");

        let mut p = ScriptedPrompter::new(vec!["1".into()]);
        let chosen = choose_name(None, &["a".into(), "b".into()], &mut p);
        assert_eq!(chosen, "a");
    }

    #[test]
    fn test_choose_name_invalid_cli_prompts_input() {
        // Empty candidates
        let mut p = ScriptedPrompter::new(vec!["good".into()]);
        let chosen = choose_name(Some("bad/name"), &[], &mut p);
        assert_eq!(chosen, "good");

        // Candidates exist, but invalid cli argument must prompt for input rather than picking candidate
        let mut p = ScriptedPrompter::new(vec!["good".into()]);
        let chosen = choose_name(Some("bad/name"), &["cand".into()], &mut p);
        assert_eq!(chosen, "good");

        // Re-prompts until valid when invalid input is provided
        let mut p = ScriptedPrompter::new(vec!["still/bad".into(), "".into(), "valid-tool".into()]);
        let chosen = choose_name(Some("bad/name"), &["cand1".into(), "cand2".into()], &mut p);
        assert_eq!(chosen, "valid-tool");
    }

    #[test]
    fn test_choose_name_empty_candidates_prompt_loop() {
        let mut p = ScriptedPrompter::new(vec!["invalid/name".into(), "".into(), "valid-tool".into()]);
        let chosen = choose_name(None, &[], &mut p);
        assert_eq!(chosen, "valid-tool");
    }

    #[test]
    fn test_choose_name_more_than_nine_candidates() {
        let candidates: Vec<String> = (1..=12).map(|i| format!("tool{i:02}")).collect();
        let mut p = ScriptedPrompter::new(vec!["wrong-name".into(), "tool10".into()]);
        let chosen = choose_name(None, &candidates, &mut p);
        assert_eq!(chosen, "tool10");
    }

    #[test]
    fn test_path_dirs() {
        let user = UserInfo {
            name: "tester".into(),
            uid: 1000,
            gid: 1000,
            home: PathBuf::from("/home/tester"),
        };
        let dirs = path_dirs(&user);

        assert!(dirs.contains(&PathBuf::from("/home/tester/.local/bin")));
        assert!(dirs.contains(&PathBuf::from("/usr/local/bin")));

        // Deduplication check
        let mut seen = HashSet::new();
        for dir in &dirs {
            assert!(seen.insert(dir), "duplicate entry found in path_dirs: {dir:?}");
        }
    }
}
