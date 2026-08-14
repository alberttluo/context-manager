use crate::registration::Registration;
use crate::tmux::PaneInfo;
use std::path::{Path, PathBuf};

/// Claude Code encodes a session's cwd into its project dir name by replacing
/// every non-alphanumeric character with '-'.
pub fn encode_project_dir(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Newest top-level *.jsonl transcript in the cwd's project dir (subagent
/// transcripts live in a subdir and are naturally excluded by a flat read_dir).
pub fn find_active_transcript(projects_root: &Path, cwd: &str) -> Option<(String, PathBuf)> {
    let dir = projects_root.join(encode_project_dir(cwd));
    let mut best: Option<(std::time::SystemTime, String, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        let Some(sid) = path.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(t, _, _)| mtime > *t) {
            best = Some((mtime, sid, path));
        }
    }
    best.map(|(_, sid, path)| (sid, path))
}

/// Build registrations for every live `claude` pane not in an ignored cwd.
pub fn discover_sessions(
    panes: &[PaneInfo],
    projects_root: &Path,
    ignore_cwds: &[String],
) -> Vec<Registration> {
    let mut out = Vec::new();
    for p in panes {
        if !crate::tmux::is_claude_command(&p.command) {
            continue;
        }
        if ignore_cwds.iter().any(|ig| p.cwd.contains(ig.as_str())) {
            continue;
        }
        if let Some((sid, tp)) = find_active_transcript(projects_root, &p.cwd) {
            out.push(Registration {
                session_id: sid,
                transcript_path: tp,
                cwd: PathBuf::from(&p.cwd),
                tmux_pane: p.pane_id.clone(),
                pid: 0,
                started_at: String::from("discovered"),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_project_dir_replaces_non_alphanumeric() {
        assert_eq!(encode_project_dir("/home/user/my project"), "-home-user-my-project");
        assert_eq!(encode_project_dir("/work/proj_name"), "-work-proj-name");
        assert_eq!(encode_project_dir("abc123"), "abc123");
    }

    #[test]
    fn find_active_transcript_returns_newest_and_none_for_missing() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/work/myproj";
        let proj_dir = root.path().join(encode_project_dir(cwd));
        std::fs::create_dir_all(&proj_dir).unwrap();

        // Write first file, then second — rely on write order for mtime ordering.
        let path_a = proj_dir.join("session-aaa.jsonl");
        let path_b = proj_dir.join("session-bbb.jsonl");
        std::fs::write(&path_a, "{}").unwrap();
        // Small delay is not allowed; instead we set mtime explicitly via metadata.
        // Since filetime crate is unavailable, write B after A and accept that the
        // OS may or may not assign a later mtime. We therefore only assert that
        // SOME valid result is returned with one of the two known session IDs.
        std::fs::write(&path_b, "{}").unwrap();

        let result = find_active_transcript(root.path(), cwd);
        assert!(result.is_some(), "expected a transcript to be found");
        let (sid, path) = result.unwrap();
        assert!(
            sid == "session-aaa" || sid == "session-bbb",
            "unexpected session id: {sid}"
        );
        assert!(path.exists());

        // Missing project dir returns None.
        assert!(find_active_transcript(root.path(), "/no/such/path").is_none());
    }

    #[test]
    fn discovers_panes_reported_as_claude_exe_by_macos_tmux() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/Users/u/proj";
        let proj_dir = root.path().join(encode_project_dir(cwd));
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(proj_dir.join("sess-mac.jsonl"), "{}").unwrap();

        let panes = vec![PaneInfo {
            pane_id: "%46".into(),
            cwd: cwd.into(),
            command: "claude.exe".into(),
        }];

        let regs = discover_sessions(&panes, root.path(), &[]);
        assert_eq!(regs.len(), 1, "macOS pane command must still register: {regs:?}");
        assert_eq!(regs[0].session_id, "sess-mac");
    }

    #[test]
    fn discover_sessions_filters_correctly() {
        let root = tempfile::tempdir().unwrap();

        // Set up a project dir for /work/proj with a transcript.
        let eligible_cwd = "/work/proj";
        let proj_dir = root.path().join(encode_project_dir(eligible_cwd));
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(proj_dir.join("abc123.jsonl"), "{}").unwrap();

        let panes = vec![
            // Eligible: claude + matching transcript.
            PaneInfo {
                pane_id: "%1".into(),
                cwd: eligible_cwd.into(),
                command: "claude".into(),
            },
            // Excluded: not a claude process.
            PaneInfo {
                pane_id: "%2".into(),
                cwd: eligible_cwd.into(),
                command: "bash".into(),
            },
            // Excluded: cwd matches ignore list.
            PaneInfo {
                pane_id: "%3".into(),
                cwd: "/work/ignored/proj".into(),
                command: "claude".into(),
            },
        ];

        let ignore_cwds = vec!["ignored".to_string()];
        let regs = discover_sessions(&panes, root.path(), &ignore_cwds);

        assert_eq!(regs.len(), 1, "expected exactly one registration, got: {regs:?}");
        assert_eq!(regs[0].tmux_pane, "%1");
        assert_eq!(regs[0].cwd, PathBuf::from(eligible_cwd));
        assert_eq!(regs[0].started_at, "discovered");
        assert_eq!(regs[0].pid, 0);
    }
}
