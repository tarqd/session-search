//! Locate transcript roots and enumerate the JSONL files under them.
//!
//! Two shapes are collected, per `docs/TRANSCRIPT-FORMAT.md` §1:
//!
//! ```text
//! <root>/<projectKey>/<sessionId>.jsonl                       main transcript
//! <root>/<projectKey>/<sessionId>/subagents/**/agent-<id>.jsonl   sidechain (arbitrarily nested)
//! ```
//!
//! `<projectKey>` is **lossy and is never decoded** — the project comes from each record's
//! `cwd` field, which is `parse.rs`'s job. The only thing taken from the path here is the
//! session id and the agent id.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::model::{Extra, flex_string, flex_u64};

/// `agent-<id>.meta.json`, alongside a sidechain transcript.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentMeta {
    #[serde(deserialize_with = "flex_string")]
    pub agent_type: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub description: Option<String>,
    /// Joins back to the parent session's `tool_use.id`.
    #[serde(deserialize_with = "flex_string")]
    pub tool_use_id: Option<String>,
    #[serde(deserialize_with = "flex_u64")]
    pub spawn_depth: Option<u64>,
    #[serde(deserialize_with = "flex_string")]
    pub request_shape: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl AgentMeta {
    /// Read a `.meta.json`, tolerating anything that is not a usable object.
    pub fn read(path: &Path) -> Option<AgentMeta> {
        let bytes = std::fs::read(path).ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        AgentMeta::deserialize(&value).ok()
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptFile {
    pub path: PathBuf,
    /// From the filename / parent dir. Records may disagree; records win downstream.
    pub session_id: String,
    /// `Some(..)` for `subagents/**/agent-<id>.jsonl`.
    pub agent_id: Option<String>,
    pub meta: Option<AgentMeta>,
    pub size: u64,
    pub mtime_ms: i64,
}

impl TranscriptFile {
    /// The `sessions.json` key: `"<session_id>"` or `"<session_id>:<agent_id>"`.
    pub fn key(&self) -> String {
        match &self.agent_id {
            Some(a) => format!("{}:{}", self.session_id, a),
            None => self.session_id.clone(),
        }
    }
}

/// `$CLAUDE_CONFIG_DIR` else `~/.claude`, plus `/projects`.
pub fn default_root() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME")
                .context("neither $CLAUDE_CONFIG_DIR nor $HOME is set; pass --root explicitly")?;
            PathBuf::from(home).join(".claude")
        }
    };
    Ok(base.join("projects"))
}

/// Enumerate every transcript under `roots`. Missing roots are skipped, not an error —
/// the CLI prunes old transcripts and roots come and go.
pub fn discover(roots: &[PathBuf]) -> anyhow::Result<Vec<TranscriptFile>> {
    // Keyed by path so overlapping roots do not yield duplicates.
    let mut found: BTreeMap<PathBuf, TranscriptFile> = BTreeMap::new();

    for root in roots {
        if !root.is_dir() {
            tracing::debug!(root = %root.display(), "transcript root missing, skipping");
            continue;
        }
        for entry in WalkDir::new(root).follow_links(false).into_iter() {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    tracing::debug!(%err, "skipping unreadable path");
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(ids) = classify(root, path) else {
                continue;
            };
            let meta_data = entry.metadata().ok();
            let size = meta_data.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime_ms = meta_data.as_ref().map(mtime_ms).unwrap_or(0);
            let meta = ids
                .agent_id
                .as_ref()
                .and_then(|_| AgentMeta::read(&path.with_extension("meta.json")));

            found.insert(
                path.to_path_buf(),
                TranscriptFile {
                    path: path.to_path_buf(),
                    session_id: ids.session_id,
                    agent_id: ids.agent_id,
                    meta,
                    size,
                    mtime_ms,
                },
            );
        }
    }

    // Main transcript before its subagents, so a consumer walking the list sees the parent
    // session first.
    let mut files: Vec<TranscriptFile> = found.into_values().collect();
    files.sort_by(|a, b| {
        (&a.session_id, &a.agent_id, &a.path).cmp(&(&b.session_id, &b.agent_id, &b.path))
    });
    Ok(files)
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct Ids {
    session_id: String,
    agent_id: Option<String>,
}

/// Decide whether `path` is a transcript, and what session / agent it belongs to.
///
/// Accepted, relative to `root`:
/// * `<projectKey>/<sessionId>.jsonl`
/// * `<projectKey>/<sessionId>/…/subagents/…/agent-<agentId>.jsonl`
///
/// Everything else under a session directory (`journal.jsonl`, `tool-results/*`, …) is
/// deliberately ignored.
fn classify(root: &Path, path: &Path) -> Option<Ids> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    // parts[0] is the lossy <projectKey>; never decoded.
    if parts.len() < 2 {
        return None;
    }
    let file = parts[parts.len() - 1];
    let stem = file.strip_suffix(".jsonl")?;

    if parts.len() == 2 {
        return Some(Ids {
            session_id: stem.to_string(),
            agent_id: None,
        });
    }

    let agent_id = stem.strip_prefix("agent-")?;
    // The component right before the first `subagents` is the parent session id.
    let sub = parts.iter().position(|p| *p == "subagents")?;
    if sub < 2 {
        return None;
    }
    Some(Ids {
        session_id: parts[sub - 1].to_string(),
        agent_id: Some(agent_id.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn classify_recognises_both_layouts() {
        let root = Path::new("/r/projects");
        let main = classify(root, Path::new("/r/projects/-home-user-x/sess-1.jsonl")).unwrap();
        assert_eq!(main.session_id, "sess-1");
        assert!(main.agent_id.is_none());

        let side = classify(
            root,
            Path::new("/r/projects/-home-user-x/sess-1/subagents/agent-abc.jsonl"),
        )
        .unwrap();
        assert_eq!(side.session_id, "sess-1");
        assert_eq!(side.agent_id.as_deref(), Some("abc"));

        let nested = classify(
            root,
            Path::new("/r/projects/-p/sess-2/subagents/workflows/wf_1/agent-def.jsonl"),
        )
        .unwrap();
        assert_eq!(nested.session_id, "sess-2");
        assert_eq!(nested.agent_id.as_deref(), Some("def"));
    }

    #[test]
    fn classify_rejects_non_transcripts() {
        let root = Path::new("/r/projects");
        for p in [
            "/r/projects/-p/sess/subagents/workflows/wf_1/journal.jsonl",
            "/r/projects/-p/sess/tool-results/x.jsonl",
            "/r/projects/loose.jsonl",
        ] {
            assert!(classify(root, Path::new(p)).is_none(), "{p}");
        }
    }

    #[test]
    fn discover_finds_main_and_nested_sidechains_with_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        touch(&root.join("-home-user-x/sess-1.jsonl"), "{}\n");
        touch(
            &root.join("-home-user-x/sess-1/subagents/agent-a1.jsonl"),
            "{}\n",
        );
        touch(
            &root.join("-home-user-x/sess-1/subagents/agent-a1.meta.json"),
            r#"{"agentType":"Explore","description":"look around","spawnDepth":1}"#,
        );
        touch(
            &root.join("-home-user-x/sess-1/subagents/workflows/wf/agent-a2.jsonl"),
            "{}\n",
        );
        touch(
            &root.join("-home-user-x/sess-1/subagents/workflows/wf/journal.jsonl"),
            "{}\n",
        );
        touch(&root.join("-home-user-x/sess-1/ccr-tip.json"), "{}");

        let files = discover(&[root]).unwrap();
        let names: Vec<String> = files.iter().map(|f| f.key()).collect();
        assert_eq!(names, vec!["sess-1", "sess-1:a1", "sess-1:a2"]);

        let a1 = files.iter().find(|f| f.key() == "sess-1:a1").unwrap();
        assert_eq!(
            a1.meta.as_ref().unwrap().agent_type.as_deref(),
            Some("Explore")
        );
        assert_eq!(
            a1.meta.as_ref().unwrap().description.as_deref(),
            Some("look around")
        );
        let a2 = files.iter().find(|f| f.key() == "sess-1:a2").unwrap();
        assert!(a2.meta.is_none());
        assert!(files.iter().all(|f| f.size > 0));
    }

    #[test]
    fn discover_skips_missing_roots() {
        let files = discover(&[PathBuf::from("/definitely/not/here")]).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn default_root_respects_claude_config_dir() {
        // SAFETY: single-threaded assertions on process env inside one test.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/cfg");
        }
        let root = default_root().unwrap();
        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }
        assert_eq!(root, PathBuf::from("/tmp/cfg/projects"));
    }

    #[test]
    fn agent_meta_tolerates_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("m.json");
        std::fs::write(&p, "not json").unwrap();
        assert!(AgentMeta::read(&p).is_none());
        std::fs::write(&p, r#"{"agentType":7,"unknownKey":[1]}"#).unwrap();
        let m = AgentMeta::read(&p).unwrap();
        assert_eq!(m.agent_type.as_deref(), Some("7"));
        assert!(m.extra.contains_key("unknownKey"));
    }
}
