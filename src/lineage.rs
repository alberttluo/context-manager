use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineageRecord {
    pub ts: String,
    pub from_session: String,
    pub to_pane: String,
    pub handoff_path: String,
    pub context_pct: f64,
    pub dry_run: bool,
}

pub fn append(log_path: &Path, rec: &LineageRecord) -> anyhow::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(rec)?;
    let mut f = OpenOptions::new().create(true).append(true).open(log_path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_one_json_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("lineage.jsonl");
        let rec = LineageRecord {
            ts: "2026-06-15T12:00:00Z".into(),
            from_session: "sess-1".into(),
            to_pane: "%3".into(),
            handoff_path: "/tmp/h.md".into(),
            context_pct: 0.52,
            dry_run: false,
        };
        append(&log, &rec).unwrap();
        append(&log, &rec).unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 2);
        // each line must parse back as a record
        let first: LineageRecord = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first.from_session, "sess-1");
        assert_eq!(first.context_pct, 0.52);
    }
}
