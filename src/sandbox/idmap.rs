#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdRange {
    pub inside: u32,
    pub outside: u32,
    pub count: u32,
}

pub fn parse_subid(content: &str, user: &str, id: u32) -> Option<(u32, u32)> {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split(':').map(|s| s.trim()).collect();
        if parts.len() < 3 {
            continue;
        }
        let matches_name = parts[0] == user;
        let matches_id = parts[0].parse::<u32>().map(|v| v == id).unwrap_or(false);
        if (matches_name || matches_id)
            && let (Ok(start), Ok(count)) = (parts[1].parse::<u32>(), parts[2].parse::<u32>())
        {
            return Some((start, count));
        }
    }
    None
}

pub fn compute_map(id: u32, sub_start: u32, sub_count: u32) -> Result<Vec<IdRange>, String> {
    if id == 0 {
        return Err("refusing to run as root: run installctl as a normal user".to_string());
    }

    const MIN_CONTAINER_RANGE: u32 = 65536;

    if id < MIN_CONTAINER_RANGE {
        if sub_count < MIN_CONTAINER_RANGE - 1 {
            return Err(format!(
                "insufficient subid range: need at least 65535 IDs, but available count is {sub_count}"
            ));
        }
        let mut ranges = Vec::new();
        if id > 0 {
            ranges.push(IdRange {
                inside: 0,
                outside: sub_start,
                count: id,
            });
        }
        ranges.push(IdRange {
            inside: id,
            outside: id,
            count: 1,
        });
        let remaining = MIN_CONTAINER_RANGE - 1 - id;
        if remaining > 0 {
            ranges.push(IdRange {
                inside: id + 1,
                outside: sub_start + id,
                count: remaining,
            });
        }
        Ok(ranges)
    } else {
        if sub_count < MIN_CONTAINER_RANGE {
            return Err(format!(
                "insufficient subid range: need at least 65536 IDs (minimum 65535), but available count is {sub_count}"
            ));
        }
        Ok(vec![
            IdRange {
                inside: 0,
                outside: sub_start,
                count: MIN_CONTAINER_RANGE,
            },
            IdRange {
                inside: id,
                outside: id,
                count: 1,
            },
        ])
    }
}

pub fn apply_map(pid: i32, tool: &str, ranges: &[IdRange]) -> anyhow::Result<()> {
    if ranges.is_empty() {
        return Ok(());
    }
    let mut cmd = std::process::Command::new(tool);
    cmd.arg(pid.to_string());
    for r in ranges {
        cmd.arg(r.inside.to_string());
        cmd.arg(r.outside.to_string());
        cmd.arg(r.count.to_string());
    }
    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("failed to execute {tool}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{tool} exited with {}: {stderr}", output.status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_map_normal_user() {
        assert_eq!(
            compute_map(1000, 100000, 65536),
            Ok(vec![
                IdRange { inside: 0, outside: 100000, count: 1000 },
                IdRange { inside: 1000, outside: 1000, count: 1 },
                IdRange { inside: 1001, outside: 101000, count: 64535 },
            ])
        );
    }

    #[test]
    fn test_compute_map_root_rejected() {
        let err = compute_map(0, 100000, 65536).unwrap_err();
        assert!(
            err.contains("run installctl as a normal user"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_compute_map_too_small_subids() {
        let err = compute_map(1000, 100000, 1000).unwrap_err();
        assert!(err.contains("65535"), "unexpected error: {err}");
    }

    #[test]
    fn test_compute_map_high_uid() {
        assert_eq!(
            compute_map(70000, 200000, 65536),
            Ok(vec![
                IdRange { inside: 0, outside: 200000, count: 65536 },
                IdRange { inside: 70000, outside: 70000, count: 1 },
            ])
        );
    }

    #[test]
    fn test_parse_subid() {
        let content = "\
# Subuid configuration file
root:1:2

agent:100000:65536
other:200000:65536
";
        assert_eq!(
            parse_subid(content, "agent", 1000),
            Some((100000, 65536))
        );

        let content_numeric = "\
# comment
1000:5:65536
";
        assert_eq!(
            parse_subid(content_numeric, "agent", 1000),
            Some((5, 65536))
        );

        let content_nomatch = "\
# comment
other:5:65536
";
        assert_eq!(parse_subid(content_nomatch, "agent", 1000), None);
    }

    #[test]
    fn test_apply_map_empty() {
        assert!(apply_map(1234, "true", &[]).is_ok());
    }

    #[test]
    fn test_apply_map_failure() {
        assert!(apply_map(1234, "false", &[IdRange { inside: 0, outside: 0, count: 1 }]).is_err());
    }
}
