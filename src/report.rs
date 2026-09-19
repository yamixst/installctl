use std::collections::BTreeSet;
use std::path::Path;

use crate::actions;
use crate::changes::{Change, ChangeKind};
use crate::control::{ActionRecord, Decision};
use crate::ipc::Leftover;
use crate::prompt::Prompter;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Review {
    ApplyAll,
    ApplySubset(Vec<usize>),
    Reject,
}

pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: &[&str] = &["KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut val = bytes as f64 / 1024.0;
    let mut idx = 0;
    while val >= 1024.0 && idx < UNITS.len() - 1 {
        val /= 1024.0;
        idx += 1;
    }
    format!("{val:.1} {}", UNITS[idx])
}

pub fn parse_ranges(s: &str, max: usize) -> Result<BTreeSet<usize>, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut res = BTreeSet::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("empty range item".to_string());
        }
        if let Some((start_str, end_str)) = part.split_once('-') {
            let start_str = start_str.trim();
            let end_str = end_str.trim();
            let start: usize = start_str
                .parse()
                .map_err(|_| format!("invalid range start: '{start_str}'"))?;
            let end: usize = end_str
                .parse()
                .map_err(|_| format!("invalid range end: '{end_str}'"))?;
            if start == 0 || end == 0 {
                return Err("range numbers must be >= 1".to_string());
            }
            if start > end {
                return Err(format!("range start {start} > end {end}"));
            }
            if end > max {
                return Err(format!("number {end} exceeds max {max}"));
            }
            for i in start..=end {
                res.insert(i);
            }
        } else {
            let val: usize = part
                .parse()
                .map_err(|_| format!("invalid number: '{part}'"))?;
            if val == 0 {
                return Err("number must be >= 1".to_string());
            }
            if val > max {
                return Err(format!("number {val} exceeds max {max}"));
            }
            res.insert(val);
        }
    }
    Ok(res)
}

pub fn diff_text(old: &[u8], new: &[u8], path: &Path) -> Option<String> {
    if old.contains(&0) || new.contains(&0) {
        return None;
    }
    let old_str = std::str::from_utf8(old).ok()?;
    let new_str = std::str::from_utf8(new).ok()?;

    let path_rel = path.strip_prefix("/").unwrap_or(path);
    let old_label = format!("a/{}", path_rel.display());
    let new_label = format!("b/{}", path_rel.display());

    let diff = similar::TextDiff::from_lines(old_str, new_str)
        .unified_diff()
        .header(&old_label, &new_label)
        .to_string();

    Some(diff)
}

fn owner_note(owner: (u32, u32), current_uid: u32) -> Option<String> {
    if owner.0 == 0 {
        Some("root".to_string())
    } else if owner.0 != current_uid {
        Some(format!("{}:{}", owner.0, owner.1))
    } else {
        None
    }
}

fn line_diff_summary(old: &[u8], new: &[u8]) -> Option<String> {
    if old.contains(&0) || new.contains(&0) {
        return None;
    }
    let old_str = std::str::from_utf8(old).ok()?;
    let new_str = std::str::from_utf8(new).ok()?;
    let diff = similar::TextDiff::from_lines(old_str, new_str);
    let mut added = 0;
    let mut deleted = 0;
    for op in diff.ops() {
        match op.tag() {
            similar::DiffTag::Insert => added += op.new_range().len(),
            similar::DiffTag::Delete => deleted += op.old_range().len(),
            similar::DiffTag::Replace => {
                deleted += op.old_range().len();
                added += op.new_range().len();
            }
            similar::DiffTag::Equal => {}
        }
    }
    if added == 0 && deleted == 0 {
        return None;
    }
    let s = match (added, deleted) {
        (1, 0) => "+1 line".to_string(),
        (a, 0) => format!("+{a} lines"),
        (0, 1) => "-1 line".to_string(),
        (0, d) => format!("-{d} lines"),
        (a, d) => format!("+{a}, -{d} lines"),
    };
    Some(s)
}

fn format_parens(change: &Change, current_uid: u32) -> String {
    let note = owner_note(change.owner, current_uid);
    let info = if change.kind == ChangeKind::Modified {
        let delta = (|| {
            let blob_path = change.blob.as_ref()?;
            let new_bytes = std::fs::read(blob_path).ok()?;
            let old_bytes = std::fs::read(&change.path).ok()?;
            line_diff_summary(&old_bytes, &new_bytes)
        })();
        delta.unwrap_or_else(|| human_size(change.size))
    } else {
        human_size(change.size)
    };

    if let Some(n) = note {
        format!("({info}, {n})")
    } else {
        format!("({info})")
    }
}

pub fn render(
    changes: &[Change],
    actions: &[ActionRecord],
    leftovers: &[Leftover],
    exit_code: i32,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Installer exited with code {exit_code}.\n"));
    if exit_code != 0 {
        out.push_str("Warning: the installer failed; the installation is probably incomplete.\n");
    }
    out.push('\n');

    let current_uid = nix::unistd::getuid().as_raw();

    let mut created_indices = Vec::new();
    let mut modified_indices = Vec::new();
    let mut deleted_indices = Vec::new();

    for (idx, c) in changes.iter().enumerate() {
        match c.kind {
            ChangeKind::Created => created_indices.push(idx),
            ChangeKind::Modified => modified_indices.push(idx),
            ChangeKind::Deleted => deleted_indices.push(idx),
        }
    }

    let mut item_num = 1;

    out.push_str(&format!("Created ({}):\n", created_indices.len()));
    for idx in created_indices {
        let c = &changes[idx];
        let parens = format_parens(c, current_uid);
        out.push_str(&format!(
            "  [{item_num}] + {}  {parens}\n",
            c.path.display()
        ));
        item_num += 1;
    }

    out.push_str(&format!("Modified ({}):\n", modified_indices.len()));
    for idx in modified_indices {
        let c = &changes[idx];
        let parens = format_parens(c, current_uid);
        out.push_str(&format!(
            "  [{item_num}] ~ {}  {parens}\n",
            c.path.display()
        ));
        item_num += 1;
    }

    out.push_str(&format!("Deleted ({}):\n", deleted_indices.len()));
    for idx in deleted_indices {
        let c = &changes[idx];
        let parens = format_parens(c, current_uid);
        out.push_str(&format!(
            "  [{item_num}] - {}  {parens}\n",
            c.path.display()
        ));
        item_num += 1;
    }

    if !actions.is_empty() {
        out.push_str(&format!("Dangerous actions ({}):\n", actions.len()));
        for record in actions {
            let disp = actions::display(&record.action);
            let summary = &record.action.summary;
            match record.decision {
                Decision::Skip => {
                    out.push_str(&format!("  ! skipped: {disp} \u{2014} {summary}\n"));
                }
                Decision::RunForReal => {
                    let exit_str = record
                        .exit_code
                        .map(|code| format!("exit {code}"))
                        .unwrap_or_else(|| "exit ?".to_string());
                    out.push_str(&format!(
                        "  ! ran ({exit_str}): {disp} \u{2014} {summary}\n"
                    ));
                }
                Decision::Abort => {
                    out.push_str(&format!("  ! aborted: {disp} \u{2014} {summary}\n"));
                }
            }
        }
    }

    if !leftovers.is_empty() {
        out.push_str(&format!(
            "Background processes left by the installer ({}), now stopped:\n",
            leftovers.len()
        ));
        for leftover in leftovers {
            out.push_str(&format!("  ! {}\n", leftover.cmdline));
        }
    }

    out
}

fn change_report_order(changes: &[Change]) -> Vec<usize> {
    let mut order = Vec::with_capacity(changes.len());
    for (i, c) in changes.iter().enumerate() {
        if c.kind == ChangeKind::Created {
            order.push(i);
        }
    }
    for (i, c) in changes.iter().enumerate() {
        if c.kind == ChangeKind::Modified {
            order.push(i);
        }
    }
    for (i, c) in changes.iter().enumerate() {
        if c.kind == ChangeKind::Deleted {
            order.push(i);
        }
    }
    order
}

pub fn review(changes: &[Change], lower_root: &Path, p: &mut dyn Prompter) -> Review {
    const MENU_OPTIONS: &[(char, &str)] = &[
        ('a', "apply all"),
        ('s', "select what to apply"),
        ('d', "show diffs of modified text files"),
        ('r', "reject, change nothing"),
    ];

    loop {
        let choice = p.choose("", MENU_OPTIONS);
        match choice {
            'a' => return Review::ApplyAll,
            'r' => return Review::Reject,
            'd' => {
                let mut found_any = false;
                for c in changes {
                    if c.kind != ChangeKind::Modified {
                        continue;
                    }
                    let lower_path = lower_root.join(c.path.strip_prefix("/").unwrap_or(&c.path));
                    let old_bytes = std::fs::read(&lower_path)
                        .or_else(|_| std::fs::read(&c.path))
                        .ok();
                    let new_bytes = c.blob.as_ref().and_then(|b| std::fs::read(b).ok());

                    if let (Some(old), Some(new)) = (old_bytes, new_bytes)
                        && let Some(diff) = diff_text(&old, &new, &c.path)
                    {
                        found_any = true;
                        if diff.is_empty() {
                            p.say(&format!("--- a/{} (no content changes)", c.path.display()));
                        } else {
                            p.say(diff.trim_end());
                        }
                    }
                }
                if !found_any {
                    p.say("No modified text files.");
                }
            }
            's' => {
                let order = change_report_order(changes);
                let excluded_report_nums = loop {
                    let input_str = p.input("Numbers to EXCLUDE (e.g. 2,5-7):", None);
                    match parse_ranges(&input_str, changes.len()) {
                        Ok(set) => break set,
                        Err(e) => {
                            p.say(&format!("Invalid selection: {e}"));
                        }
                    }
                };

                p.say("Excluded changes may leave the program broken.");

                let mut remaining = Vec::new();
                for (idx_1based, &orig_idx) in order.iter().enumerate() {
                    let report_num = idx_1based + 1;
                    if !excluded_report_nums.contains(&report_num) {
                        remaining.push(orig_idx);
                    }
                }
                remaining.sort_unstable();

                let question = format!("Apply the remaining {} changes?", remaining.len());
                let confirm = p.choose(&question, &[('y', "yes"), ('n', "no")]);
                if confirm == 'y' {
                    return Review::ApplySubset(remaining);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{Category, DangerousAction};
    use crate::overlay::FileKind;
    use crate::prompt::ScriptedPrompter;
    use std::path::PathBuf;

    #[test]
    fn test_human_size() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(3_565_158), "3.4 MiB");
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1024), "1.0 KiB");
    }

    #[test]
    fn test_parse_ranges() {
        assert_eq!(parse_ranges("2,5-7", 9), Ok(BTreeSet::from([2, 5, 6, 7])));
        assert!(parse_ranges("0", 9).is_err());
        assert!(parse_ranges("3-1", 9).is_err());
        assert!(parse_ranges("10", 9).is_err());
        assert!(parse_ranges("invalid", 9).is_err());
        assert_eq!(parse_ranges("", 9), Ok(BTreeSet::new()));
    }

    #[test]
    fn test_diff_text() {
        let diff = diff_text(b"a\n", b"a\nb\n", Path::new("file.txt")).expect("valid diff");
        assert!(diff.contains("+b"), "diff was: {diff}");
        assert_eq!(diff_text(b"\0", b"a\nb\n", Path::new("file.txt")), None);
        assert_eq!(diff_text(b"a\n", b"\0", Path::new("file.txt")), None);
        assert_eq!(diff_text(b"\xff\xfe", b"a\n", Path::new("file.txt")), None);
    }

    #[test]
    fn test_render_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let lower = tmp.path().join("lower");
        let upper = tmp.path().join("upper");
        std::fs::create_dir_all(&lower).unwrap();
        std::fs::create_dir_all(&upper).unwrap();

        // 1. Created helper file
        let helper_path = PathBuf::from("/usr/local/bin/helper");
        let helper_blob = upper.join("helper_blob");
        std::fs::write(&helper_blob, vec![0u8; 120]).unwrap();
        let c_created = Change {
            path: helper_path.clone(),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 120,
            mode: 0o755,
            owner: (0, 0), // root
            blob: Some(helper_blob),
            link_target: None,
            origin: None,
        };

        // 2. Modified bashrc file
        let bashrc_path = lower.join(".bashrc");
        std::fs::write(&bashrc_path, b"# bashrc\n").unwrap();
        let bashrc_blob = upper.join("bashrc_blob");
        std::fs::write(&bashrc_blob, b"# bashrc\nexport PATH=/foo:$PATH\n").unwrap();
        let c_modified = Change {
            path: bashrc_path.clone(),
            kind: ChangeKind::Modified,
            entry: FileKind::File,
            size: 32,
            mode: 0o644,
            owner: (
                nix::unistd::getuid().as_raw(),
                nix::unistd::getgid().as_raw(),
            ),
            blob: Some(bashrc_blob),
            link_target: None,
            origin: None,
        };

        // 3. Deleted old.conf file
        let old_conf_path = lower.join("old.conf");
        std::fs::write(&old_conf_path, b"content").unwrap();
        let c_deleted = Change {
            path: old_conf_path.clone(),
            kind: ChangeKind::Deleted,
            entry: FileKind::File,
            size: 7,
            mode: 0o644,
            owner: (
                nix::unistd::getuid().as_raw(),
                nix::unistd::getgid().as_raw(),
            ),
            blob: None,
            link_target: None,
            origin: None,
        };

        let changes = vec![c_created, c_modified, c_deleted];

        let actions = vec![
            ActionRecord {
                action: DangerousAction {
                    argv: vec![
                        "apt-get".into(),
                        "install".into(),
                        "-y".into(),
                        "curl".into(),
                    ],
                    as_root: true,
                    category: Category::Packages,
                    summary: "installs system packages curl via apt-get".into(),
                },
                decision: Decision::Skip,
                exit_code: None,
            },
            ActionRecord {
                action: DangerousAction {
                    argv: vec![
                        "systemctl".into(),
                        "--user".into(),
                        "enable".into(),
                        "x.service".into(),
                    ],
                    as_root: false,
                    category: Category::Services,
                    summary: "enables user service x.service".into(),
                },
                decision: Decision::RunForReal,
                exit_code: Some(0),
            },
        ];

        let leftovers = vec![Leftover {
            pid: 1234,
            cmdline: "sleep 300".into(),
        }];

        let report = render(&changes, &actions, &leftovers, 0);

        assert!(report.contains("Installer exited with code 0."));
        assert!(report.contains("Created (1):"));
        assert!(report.contains("  [1] + /usr/local/bin/helper  (120 B, root)"));
        assert!(report.contains("Modified (1):"));
        assert!(report.contains(&format!("  [2] ~ {}  (+1 line)", bashrc_path.display())));
        assert!(report.contains("Deleted (1):"));
        assert!(report.contains(&format!("  [3] - {}  (7 B)", old_conf_path.display())));
        assert!(report.contains("Dangerous actions (2):"));
        assert!(report.contains("  ! skipped: sudo apt-get install -y curl \u{2014} installs system packages curl via apt-get"));
        assert!(report.contains("  ! ran (exit 0): systemctl --user enable x.service \u{2014} enables user service x.service"));
        assert!(report.contains("Background processes left by the installer (1), now stopped:"));
        assert!(report.contains("  ! sleep 300"));

        // Non-zero exit code warning
        let failed_report = render(&[], &[], &[], 1);
        assert!(failed_report.contains("Installer exited with code 1."));
        assert!(
            failed_report.contains(
                "Warning: the installer failed; the installation is probably incomplete."
            )
        );
    }

    #[test]
    fn test_review_apply_subset_and_reject() {
        let dummy_change = |name: &str| Change {
            path: PathBuf::from(format!("/home/u/{name}")),
            kind: ChangeKind::Created,
            entry: FileKind::File,
            size: 100,
            mode: 0o644,
            owner: (1000, 1000),
            blob: None,
            link_target: None,
            origin: None,
        };
        let changes = vec![
            dummy_change("file1"),
            dummy_change("file2"),
            dummy_change("file3"),
        ];

        // 1. Scripted 's', '2', 'y' -> ApplySubset(vec![0, 2])
        let mut p_subset = ScriptedPrompter::new(vec!["s".into(), "2".into(), "y".into()]);
        let res_subset = review(&changes, Path::new("/dummy/lower"), &mut p_subset);
        assert_eq!(res_subset, Review::ApplySubset(vec![0, 2]));

        // 2. Scripted 'r' -> Reject
        let mut p_reject = ScriptedPrompter::new(vec!["r".into()]);
        let res_reject = review(&changes, Path::new("/dummy/lower"), &mut p_reject);
        assert_eq!(res_reject, Review::Reject);

        // 3. Scripted 'a' -> ApplyAll
        let mut p_all = ScriptedPrompter::new(vec!["a".into()]);
        let res_all = review(&changes, Path::new("/dummy/lower"), &mut p_all);
        assert_eq!(res_all, Review::ApplyAll);

        // 4. Scripted 's', '2', 'n' (cancel subset and return to menu), 'a' -> ApplyAll
        let mut p_back =
            ScriptedPrompter::new(vec!["s".into(), "2".into(), "n".into(), "a".into()]);
        let res_back = review(&changes, Path::new("/dummy/lower"), &mut p_back);
        assert_eq!(res_back, Review::ApplyAll);
    }

    #[test]
    fn test_review_show_diffs() {
        let tmp = tempfile::tempdir().unwrap();
        let lower = tmp.path().join("lower");
        let upper = tmp.path().join("upper");
        std::fs::create_dir_all(&lower).unwrap();
        std::fs::create_dir_all(&upper).unwrap();

        let bashrc_rel = Path::new("home/u/.bashrc");
        let bashrc_lower = lower.join(bashrc_rel);
        std::fs::create_dir_all(bashrc_lower.parent().unwrap()).unwrap();
        std::fs::write(&bashrc_lower, b"line1\nline2\n").unwrap();

        let bashrc_blob = upper.join("bashrc_blob");
        std::fs::write(&bashrc_blob, b"line1\nline2\nline3\n").unwrap();

        let change = Change {
            path: PathBuf::from("/home/u/.bashrc"),
            kind: ChangeKind::Modified,
            entry: FileKind::File,
            size: 18,
            mode: 0o644,
            owner: (1000, 1000),
            blob: Some(bashrc_blob),
            link_target: None,
            origin: None,
        };

        // Scripted 'd' then 'a'
        let mut p = ScriptedPrompter::new(vec!["d".into(), "a".into()]);
        let res = review(&[change], &lower, &mut p);
        assert_eq!(res, Review::ApplyAll);
    }
}
