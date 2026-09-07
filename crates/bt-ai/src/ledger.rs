//! AI audit ledger: append-only JSONL, one record per gateway invocation.
//! Contains hashes, model/params, usage and timing — NEVER prompts, response
//! bodies, or API keys. The ledger is what makes AI-derived artifacts
//! auditable end to end.

use bt_core::error::{CoreError, CoreResult};
use serde::Serialize;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct LedgerEntry {
    pub id: u64,
    pub ts: String,
    pub purpose: String,
    pub provider: String,
    pub model: String,
    /// SHA-256 of the exact request payload (model + messages + params).
    pub prompt_hash: String,
    pub temperature: f64,
    pub max_tokens: u32,
    pub cache_hit: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u64,
    /// SHA-256 of the response text.
    pub response_hash: String,
    pub status: String,
    pub error: Option<String>,
}

pub struct LedgerWriter {
    path: std::path::PathBuf,
}

impl LedgerWriter {
    pub fn new(path: impl Into<std::path::PathBuf>) -> CoreResult<LedgerWriter> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CoreError::Io)?;
        }
        Ok(LedgerWriter { path })
    }

    /// Append one entry; the id is the 1-based line number.
    pub fn append(&self, mut entry: LedgerEntry) -> CoreResult<u64> {
        let id = self.next_id()?;
        entry.id = id;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(CoreError::Io)?;
        let line = serde_json::to_string(&entry)
            .map_err(|e| CoreError::InvalidData(format!("ledger serialize: {e}")))?;
        writeln!(f, "{line}").map_err(CoreError::Io)?;
        Ok(id)
    }

    fn next_id(&self) -> CoreResult<u64> {
        Ok(self.count_lines() + 1)
    }

    fn count_lines(&self) -> u64 {
        std::fs::read_to_string(&self.path)
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count() as u64)
            .unwrap_or(0)
    }

    /// Read the last `limit` entries (parse failures skipped with count).
    pub fn read_last(&self, limit: usize) -> CoreResult<Vec<(u64, String)>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path).map_err(CoreError::Io)?;
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(limit);
        Ok(lines[start..]
            .iter()
            .enumerate()
            .map(|(i, l)| ((start + i + 1) as u64, l.to_string()))
            .collect())
    }

    pub fn entry_count(&self) -> u64 {
        self.count_lines()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(purpose: &str) -> LedgerEntry {
        LedgerEntry {
            id: 0,
            ts: "2026-09-07T00:00:00Z".into(),
            purpose: purpose.into(),
            provider: "zai".into(),
            model: "glm-5.3-flash".into(),
            prompt_hash: "ph".into(),
            temperature: 0.0,
            max_tokens: 100,
            cache_hit: false,
            prompt_tokens: 10,
            completion_tokens: 20,
            latency_ms: 5,
            response_hash: "rh".into(),
            status: "ok".into(),
            error: None,
        }
    }

    #[test]
    fn append_read_ids_are_monotonic() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("bt_ai_ledger_{}_{}", std::process::id(), seq));
        let _ = std::fs::remove_dir_all(&dir);
        let w = LedgerWriter::new(dir.join("ledger.jsonl")).unwrap();
        let id1 = w.append(entry("compile")).unwrap();
        let id2 = w.append(entry("review")).unwrap();
        assert_eq!((id1, id2), (1, 2));
        assert_eq!(w.entry_count(), 2);
        let last = w.read_last(1).unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].0, 2);
        assert!(last[0].1.contains("\"purpose\":\"review\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entries_never_contain_secrets_fields() {
        // Contract check: the serialization shape must not carry key-ish fields.
        let line = serde_json::to_string(&entry("compile")).unwrap();
        assert!(!line.contains("api_key"));
        assert!(!line.contains("prompt\":")); // no raw prompt body
        assert!(!line.contains("response_text"));
    }
}
